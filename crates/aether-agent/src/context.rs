use std::{fmt::Write as FmtWrite, path::Path};

use aether_core::{
    ActionClassification, BoundedText, CONTEXT_GUIDANCE, CommandEffects, CompactToolSummary,
    ContextSnapshot, FileExcerpt, GitSnapshot, InspectedFile, LineRange,
    MAX_CONTEXT_FINGERPRINT_BYTES, MAX_CONTEXT_FINGERPRINTS, MAX_CONTEXT_ITEMS,
    MAX_CONTEXT_RENDER_BYTES, MAX_EXCERPT_BYTES, MAX_FILE_EXCERPTS, MAX_INSPECTED_FILES,
    MAX_STORED_TOOL_SUMMARIES, MAX_SUMMARY_PATHS, MAX_TASK_BYTES, MAX_USER_MODIFIED_FILES,
    MAX_WORKFLOW_FIELD_BYTES, ObservedFileState, OpaqueContinuation, PreparedAction, ToolResult,
    WorkflowFailure, WorkflowPhase, WorkflowVerification, WorkspacePath, analyze_command,
    compact_tool_result, merge_line_ranges,
};
use serde_json::Value;

use crate::repo_map::RepoMap;
use crate::{
    ContextCandidate, ContextKind, RelationshipHint, SymbolHint, plan_verification,
    select_context_with_relationships,
};

const MAX_REPO_MAP_CONTEXT_BYTES: usize = 8 * 1024;

fn relevant_symbol_paths(snapshot: &ContextSnapshot) -> Vec<&str> {
    let mut paths = Vec::new();
    for path in snapshot
        .inspected
        .iter()
        .map(|file| file.path.as_str())
        .chain(snapshot.modified.iter().map(String::as_str))
        .chain(snapshot.workflow.relevant_files.iter().map(String::as_str))
    {
        if !paths.contains(&path) {
            paths.push(path);
        }
        if paths.len() == crate::symbol_index::MAX_SYMBOL_FILES {
            break;
        }
    }
    paths
}

/// In-memory bounded context engine for one AETHER session.
#[derive(Clone, Debug)]
pub struct ContextEngine {
    snapshot: ContextSnapshot,
    repo_map: Option<RepoMap>,
}

impl ContextEngine {
    /// Create an empty engine for a workspace.
    pub fn new(workspace_root: impl Into<String>, model: Option<String>) -> Self {
        let workspace_root = workspace_root.into();
        let repo_map = (!workspace_root.is_empty()).then(|| RepoMap::new(&workspace_root));
        Self { snapshot: ContextSnapshot::new(workspace_root, model), repo_map }
    }

    /// Restore from persisted snapshot without trusting hashes yet.
    pub fn restore(mut snapshot: ContextSnapshot) -> Self {
        snapshot.inspected.truncate(MAX_INSPECTED_FILES);
        snapshot.excerpts.truncate(MAX_FILE_EXCERPTS);
        snapshot.tool_summaries.truncate(MAX_STORED_TOOL_SUMMARIES);
        snapshot.modified.truncate(MAX_CONTEXT_ITEMS);
        snapshot.user_modified.truncate(MAX_USER_MODIFIED_FILES);
        snapshot.rendered_fingerprints.truncate(MAX_CONTEXT_FINGERPRINTS);
        snapshot
            .user_modified
            .retain(|path| !path.is_empty() && path.len() <= aether_core::MAX_CONTEXT_PATH_BYTES);
        snapshot.rendered_fingerprints.retain(|fingerprint| {
            !fingerprint.is_empty() && fingerprint.len() <= MAX_CONTEXT_FINGERPRINT_BYTES
        });
        snapshot.workflow.enforce_bounds();
        let repo_map =
            (!snapshot.workspace_root.is_empty()).then(|| RepoMap::new(&snapshot.workspace_root));
        Self { snapshot, repo_map }
    }

    /// Return the current bounded snapshot.
    pub fn snapshot(&self) -> &ContextSnapshot {
        &self.snapshot
    }

    /// Borrow the bounded snapshot for local policy state updates.
    pub(crate) fn snapshot_mut(&mut self) -> &mut ContextSnapshot {
        &mut self.snapshot
    }

    /// Consume the engine and return the snapshot.
    pub fn into_snapshot(self) -> ContextSnapshot {
        self.snapshot
    }

    /// Return the lazy repository map associated with this context, if a workspace is known.
    pub fn repository_map(&self) -> Option<&RepoMap> {
        self.repo_map.as_ref()
    }

    /// Record the latest user task.
    pub fn set_task(&mut self, prompt: &str) {
        self.snapshot.current_task = BoundedText::new(prompt, MAX_TASK_BYTES);
    }

    /// Store the latest opaque continuation after sanitizing identity keys.
    pub fn set_continuation(&mut self, continuation: Option<OpaqueContinuation>) {
        self.snapshot.continuation =
            continuation.as_ref().and_then(aether_core::persistable_continuation);
    }

    /// Record a compact git snapshot.
    pub fn set_git(&mut self, git: GitSnapshot) {
        self.record_user_modified_from_status(git.status.as_str());
        self.snapshot.git = Some(git);
    }

    /// Mark that resume or live observation found workspace drift.
    pub fn set_workspace_changed(&mut self, changed: bool) {
        if changed && !self.snapshot.workspace_changed {
            self.reset_workflow_for_workspace_change();
        }
        self.snapshot.workspace_changed = changed;
    }

    /// Update model identity if a later turn selected one.
    pub fn set_model(&mut self, model: Option<String>) {
        if model.is_some() {
            self.snapshot.model = model;
        }
    }

    /// Apply deterministic completion criteria at normal turn termination.
    pub fn finish_turn(&mut self) -> bool {
        if self.snapshot.workspace_changed || self.snapshot.inspected.iter().any(|file| file.stale)
        {
            self.reset_workflow_for_workspace_change();
            return false;
        }
        self.snapshot.workflow.complete_if_ready(&self.snapshot.modified)
    }

    /// Record a bounded dirty-worktree path as user-owned unless this session owns it already.
    pub fn record_user_modified(&mut self, path: &str) {
        if !path_is_workspace_relative(path)
            || self.snapshot.modified.iter().any(|owned| owned == path)
            || self.snapshot.user_modified.iter().any(|existing| existing == path)
        {
            return;
        }
        self.snapshot.user_modified.push(path.to_owned());
        if self.snapshot.user_modified.len() > MAX_USER_MODIFIED_FILES {
            self.snapshot.user_modified.remove(0);
        }
    }

    /// Observe a completed tool call and fold it into bounded working state.
    pub fn observe_tool(&mut self, name: &str, input: &Value, result: &ToolResult) {
        let command_effects = command_effects_for_tool(name, input);
        self.observe_with_effects(name, input, result, command_effects.as_ref(), None);
    }

    fn observe_with_effects(
        &mut self,
        name: &str,
        input: &Value,
        result: &ToolResult,
        command_effects: Option<&CommandEffects>,
        prepared_classification: Option<ActionClassification>,
    ) {
        let focused_verification = prepared_classification.map_or_else(
            || self.is_focused_verification(name, input),
            |classification| {
                (matches!(classification, ActionClassification::Verification)
                    || (matches!(classification, ActionClassification::Read)
                        && matches!(name, "read" | "git")
                        && self.is_focused_verification(name, input)))
                    && matches!(
                        self.snapshot.workflow.verification,
                        WorkflowVerification::Pending | WorkflowVerification::Failed
                    )
            },
        );
        let failure_key = (focused_verification
            || prepared_classification == Some(ActionClassification::Verification)
            || (prepared_classification.is_none() && is_verification_command(input))
            || !result.ok
            || !self.snapshot.workflow.unresolved_failures.is_empty())
        .then(|| workflow_failure_key(name, input));
        let mutation = prepared_classification.map_or_else(
            || is_workspace_mutation(name, input, command_effects),
            |classification| classification == ActionClassification::Mutation,
        );
        let repository_observation =
            result.data.as_ref().is_some_and(|data| data.get("repository_observation").is_some());
        self.snapshot.workflow.progress.note_tool_result();
        if mutation {
            self.snapshot.workflow.begin_modification();
        }
        let summary = compact_tool_result(name, result);
        self.push_summary(summary);
        self.observe_relevant(name, input, result);
        match name {
            "read" => self.observe_read(result),
            "write" => self.observe_write(input, result),
            "patch" => self.observe_patch(result),
            "git" => self.observe_git(result),
            _ => {}
        }
        if repository_observation && name != "read" {
            self.observe_read(result);
            if read_contains_files(result) {
                self.snapshot
                    .workflow
                    .record_inspection_with_provenance(aether_core::EvidenceProvenance::ToolOutput);
            }
        }
        if result.ok {
            if let Some(failure_key) = failure_key.as_deref() {
                self.snapshot.workflow.clear_failure(failure_key);
            }
        } else {
            self.record_workflow_failure(
                failure_key.as_deref().unwrap_or("operation"),
                name,
                result,
            );
        }
        if mutation && mutation_applied(name, input, result, command_effects) {
            if let Some(effects) = command_effects.as_ref() {
                let include_manifests = matches!(
                    effects.class,
                    aether_core::CommandClass::DependencyManagement
                        | aether_core::CommandClass::GitMutation
                );
                let paths = effects.paths.iter().chain(
                    include_manifests.then_some(effects.manifests.iter()).into_iter().flatten(),
                );
                for path in paths {
                    if path_is_workspace_relative(path) {
                        self.record_relevant_path(path);
                        self.record_modified(path);
                    }
                }
                self.invalidate_command_effects(effects);
            }
            self.snapshot.workflow.record_mutation();
            self.refresh_verification_plan();
        }
        if focused_verification {
            let passed = verification_passed(name, result);
            let planned = shell_command_display(input).is_some_and(|command| {
                self.snapshot.workflow.record_verification_command(&command, passed)
            });
            if !planned && self.snapshot.workflow.verification_steps.is_empty() {
                self.snapshot.workflow.record_verification(passed);
            }
            if passed {
                if let Some(failure_key) = failure_key.as_deref() {
                    self.snapshot.workflow.clear_failure(failure_key);
                }
            } else {
                self.snapshot.workflow.record_failure(WorkflowFailure::new(
                    failure_key.as_deref().unwrap_or("operation"),
                    name,
                    "verification_failed",
                    verification_message(result),
                ));
            }
        }
        if name == "read" && result.ok && read_contains_files(result) {
            self.snapshot
                .workflow
                .record_inspection_with_provenance(aether_core::EvidenceProvenance::ToolOutput);
        }
        if result.ok
            && matches!(name, "read" | "find" | "list" | "search" | "git" | "write" | "patch")
            && !self.snapshot.inspected.iter().any(|file| file.stale)
        {
            self.snapshot.workspace_changed = false;
        }
        self.enforce_bounds();
    }

    /// Observe an action whose normalized representation is shared across the turn hot path.
    pub(crate) fn observe_prepared(&mut self, action: &PreparedAction, result: &ToolResult) {
        self.observe_with_effects(
            &action.tool,
            &action.normalized_input,
            result,
            action.command_effects.as_ref(),
            Some(action.classification),
        );
    }

    /// Re-hash inspected files against the live workspace. Stale hashes never win.
    pub fn refresh_against_workspace(&mut self, workspace_root: &Path) -> Vec<String> {
        let mut stale_paths = Vec::new();
        let live_root = workspace_root.display().to_string();
        let root_changed =
            !self.snapshot.workspace_root.is_empty() && self.snapshot.workspace_root != live_root;
        if root_changed {
            self.snapshot.workspace_changed = true;
        }
        self.snapshot.workspace_root = live_root;
        if self.repo_map.as_ref().is_none_or(|map| map.root() != workspace_root) {
            self.repo_map = Some(RepoMap::new(workspace_root));
        }
        let mut invalid_path = false;
        self.snapshot.inspected.retain_mut(|file| {
            if !path_is_workspace_relative(&file.path) {
                invalid_path = true;
                return false;
            }
            match live_file_hash(workspace_root, &file.path) {
                Some(hash) => {
                    if file.content_hash.as_ref().is_some_and(|expected| expected != &hash) {
                        file.stale = true;
                        file.content_hash = Some(hash);
                        stale_paths.push(file.path.clone());
                    } else if file.content_hash.is_none() {
                        file.content_hash = Some(hash);
                        file.stale = false;
                    } else {
                        file.stale = false;
                        file.last_state = ObservedFileState::Present;
                    }
                    true
                }
                None => {
                    if file.last_state == ObservedFileState::Present {
                        file.stale = true;
                        file.last_state = ObservedFileState::Missing;
                        stale_paths.push(file.path.clone());
                    }
                    true
                }
            }
        });
        if invalid_path {
            self.snapshot.workspace_changed = true;
        }
        if !stale_paths.is_empty() {
            self.snapshot.workspace_changed = true;
            self.snapshot
                .excerpts
                .retain(|excerpt| !stale_paths.iter().any(|path| path == &excerpt.path));
        }
        if root_changed || !stale_paths.is_empty() {
            self.reset_workflow_for_workspace_change();
        }
        self.snapshot.modified.retain(|path| path_is_workspace_relative(path));
        stale_paths
    }

    /// Render a token-conscious packet for model reconstruction. Full files are never included.
    pub fn render(&mut self, include_guidance: bool) -> String {
        self.render_internal(include_guidance, false)
    }

    /// Render only context items whose bounded fingerprints changed since the last packet.
    pub fn render_delta(&mut self, include_guidance: bool) -> String {
        self.render_internal(include_guidance, true)
    }

    fn render_internal(&mut self, include_guidance: bool, delta_only: bool) -> String {
        let mut out = String::from("# AETHER workspace context\n");
        if !self.snapshot.workspace_root.is_empty() {
            out.push_str("workspace: ");
            out.push_str(&self.snapshot.workspace_root);
            out.push('\n');
        }
        if !self.snapshot.current_task.as_str().is_empty() {
            out.push_str("task: ");
            out.push_str(self.snapshot.current_task.as_str());
            out.push('\n');
        }
        if self.snapshot.workspace_changed {
            out.push_str(
                "workspace_changed: true; treat persisted hashes and excerpts as stale until re-read\n",
            );
        }
        self.render_workflow(&mut out);
        let repo_metadata = self
            .repo_map
            .as_ref()
            .and_then(|map| map.compact(MAX_REPO_MAP_CONTEXT_BYTES).ok())
            .unwrap_or_default();
        let symbol_paths = relevant_symbol_paths(&self.snapshot);
        let symbol_matches = self
            .repo_map
            .as_ref()
            .and_then(|map| {
                map.lookup_symbols_in_paths(self.snapshot.current_task.as_str(), &symbol_paths, 64)
                    .ok()
            })
            .unwrap_or_default();
        let relationship_matches = self
            .repo_map
            .as_ref()
            .and_then(|map| {
                map.lookup_relationships_in_paths(
                    self.snapshot.current_task.as_str(),
                    &symbol_paths,
                    64,
                )
                .ok()
            })
            .unwrap_or_default();
        let symbol_hint_storage = symbol_matches
            .iter()
            .map(|symbol| (symbol.path.to_string_lossy(), symbol.symbol.name.as_str()))
            .collect::<Vec<_>>();
        let symbol_hints = symbol_hint_storage
            .iter()
            .map(|(path, name)| SymbolHint { path, name })
            .collect::<Vec<_>>();
        let relationship_hint_storage = relationship_matches
            .iter()
            .map(|relationship| {
                (
                    relationship.path.to_string_lossy(),
                    relationship.kind,
                    relationship.score,
                    relationship.ambiguous,
                )
            })
            .collect::<Vec<_>>();
        let relationship_hints = relationship_hint_storage
            .iter()
            .map(|(path, kind, score, ambiguous)| RelationshipHint {
                path,
                kind: *kind,
                score: *score,
                ambiguous: *ambiguous,
            })
            .collect::<Vec<_>>();
        let mut candidates = Vec::with_capacity(
            self.snapshot.inspected.len()
                + self.snapshot.modified.len()
                + self.snapshot.tool_summaries.len()
                + self.snapshot.excerpts.len()
                + 2,
        );
        let mut recency = 0;
        let phase = self.snapshot.workflow.phase;
        let task = self.snapshot.current_task.as_str();
        let previous_fingerprints = &self.snapshot.rendered_fingerprints;
        for file in &self.snapshot.inspected {
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::InspectedFile,
                Some(&file.path),
                file.content_hash.as_deref().unwrap_or(""),
                &file.ranges,
                self.snapshot.modified.iter().any(|path| path == &file.path),
                file.stale,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::InspectedFile,
                path: Some(&file.path),
                content: "",
                start_line: 0,
                end_line: 0,
                recency,
                modified: self.snapshot.modified.iter().any(|path| path == &file.path),
                stale: file.stale,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
            });
            recency += 1;
        }
        for path in &self.snapshot.modified {
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::ModifiedPath,
                Some(path),
                "",
                &[],
                true,
                false,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::ModifiedPath,
                path: Some(path),
                content: "",
                start_line: 0,
                end_line: 0,
                recency,
                modified: true,
                stale: false,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
            });
            recency += 1;
        }
        for path in &self.snapshot.user_modified {
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::UserModifiedPath,
                Some(path),
                "",
                &[],
                false,
                false,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::UserModifiedPath,
                path: Some(path),
                content: "",
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: false,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
            });
            recency += 1;
        }
        for summary in &self.snapshot.tool_summaries {
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::ToolSummary,
                None,
                summary.summary.as_str(),
                &[],
                false,
                false,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::ToolSummary,
                path: None,
                content: summary.summary.as_str(),
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: false,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
            });
            recency += 1;
        }
        for excerpt in &self.snapshot.excerpts {
            let stale = self
                .snapshot
                .inspected
                .iter()
                .find(|file| file.path == excerpt.path)
                .is_some_and(|file| file.stale);
            let modified = self.snapshot.modified.iter().any(|path| path == &excerpt.path);
            let ranges = [LineRange { start: excerpt.start_line, end: excerpt.end_line }];
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::Excerpt,
                Some(&excerpt.path),
                excerpt.text.as_str(),
                &ranges,
                modified,
                stale,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::Excerpt,
                path: Some(&excerpt.path),
                content: excerpt.text.as_str(),
                start_line: excerpt.start_line,
                end_line: excerpt.end_line,
                recency,
                modified,
                stale,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
            });
            recency += 1;
        }
        if let Some(git) = &self.snapshot.git {
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::GitMetadata,
                git.branch.as_deref(),
                git.status.as_str(),
                &[],
                false,
                self.snapshot.workspace_changed,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::GitMetadata,
                path: git.branch.as_deref(),
                content: git.status.as_str(),
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: self.snapshot.workspace_changed,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
            });
            recency += 1;
        }
        if !repo_metadata.is_empty() {
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::RepositoryMetadata,
                None,
                &repo_metadata,
                &[],
                false,
                self.snapshot.workspace_changed,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::RepositoryMetadata,
                path: None,
                content: &repo_metadata,
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: self.snapshot.workspace_changed,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
            });
        }
        let fixed_bytes = out.len() + CONTEXT_GUIDANCE.len() + 32;
        let selection = select_context_with_relationships(
            self.snapshot.current_task.as_str(),
            &candidates,
            MAX_CONTEXT_RENDER_BYTES.saturating_sub(fixed_bytes),
            MAX_CONTEXT_ITEMS,
            &symbol_hints,
            &relationship_hints,
        );
        for item in selection.items.iter().take(MAX_CONTEXT_FINGERPRINTS) {
            if !item.truncated {
                let fingerprint = fingerprint_hex(candidates[item.index].fingerprint);
                if !self.snapshot.rendered_fingerprints.iter().any(|seen| seen == &fingerprint) {
                    self.snapshot.rendered_fingerprints.push(fingerprint);
                }
            }
        }
        if self.snapshot.rendered_fingerprints.len() > MAX_CONTEXT_FINGERPRINTS {
            let excess = self.snapshot.rendered_fingerprints.len() - MAX_CONTEXT_FINGERPRINTS;
            self.snapshot.rendered_fingerprints.drain(..excess);
        }
        if delta_only {
            out.push_str("context_mode: delta\n");
        }
        if !selection.items.is_empty() {
            out.push_str("selected context (relevance-ranked):\n");
        }
        for item in selection.items {
            let candidate = candidates[item.index];
            out.push_str("- ");
            out.push_str(match candidate.kind {
                ContextKind::ModifiedPath => "modified",
                ContextKind::UserModifiedPath => "user-modified",
                ContextKind::Excerpt => "excerpt",
                ContextKind::InspectedFile => "inspected",
                ContextKind::ToolSummary => "tool",
                ContextKind::RepositoryMetadata => "repository",
                ContextKind::GitMetadata => "git",
            });
            if let Some(path) = candidate.path {
                out.push(' ');
                out.push_str(path);
            }
            if candidate.start_line != 0 {
                let _ = write!(out, ":{}-{}", candidate.start_line, candidate.end_line);
            }
            if candidate.kind == ContextKind::InspectedFile
                && let Some(path) = candidate.path
                && let Some(file) = self.snapshot.inspected.iter().find(|file| file.path == path)
                && !file.ranges.is_empty()
            {
                out.push_str(" ranges=");
                push_ranges(&mut out, &file.ranges);
            }
            if candidate.stale {
                out.push_str(" [STALE]");
            } else if matches!(candidate.kind, ContextKind::Excerpt | ContextKind::InspectedFile) {
                out.push_str(" [current]");
            }
            if !candidate.content.is_empty() {
                out.push_str(":\n");
                out.push_str(candidate.content);
            }
            if item.truncated {
                out.push_str("\n[truncated]");
            }
            out.push('\n');
        }
        if include_guidance {
            out.push_str("guidance: ");
            out.push_str(CONTEXT_GUIDANCE);
            out.push('\n');
        }
        if out.len() > MAX_CONTEXT_RENDER_BYTES {
            BoundedText::new(out, MAX_CONTEXT_RENDER_BYTES).into_string()
        } else {
            out
        }
    }

    /// Whether the packet should be attached to the next user turn.
    pub fn should_attach_to_prompt(&self, has_continuation: bool) -> bool {
        if !has_continuation {
            return !self.snapshot.is_empty() || !self.snapshot.workspace_root.is_empty();
        }
        self.snapshot.workspace_changed || self.snapshot.inspected.iter().any(|file| file.stale)
    }

    fn render_workflow(&self, out: &mut String) {
        let workflow = &self.snapshot.workflow;
        out.push_str("workflow: phase=");
        out.push_str(workflow.phase.as_str());
        out.push_str(" verification=");
        out.push_str(workflow.verification.as_str());
        out.push_str(" relevant=");
        let _ = write!(out, "{}", workflow.relevant_files.len());
        out.push_str(" modified=");
        let _ = write!(out, "{}", self.snapshot.modified.len());
        out.push_str(" failures=");
        let _ = write!(out, "{}", workflow.unresolved_failures.len());
        out.push_str(" progress=");
        let _ = write!(out, "{}", workflow.progress.discoveries);
        out.push('/');
        let _ = write!(out, "{}", workflow.progress.inspections);
        out.push('/');
        let _ = write!(out, "{}", workflow.progress.mutations);
        out.push('/');
        let _ = write!(out, "{}", workflow.progress.verifications);
        out.push('\n');
        let decision = &workflow.decision;
        out.push_str("decision: action=");
        out.push_str(decision.next_action.as_str());
        out.push_str(" confidence=");
        let _ = write!(out, "{}", decision.progress_confidence);
        out.push_str(" candidates=");
        let _ = write!(out, "{}", decision.candidate_files.len());
        out.push_str(" questions=");
        let _ = write!(out, "{}", decision.unresolved_questions.len());
        if let Some(target) = &decision.next_target {
            out.push_str(" target=");
            out.push_str(target.as_str());
        }
        out.push('\n');
        if let Some(candidate) = decision.candidate_files.first() {
            out.push_str("decision_candidate: ");
            out.push_str(candidate.path.as_str());
            out.push_str(" score=");
            let _ = write!(out, "{}", candidate.score);
            if !candidate.inspected || candidate.stale {
                out.push_str(" inspect_required");
            }
            out.push('\n');
        }
        if let Some(question) = decision.unresolved_questions.first() {
            out.push_str("decision_question: ");
            out.push_str(question.question.as_str());
            out.push('\n');
        }
        if !decision.next_reason.as_str().is_empty() {
            out.push_str("decision_reason: ");
            out.push_str(decision.next_reason.as_str());
            out.push('\n');
        }
        if let Some(failure) = workflow.unresolved_failures.last() {
            out.push_str("workflow_failure: ");
            out.push_str(failure.tool.as_str());
            out.push(' ');
            out.push_str(failure.code.as_str());
            out.push_str(" — ");
            out.push_str(failure.message.as_str());
            out.push('\n');
        }
        render_workflow_paths(out, "workflow_relevant", &workflow.relevant_files);
        render_workflow_paths(out, "workflow_modified", &self.snapshot.modified);
        render_workflow_paths(out, "workflow_user_modified", &self.snapshot.user_modified);
        for step in workflow
            .verification_steps
            .iter()
            .filter(|step| step.revision == workflow.workspace_revision)
        {
            out.push_str("workflow_verify: ");
            out.push_str(step.status.as_str());
            out.push(' ');
            out.push_str(step.command.as_str());
            out.push_str(" scope=");
            out.push_str(step.scope.as_str());
            out.push('\n');
        }
        let action = match workflow.phase {
            WorkflowPhase::Discover => "identify relevant files before editing",
            WorkflowPhase::Inspect => "inspect identified files before editing",
            WorkflowPhase::Modify => "retry or complete the pending mutation",
            WorkflowPhase::Verify => "run focused verification for modified files",
            WorkflowPhase::Complete => "workflow complete",
        };
        out.push_str("workflow_action: ");
        out.push_str(action);
        out.push('\n');
    }

    fn reset_workflow_for_workspace_change(&mut self) {
        self.snapshot.workflow.reset_for_workspace_change();
        if !self.snapshot.modified.is_empty() {
            self.snapshot.workflow.verification = WorkflowVerification::Pending;
        }
    }

    fn refresh_verification_plan(&mut self) {
        let Some(repo_map) = &self.repo_map else { return };
        let Ok(snapshot) = repo_map.snapshot() else { return };
        let plan = plan_verification(&snapshot, &self.snapshot.modified);
        let revision = self.snapshot.workflow.workspace_revision;
        self.snapshot.workflow.set_verification_plan(
            plan.commands.iter().map(|command| command.workflow_step(revision)),
        );
    }

    fn invalidate_command_effects(&mut self, effects: &CommandEffects) {
        let has_affected = !effects.paths.is_empty() || !effects.manifests.is_empty();
        if effects.uncertain || !has_affected {
            for file in &mut self.snapshot.inspected {
                file.stale = true;
            }
            self.snapshot.excerpts.clear();
        } else {
            for file in &mut self.snapshot.inspected {
                if effects
                    .paths
                    .iter()
                    .chain(effects.manifests.iter())
                    .any(|path| paths_overlap(&file.path, path))
                {
                    file.stale = true;
                }
            }
            self.snapshot.excerpts.retain(|excerpt| {
                !effects
                    .paths
                    .iter()
                    .chain(effects.manifests.iter())
                    .any(|path| excerpt.path == path.as_str())
            });
        }
        self.snapshot.workspace_changed = true;
        if let Some(repo_map) = &self.repo_map {
            repo_map.invalidate();
        }
    }

    fn observe_relevant(&mut self, name: &str, input: &Value, result: &ToolResult) {
        if !result.ok {
            return;
        }
        let Some(data) = result.data.as_ref() else {
            if name == "write"
                && let Some(path) = input.get("path").and_then(Value::as_str)
            {
                self.record_relevant_path(path);
            }
            return;
        };
        match name {
            "list" => {
                if let Some(entries) = data.get("entries").and_then(Value::as_array) {
                    for entry in entries {
                        if entry.get("file_type").and_then(Value::as_str) == Some("file")
                            && let Some(path) = entry.get("path").and_then(Value::as_str)
                        {
                            self.record_relevant_path(path);
                        }
                    }
                }
            }
            "find" => {
                if let Some(paths) = data.get("paths").and_then(Value::as_array) {
                    for path in paths.iter().filter_map(Value::as_str) {
                        self.record_relevant_path(path);
                    }
                }
            }
            "search" => {
                if let Some(matches) = data.get("matches").and_then(Value::as_array) {
                    for path in matches.iter().filter_map(|item| item.get("path")) {
                        if let Some(path) = path.as_str() {
                            self.record_relevant_path(path);
                        }
                    }
                }
            }
            "read" => {
                if let Some(files) = data.get("files").and_then(Value::as_array) {
                    for path in files.iter().filter_map(|file| file.get("path")) {
                        if let Some(path) = path.as_str() {
                            self.record_relevant_path(path);
                        }
                    }
                }
            }
            "write" => {
                if let Some(path) = data.get("path").and_then(Value::as_str) {
                    self.record_relevant_path(path);
                }
            }
            "patch" => {
                if let Some(files) = data.get("files").and_then(Value::as_array) {
                    for path in files.iter().filter_map(|file| file.get("path")) {
                        if let Some(path) = path.as_str() {
                            self.record_relevant_path(path);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn record_relevant_path(&mut self, path: &str) {
        if path_is_workspace_relative(path) {
            self.snapshot.workflow.record_relevant_file_with_provenance(
                aether_core::EvidenceProvenance::ToolOutput,
                path,
            );
        }
    }

    fn record_workflow_failure(&mut self, key: &str, name: &str, result: &ToolResult) {
        let code = result.error.as_ref().map_or("tool_failed", |error| error.code.as_str());
        self.snapshot.workflow.record_failure(WorkflowFailure::new(
            key,
            name,
            code,
            result.error.as_ref().map_or(result.output.as_str(), |error| error.message.as_str()),
        ));
    }

    fn is_focused_verification(&self, name: &str, input: &Value) -> bool {
        if !matches!(
            self.snapshot.workflow.verification,
            WorkflowVerification::Pending | WorkflowVerification::Failed
        ) {
            return false;
        }
        match name {
            "read" => read_targets_modified(input, &self.snapshot.modified),
            "git" => {
                matches!(input.get("operation").and_then(Value::as_str), Some("status" | "diff"))
            }
            "shell" => is_verification_command(input),
            _ => false,
        }
    }

    fn observe_read(&mut self, result: &ToolResult) {
        let Some(files) = result.data.as_ref().and_then(|data| data.get("files")?.as_array())
        else {
            return;
        };
        for file in files {
            let Some(path) = file.get("path").and_then(Value::as_str) else {
                continue;
            };
            if !path_is_workspace_relative(path) {
                continue;
            }
            let hash = file.get("content_hash").and_then(Value::as_str).map(str::to_owned);
            let lines = file.get("lines").and_then(Value::as_array);
            let range = lines.and_then(|lines| line_range_from_read(lines));
            self.upsert_inspected(path, hash.clone(), range, ObservedFileState::Present);
            if let (Some(range), Some(lines)) = (range, lines)
                && let Some(text) = excerpt_from_lines(lines)
            {
                self.push_excerpt(FileExcerpt {
                    path: path.to_owned(),
                    start_line: range.start,
                    end_line: range.end,
                    text: BoundedText::new(text, MAX_EXCERPT_BYTES),
                    content_hash: hash,
                });
            }
        }
    }

    fn observe_write(&mut self, input: &Value, result: &ToolResult) {
        let path = result
            .data
            .as_ref()
            .and_then(|data| data.get("path").and_then(Value::as_str))
            .or_else(|| input.get("path").and_then(Value::as_str));
        let Some(path) = path else {
            return;
        };
        if !path_is_workspace_relative(path) {
            return;
        }
        let hash = result.data.as_ref().and_then(|data| data.get("hash").and_then(Value::as_str));
        if result.ok {
            self.upsert_inspected(path, hash.map(str::to_owned), None, ObservedFileState::Present);
            self.record_modified(path);
            self.snapshot.excerpts.retain(|excerpt| excerpt.path != path);
        }
    }

    fn observe_patch(&mut self, result: &ToolResult) {
        let Some(files) = result.data.as_ref().and_then(|data| data.get("files")?.as_array())
        else {
            return;
        };
        if result.data.as_ref().and_then(|data| data.get("dry_run")?.as_bool()).unwrap_or(false) {
            return;
        }
        for file in files {
            let Some(path) = file.get("path").and_then(Value::as_str) else {
                continue;
            };
            if !path_is_workspace_relative(path) {
                continue;
            }
            let hash = file.get("new_hash").and_then(Value::as_str).map(str::to_owned);
            if result.ok {
                self.upsert_inspected(path, hash, None, ObservedFileState::Present);
                self.record_modified(path);
                self.snapshot.excerpts.retain(|excerpt| excerpt.path != path);
            }
        }
    }

    fn observe_git(&mut self, result: &ToolResult) {
        let Some(data) = result.data.as_ref() else {
            return;
        };
        let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("");
        let branch = stdout.lines().find_map(|line| {
            line.strip_prefix("## ")
                .map(|rest| rest.split([' ', '.']).next().unwrap_or(rest).to_owned())
        });
        self.snapshot.git = Some(GitSnapshot {
            branch,
            status: BoundedText::new(stdout, aether_core::MAX_GIT_SNAPSHOT_BYTES),
            available: result.ok,
        });
        self.record_user_modified_from_status(stdout);
    }

    fn record_user_modified_from_status(&mut self, status: &str) {
        for path in dirty_paths(status).into_iter().take(MAX_USER_MODIFIED_FILES) {
            self.record_user_modified(&path);
        }
    }

    fn upsert_inspected(
        &mut self,
        path: &str,
        hash: Option<String>,
        range: Option<LineRange>,
        last_state: ObservedFileState,
    ) {
        if let Some(existing) = self.snapshot.inspected.iter_mut().find(|file| file.path == path) {
            let hash_changed = hash.as_ref().is_some_and(|new_hash| {
                existing.content_hash.as_ref().is_some_and(|old| old != new_hash)
            });
            if let Some(hash) = hash.as_deref() {
                existing.content_hash = Some(hash.to_owned());
            }
            existing.last_state = last_state;
            existing.stale = false;
            if let Some(range) = range {
                existing.ranges.push(range);
                merge_line_ranges(&mut existing.ranges);
            }
            if hash_changed {
                self.snapshot.excerpts.retain(|excerpt| excerpt.path != path);
            }
            return;
        }
        let mut ranges = Vec::new();
        if let Some(range) = range {
            ranges.push(range);
        }
        self.snapshot.inspected.push(InspectedFile {
            path: path.to_owned(),
            content_hash: hash,
            ranges,
            last_state,
            stale: false,
        });
        if self.snapshot.inspected.len() > MAX_INSPECTED_FILES {
            self.snapshot.inspected.remove(0);
        }
    }

    fn record_modified(&mut self, path: &str) {
        if !self.snapshot.modified.iter().any(|existing| existing == path) {
            self.snapshot.modified.push(path.to_owned());
        }
        if self.snapshot.modified.len() > MAX_CONTEXT_ITEMS {
            self.snapshot.modified.remove(0);
        }
    }

    fn push_summary(&mut self, summary: CompactToolSummary) {
        self.snapshot.tool_summaries.push(summary);
        if self.snapshot.tool_summaries.len() > MAX_STORED_TOOL_SUMMARIES {
            self.snapshot.tool_summaries.remove(0);
        }
    }

    fn push_excerpt(&mut self, excerpt: FileExcerpt) {
        if self.snapshot.excerpts.iter().any(|existing| {
            existing.path == excerpt.path
                && existing.start_line == excerpt.start_line
                && existing.end_line == excerpt.end_line
        }) {
            self.snapshot.excerpts.retain(|existing| {
                !(existing.path == excerpt.path
                    && existing.start_line == excerpt.start_line
                    && existing.end_line == excerpt.end_line)
            });
        }
        self.snapshot.excerpts.push(excerpt);
        if self.snapshot.excerpts.len() > MAX_FILE_EXCERPTS {
            self.snapshot.excerpts.remove(0);
        }
    }

    fn enforce_bounds(&mut self) {
        self.snapshot.inspected.truncate(MAX_INSPECTED_FILES);
        self.snapshot.excerpts.truncate(MAX_FILE_EXCERPTS);
        self.snapshot.tool_summaries.truncate(MAX_STORED_TOOL_SUMMARIES);
        self.snapshot.modified.truncate(MAX_CONTEXT_ITEMS);
        self.snapshot.workflow.enforce_bounds();
    }
}

fn path_is_workspace_relative(path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
}

fn paths_overlap(left: &str, right: &str) -> bool {
    left == right || is_path_parent(left, right) || is_path_parent(right, left)
}

fn is_path_parent(parent: &str, child: &str) -> bool {
    let parent = parent.trim_end_matches('/');
    child.strip_prefix(parent).is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_workspace_mutation(name: &str, _input: &Value, effects: Option<&CommandEffects>) -> bool {
    matches!(name, "write" | "patch")
        || matches!(name, "shell" | "process")
            && effects.is_some_and(|effects| effects.uncertain || effects.class.is_mutation())
}

fn mutation_applied(
    name: &str,
    input: &Value,
    result: &ToolResult,
    effects: Option<&CommandEffects>,
) -> bool {
    if !result.ok || !is_workspace_mutation(name, input, effects) {
        return false;
    }
    if name == "patch"
        && result
            .data
            .as_ref()
            .and_then(|data| data.get("dry_run"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return false;
    }
    name == "shell"
        || name == "process"
        || name == "write"
        || result
            .data
            .as_ref()
            .and_then(|data| data.get("files"))
            .and_then(Value::as_array)
            .is_some_and(|files| !files.is_empty())
        || input.get("files").and_then(Value::as_array).is_some_and(|files| !files.is_empty())
}

fn read_contains_files(result: &ToolResult) -> bool {
    result
        .data
        .as_ref()
        .and_then(|data| data.get("files"))
        .and_then(Value::as_array)
        .is_some_and(|files| !files.is_empty())
}

fn read_targets_modified(input: &Value, modified: &[String]) -> bool {
    input.get("files").and_then(Value::as_array).is_some_and(|files| {
        files
            .iter()
            .filter_map(|file| file.get("path").and_then(Value::as_str))
            .any(|path| modified.iter().any(|modified| modified == path))
    })
}

fn is_verification_command(input: &Value) -> bool {
    command_effects_for_input(input).is_some_and(|effects| effects.class.is_verification())
}

fn command_effects_for_tool(name: &str, input: &Value) -> Option<CommandEffects> {
    if !matches!(name, "shell" | "process") {
        return None;
    }
    if name == "process"
        && input
            .get("operation")
            .and_then(Value::as_str)
            .is_some_and(|operation| operation != "start")
    {
        return None;
    }
    command_effects_for_input(input)
}

fn command_effects_for_input(input: &Value) -> Option<CommandEffects> {
    let program = input.get("program")?.as_str()?;
    let args = match input.get("args") {
        None => Vec::new(),
        Some(value) => value
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    Some(analyze_command(program, &args, input.get("cwd").and_then(Value::as_str).unwrap_or("")))
}

fn shell_command_display(input: &Value) -> Option<String> {
    let program = input.get("program")?.as_str()?;
    let args = input
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(Value::as_str).map(str::to_owned).collect())
        .unwrap_or_default();
    Some(
        crate::PlannedCommand {
            program: program.to_owned(),
            args,
            cwd: input.get("cwd").and_then(Value::as_str).unwrap_or("").to_owned(),
            scope: String::new(),
        }
        .workflow_key(),
    )
}

fn verification_passed(name: &str, result: &ToolResult) -> bool {
    if !result.ok {
        return false;
    }
    if name == "read" {
        return read_contains_files(result);
    }
    if matches!(name, "shell" | "git") {
        return result
            .data
            .as_ref()
            .and_then(|data| data.get("success"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
    }
    true
}

fn verification_message(result: &ToolResult) -> &str {
    result.error.as_ref().map_or(result.output.as_str(), |error| error.message.as_str())
}

fn workflow_failure_key(name: &str, input: &Value) -> String {
    let target = input
        .get("path")
        .and_then(Value::as_str)
        .or_else(|| input.get("program").and_then(Value::as_str))
        .or_else(|| input.get("operation").and_then(Value::as_str))
        .or_else(|| {
            input
                .get("files")
                .and_then(Value::as_array)
                .and_then(|files| files.first())
                .and_then(|file| file.get("path"))
                .and_then(Value::as_str)
        })
        .unwrap_or("operation");
    let mut key = String::with_capacity(MAX_WORKFLOW_FIELD_BYTES);
    let bounded_name = BoundedText::new(name, MAX_WORKFLOW_FIELD_BYTES.saturating_sub(1));
    key.push_str(bounded_name.as_str());
    if key.len() < MAX_WORKFLOW_FIELD_BYTES {
        key.push(':');
    }
    let remaining = MAX_WORKFLOW_FIELD_BYTES.saturating_sub(key.len());
    key.push_str(BoundedText::new(target, remaining).as_str());
    key
}

fn render_workflow_paths(out: &mut String, label: &str, paths: &[String]) {
    let start = paths.len().saturating_sub(MAX_SUMMARY_PATHS);
    if start == paths.len() {
        return;
    }
    out.push_str(label);
    out.push_str(": ");
    for (index, path) in paths[start..].iter().enumerate() {
        if index != 0 {
            out.push_str(", ");
        }
        push_bounded_workflow_path(out, path);
    }
    out.push('\n');
}

fn push_bounded_workflow_path(out: &mut String, path: &str) {
    let mut end = path.len().min(MAX_WORKFLOW_FIELD_BYTES);
    while end > 0 && !path.is_char_boundary(end) {
        end -= 1;
    }
    out.push_str(&path[..end]);
}

fn push_ranges(out: &mut String, ranges: &[LineRange]) {
    for (index, range) in ranges.iter().enumerate() {
        if index != 0 {
            out.push(',');
        }
        let _ = write!(out, "{}-{}", range.start, range.end);
    }
}

fn line_range_from_read(lines: &[Value]) -> Option<LineRange> {
    let mut start = None;
    let mut end = None;
    for line in lines {
        if let Some(number) = line.get("number").and_then(Value::as_u64) {
            let number = number as usize;
            start = Some(start.map_or(number, |value: usize| value.min(number)));
            end = Some(end.map_or(number, |value: usize| value.max(number)));
        }
    }
    Some(LineRange { start: start?, end: end? })
}

fn excerpt_from_lines(lines: &[Value]) -> Option<String> {
    let mut text = String::new();
    for line in lines {
        let line_text = line.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(line_text);
        if text.len() >= MAX_EXCERPT_BYTES {
            break;
        }
    }
    if text.is_empty() { None } else { Some(text) }
}

fn live_file_hash(root: &Path, relative: &str) -> Option<String> {
    let relative = WorkspacePath::new(relative).ok()?;
    let path = root.join(relative.as_path());
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() > 4 * 1024 * 1024 {
        return None;
    }
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// Capture a compact git status snapshot without exposing a new model-visible tool.
pub fn capture_git_snapshot(workspace_root: &Path) -> GitSnapshot {
    let status = std::process::Command::new("git")
        .args(["status", "--short", "--branch"])
        .current_dir(workspace_root)
        .output();
    match status {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let branch = stdout.lines().next().and_then(|line| {
                line.strip_prefix("## ").map(|rest| {
                    rest.split([' ', '.']).next().unwrap_or(rest).trim_start_matches('*').to_owned()
                })
            });
            GitSnapshot {
                branch,
                status: BoundedText::new(stdout, aether_core::MAX_GIT_SNAPSHOT_BYTES),
                available: true,
            }
        }
        _ => GitSnapshot {
            branch: None,
            status: BoundedText::new("git unavailable", aether_core::MAX_GIT_SNAPSHOT_BYTES),
            available: false,
        },
    }
}

fn dirty_paths(status: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in status.lines().skip_while(|line| line.starts_with("## ")) {
        if line.starts_with("## ") || line.len() < 3 {
            continue;
        }
        let value = line[2..].trim();
        let path = value.rsplit_once(" -> ").map_or(value, |(_, current)| current).trim();
        if path_is_workspace_relative(path)
            && !paths.iter().any(|existing| existing == path)
            && paths.len() < MAX_USER_MODIFIED_FILES
        {
            paths.push(path.to_owned());
        }
    }
    paths
}

fn candidate_fingerprint(
    task: &str,
    kind: ContextKind,
    path: Option<&str>,
    content: &str,
    ranges: &[LineRange],
    modified: bool,
    stale: bool,
) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(task.as_bytes());
    hasher.update(&[kind as u8, u8::from(modified), u8::from(stale)]);
    for range in ranges {
        hasher.update(&range.start.to_le_bytes());
        hasher.update(&range.end.to_le_bytes());
    }
    if let Some(path) = path {
        hasher.update(path.as_bytes());
    }
    hasher.update(content.as_bytes());
    let hash = hasher.finalize();
    let mut fingerprint = [0; 16];
    fingerprint.copy_from_slice(&hash.as_bytes()[..16]);
    fingerprint
}

fn fingerprint_hex(fingerprint: [u8; 16]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(fingerprint.len() * 2);
    for byte in fingerprint {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::{ToolCallId, WorkflowPhase, WorkflowVerification};

    fn success(data: Value) -> ToolResult {
        ToolResult::success_json(ToolCallId::new("ctx-1").unwrap(), data, 64 * 1024)
    }

    #[test]
    fn context_deduplicates_inspected_paths_and_ranges() {
        let mut engine = ContextEngine::new("/workspace", None);
        let result = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": "aaa",
                "truncated": false,
                "lines": [
                    {"number": 1, "text": "mod a;"},
                    {"number": 2, "text": "mod b;"}
                ]
            }]
        }));
        engine.observe_tool("read", &Value::Null, &result);
        engine.observe_tool("read", &Value::Null, &result);
        assert_eq!(engine.snapshot().inspected.len(), 1);
        assert_eq!(engine.snapshot().inspected[0].ranges, vec![LineRange { start: 1, end: 2 }]);
        assert_eq!(engine.snapshot().excerpts.len(), 1);
    }

    #[test]
    fn direct_command_mutation_invalidates_targeted_context_and_verification() {
        let mut engine = ContextEngine::new("/workspace", None);
        let read = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": "before",
                "lines": [{"number": 1, "text": "fn main() {}"}]
            }]
        }));
        engine.observe_tool("read", &serde_json::json!({"files": [{"path": "src/lib.rs"}]}), &read);
        let formatted = success(serde_json::json!({"success": true}));
        engine.observe_tool(
            "shell",
            &serde_json::json!({
                "program": "cargo",
                "args": ["fmt", "--", "src/lib.rs"]
            }),
            &formatted,
        );
        assert!(engine.snapshot().inspected[0].stale);
        assert!(engine.snapshot().excerpts.is_empty());
        assert!(engine.snapshot().modified.iter().any(|path| path == "src/lib.rs"));
        assert_eq!(engine.snapshot().workflow.workspace_revision, 1);
    }

    #[test]
    fn context_bounds_drop_oldest_inspected_files() {
        let mut engine = ContextEngine::new("/workspace", None);
        for index in 0..(MAX_INSPECTED_FILES + 5) {
            let result = success(serde_json::json!({
                "path": format!("file-{index}.txt"),
                "bytes": 1,
                "hash": format!("{index:064}"),
                "replaced": false
            }));
            engine.observe_tool(
                "write",
                &serde_json::json!({"path": format!("file-{index}.txt")}),
                &result,
            );
        }
        assert_eq!(engine.snapshot().inspected.len(), MAX_INSPECTED_FILES);
        assert_eq!(engine.snapshot().modified.len(), MAX_CONTEXT_ITEMS);
        assert!(!engine.snapshot().inspected.iter().any(|file| file.path == "file-0.txt"));
    }

    #[test]
    fn changed_file_invalidation_marks_stale_and_drops_excerpts() {
        let root = std::env::temp_dir().join(format!(
            "aether-context-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("src.rs");
        std::fs::write(&path, "old").unwrap();
        let hash = blake3::hash(b"old").to_hex().to_string();
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        let result = success(serde_json::json!({
            "files": [{
                "path": "src.rs",
                "content_hash": hash,
                "truncated": false,
                "lines": [{"number": 1, "text": "old"}]
            }]
        }));
        engine.observe_tool("read", &Value::Null, &result);
        assert_eq!(engine.snapshot().excerpts.len(), 1);
        std::fs::write(&path, "new").unwrap();
        let stale = engine.refresh_against_workspace(&root);
        assert_eq!(stale, vec!["src.rs".to_owned()]);
        assert!(engine.snapshot().inspected[0].stale);
        assert!(engine.snapshot().excerpts.is_empty());
        assert!(engine.snapshot().workspace_changed);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn workflow_follows_discover_inspect_modify_verify_complete() {
        let mut engine = ContextEngine::new("/workspace", None);
        let discovered = success(serde_json::json!({
            "paths": ["src/lib.rs"],
            "truncated": false
        }));
        engine.observe_tool("find", &Value::Null, &discovered);
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Inspect);

        let read_input = serde_json::json!({
            "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 2}]
        });
        let read = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": "before",
                "lines": [{"number": 1, "text": "fn main() {}"}]
            }]
        }));
        engine.observe_tool("read", &read_input, &read);
        assert_eq!(engine.snapshot().workflow.progress.inspections, 1);

        let write = success(serde_json::json!({
            "path": "src/lib.rs",
            "hash": "after"
        }));
        engine.observe_tool("write", &serde_json::json!({"path": "src/lib.rs"}), &write);
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Verify);
        assert_eq!(engine.snapshot().workflow.verification, WorkflowVerification::Pending);
        assert_eq!(engine.snapshot().modified, vec!["src/lib.rs"]);

        let verified = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": "after",
                "lines": [{"number": 1, "text": "fn main() { println!(\"ok\"); }"}]
            }]
        }));
        engine.observe_tool("read", &read_input, &verified);
        assert_eq!(engine.snapshot().workflow.verification, WorkflowVerification::Passed);
        assert!(engine.finish_turn());
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Complete);
    }

    #[test]
    fn workflow_read_only_completion_and_failed_verification_retry() {
        let mut read_only = ContextEngine::new("/workspace", None);
        let read_input = serde_json::json!({
            "files": [{"path": "README.md", "start_line": 1, "end_line": 1}]
        });
        let read = success(serde_json::json!({
            "files": [{
                "path": "README.md",
                "content_hash": "readme",
                "lines": [{"number": 1, "text": "AETHER"}]
            }]
        }));
        read_only.observe_tool("read", &read_input, &read);
        assert!(read_only.finish_turn());
        assert_eq!(read_only.snapshot().workflow.phase, WorkflowPhase::Complete);

        let mut coding = ContextEngine::new("/workspace", None);
        coding.observe_tool("read", &read_input, &read);
        let write = success(serde_json::json!({"path": "README.md", "hash": "changed"}));
        coding.observe_tool("write", &serde_json::json!({"path": "README.md"}), &write);
        let failed = ToolResult::failure(
            ToolCallId::new("ctx-failed-verify").unwrap(),
            "io",
            "verification read failed",
            true,
            1024,
        );
        coding.observe_tool("read", &read_input, &failed);
        assert_eq!(coding.snapshot().workflow.verification, WorkflowVerification::Failed);
        assert!(!coding.finish_turn());
        coding.observe_tool("read", &read_input, &read);
        assert_eq!(coding.snapshot().workflow.verification, WorkflowVerification::Passed);
        assert!(coding.snapshot().workflow.unresolved_failures.is_empty());
        assert!(coding.finish_turn());
    }

    #[test]
    fn workflow_failed_mutation_stays_blocked_until_same_target_retries() {
        let mut engine = ContextEngine::new("/workspace", None);
        let input = serde_json::json!({"path": "src/lib.rs"});
        let failed = ToolResult::failure(
            ToolCallId::new("ctx-failed-mutation").unwrap(),
            "precondition",
            "expected hash did not match",
            true,
            1024,
        );
        engine.observe_tool("write", &input, &failed);
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Modify);
        assert!(engine.snapshot().workflow.has_unresolved_failures());

        let read = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": "current",
                "lines": [{"number": 1, "text": "old"}]
            }]
        }));
        engine.observe_tool("read", &serde_json::json!({"files": [{"path": "src/lib.rs"}]}), &read);
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Inspect);
        assert!(engine.snapshot().workflow.has_unresolved_failures());

        let write = success(serde_json::json!({"path": "src/lib.rs", "hash": "new"}));
        engine.observe_tool("write", &input, &write);
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Verify);
        assert!(engine.snapshot().workflow.unresolved_failures.is_empty());
    }

    #[test]
    fn workflow_context_is_compact_and_workspace_drift_reopens_discovery() {
        let mut engine = ContextEngine::new("/workspace", None);
        engine.observe_tool(
            "find",
            &Value::Null,
            &success(serde_json::json!({"paths": ["src/lib.rs"]})),
        );
        let rendered = engine.render(false);
        assert!(rendered.contains("workflow: phase=inspect"));
        assert!(rendered.contains("workflow_relevant: src/lib.rs"));
        assert!(rendered.contains("workflow_action: inspect identified files before editing"));
        assert!(!rendered.contains("raw transcript"));

        engine.set_workspace_changed(true);
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Discover);
        assert!(engine.snapshot().workflow.relevant_files.is_empty());
    }

    #[test]
    fn stale_context_blocks_completion_until_current_observation() {
        let mut engine = ContextEngine::new("/workspace", None);
        let input = serde_json::json!({
            "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
        });
        let read = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": "current",
                "lines": [{"number": 1, "text": "fn main() {}"}]
            }]
        }));
        engine.observe_tool("read", &input, &read);
        assert!(engine.finish_turn());

        engine.set_workspace_changed(true);
        assert!(!engine.finish_turn());
        engine.observe_tool("read", &input, &read);
        assert!(!engine.snapshot().workspace_changed);
        assert!(engine.finish_turn());
    }

    #[test]
    fn delta_render_omits_unchanged_excerpts_but_keeps_new_observations() {
        let mut engine = ContextEngine::new("/workspace", None);
        engine.set_task("inspect main");
        let first = success(serde_json::json!({
            "files": [{
                "path": "src/main.rs",
                "content_hash": "before",
                "lines": [{"number": 1, "text": "fn main() {}"}]
            }]
        }));
        engine.observe_tool("read", &Value::Null, &first);
        let full = engine.render(false);
        assert!(full.contains("fn main() {}"));

        let unchanged = engine.render_delta(false);
        assert!(unchanged.contains("context_mode: delta"));
        assert!(!unchanged.contains("fn main() {}"));

        let changed = success(serde_json::json!({
            "files": [{
                "path": "src/main.rs",
                "content_hash": "after",
                "lines": [{"number": 1, "text": "fn main() { println!(\"changed\"); }"}]
            }]
        }));
        engine.observe_tool("read", &Value::Null, &changed);
        let delta = engine.render_delta(false);
        assert!(delta.contains("changed"));
        let unchanged_again = engine.render_delta(false);
        assert!(!unchanged_again.contains("changed"));
    }

    #[test]
    fn git_status_records_dirty_paths_as_user_owned() {
        let mut engine = ContextEngine::new("/workspace", None);
        engine.set_git(GitSnapshot {
            branch: Some("main".to_owned()),
            status: BoundedText::new(
                "## main\n M src/lib.rs\n?? src/new.rs\nR  old.rs -> src/renamed.rs",
                aether_core::MAX_GIT_SNAPSHOT_BYTES,
            ),
            available: true,
        });
        assert_eq!(engine.snapshot().user_modified, ["src/lib.rs", "src/new.rs", "src/renamed.rs"]);
    }
}
