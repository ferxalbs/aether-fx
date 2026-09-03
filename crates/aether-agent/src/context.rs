use std::{fmt::Write as FmtWrite, fs, io::Read, path::Path};

use aether_core::{
    ActionClassification, BoundedText, CONTEXT_GUIDANCE, CommandEffects, CompactToolSummary,
    ContextSnapshot, FileExcerpt, GitSnapshot, InspectedFile, LineRange,
    MAX_CONTEXT_FINGERPRINT_BYTES, MAX_CONTEXT_FINGERPRINTS, MAX_CONTEXT_ITEMS,
    MAX_CONTEXT_RENDER_BYTES, MAX_EXCERPT_BYTES, MAX_FILE_EXCERPTS, MAX_INSPECTED_FILES,
    MAX_STORED_TOOL_SUMMARIES, MAX_SUMMARY_PATHS, MAX_TASK_BYTES, MAX_USER_MODIFIED_FILES,
    MAX_WORKFLOW_FIELD_BYTES, ObservedFileState, OpaqueContinuation, PreparedAction, ToolResult,
    WorkflowDiagnostic, WorkflowFailure, WorkflowFailureCategory, WorkflowPhase,
    WorkflowVerification, WorkspacePath, analyze_command, compact_tool_result, extract_diagnostics,
    merge_line_ranges,
};
use serde_json::Value;

use crate::repo_map::RepoMap;
use crate::working_set::WorkingSet;
use crate::{
    ContextCandidate, ContextKind, RelationshipHint, SymbolHint, plan_verification,
    select_context_with_relationships,
};

const MAX_REPO_MAP_CONTEXT_BYTES: usize = 8 * 1024;
const MAX_AUTOMATIC_EVIDENCE_FILES: usize = 8;
const MAX_AUTOMATIC_SOURCE_BYTES: usize = 4 * 1024 * 1024;
const MAX_AUTOMATIC_SOURCE_LINES: usize = 96;
const AUTOMATIC_DIAGNOSTIC_LINES_BEFORE: usize = 4;
const AUTOMATIC_DIAGNOSTIC_LINES_AFTER: usize = 8;
const MAX_EVIDENCE_UPDATE_BYTES: usize = 128;

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
        let stale_evidence =
            snapshot.workspace_changed || snapshot.inspected.iter().any(|file| file.stale);
        let modified = snapshot.modified.clone();
        snapshot.workflow.sync_director(&modified, stale_evidence);
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

    /// Return the bounded operational memory projection for the active repository task.
    #[must_use]
    pub fn working_set(&self) -> WorkingSet {
        WorkingSet::from_snapshot(&self.snapshot)
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
        let before = self.snapshot.workflow.work.task_revision;
        self.snapshot.current_task = BoundedText::new(prompt, MAX_TASK_BYTES);
        self.snapshot.workflow.work.initialize_from_prompt(prompt);
        let workflow = &mut self.snapshot.workflow;
        workflow.task_revision = workflow.work.task_revision;
        if workflow.task_revision != before {
            workflow.completed_task_revision = None;
            workflow.phase = WorkflowPhase::Discover;
            workflow.recovery.clear();
        }
        let modified = self.snapshot.modified.clone();
        workflow.sync_director(&modified, false);
    }

    /// Apply a bounded user steering update without discarding still-useful repository evidence.
    pub fn set_constraints(&mut self, constraints: &[String]) {
        let prompt = self.snapshot.current_task.as_str().to_owned();
        let before = self.snapshot.workflow.work.task_revision;
        self.snapshot.workflow.work.apply_task_update(&prompt, Some(constraints));
        let workflow = &mut self.snapshot.workflow;
        workflow.task_revision = workflow.work.task_revision;
        if workflow.task_revision != before {
            workflow.completed_task_revision = None;
            workflow.phase = WorkflowPhase::Discover;
            workflow.recovery.clear();
        }
        let modified = self.snapshot.modified.clone();
        workflow.sync_director(&modified, false);
    }

    /// Apply a bounded model work outline. The core work state accepts only intent; runtime
    /// observations remain the sole authority for completion, evidence, and blockers.
    pub(crate) fn apply_model_work_update(&mut self, update: &Value) -> bool {
        self.snapshot.workflow.work.apply_model_update(update)
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
        let user_changes_preserved = !self.snapshot.user_modified.iter().any(|path| {
            self.snapshot.modified.iter().any(|modified| paths_overlap(path, modified))
        });
        let ready = self.snapshot.workflow.complete_if_ready_with_evidence(
            &self.snapshot.modified,
            true,
            user_changes_preserved,
        );
        self.enforce_bounds();
        ready
    }

    /// Finish a read-only external-research turn without requiring repository evidence.
    pub fn finish_external_research(&mut self) -> bool {
        self.enforce_bounds();
        true
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
        if name.starts_with("browser.") {
            // Page content is useful only in the live model turn. Keep it out of repository
            // evidence, workflow heuristics, and the in-memory continuation that can be persisted
            // on the next turn.
            self.enforce_bounds();
            return;
        }
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
        let mutation_succeeded = mutation && mutation_applied(name, input, result, command_effects);
        let mut mutation_evidence_refreshed = false;
        if mutation_succeeded {
            let mutation_paths = mutation_paths(name, input, result, command_effects);
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
            self.snapshot.workflow.record_mutation_scope(name, &mutation_paths);
            let evidence_promoted = self.promote_mutation_evidence(&mutation_paths);
            mutation_evidence_refreshed = name == "shell"
                && command_effects.is_some_and(|effects| !effects.uncertain)
                && evidence_promoted
                && !self.snapshot.inspected.iter().any(|file| file.stale);
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
            let command =
                shell_command_display(input).unwrap_or_else(|| format!("{name} verification"));
            self.snapshot.workflow.record_work_verification(
                passed,
                &command,
                &self.snapshot.modified,
                extract_diagnostics(result),
            );
            if passed {
                if let Some(failure_key) = failure_key.as_deref() {
                    self.snapshot.workflow.clear_failure(failure_key);
                }
            } else {
                let diagnostics = extract_diagnostics(result);
                for diagnostic in &diagnostics {
                    self.record_relevant_path(diagnostic.path.as_str());
                }
                let category = failure_category(name, "verification_failed", result);
                self.snapshot.workflow.record_failure(WorkflowFailure::new_at_revision(
                    failure_key.as_deref().unwrap_or("operation"),
                    name,
                    "verification_failed",
                    verification_message(result),
                    category,
                    self.snapshot.workflow.workspace_revision,
                    diagnostics,
                ));
                self.promote_diagnostic_evidence(&extract_diagnostics(result));
            }
        }
        if name == "read" && result.ok && read_contains_files(result) {
            self.snapshot
                .workflow
                .record_inspection_with_provenance(aether_core::EvidenceProvenance::ToolOutput);
        }
        if result.ok
            && (matches!(name, "read" | "find" | "list" | "search" | "git" | "write" | "patch")
                || mutation_evidence_refreshed)
            && !self.snapshot.inspected.iter().any(|file| file.stale)
        {
            self.snapshot.workspace_changed = false;
            if self.snapshot.workflow.recovery.category == WorkflowFailureCategory::StaleEvidence {
                self.snapshot.workflow.clear_recovery();
            }
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

    /// Return the first planned verification command that is still pending for the live revision.
    /// The command is reconstructed from the repository map and matched against the persisted
    /// workflow plan, so a stale or changed plan cannot silently authorize a different command.
    pub(crate) fn next_pending_verification(&self) -> Option<crate::PlannedCommand> {
        let repo_map = self.repo_map.as_ref()?;
        let repo = repo_map.snapshot().ok()?;
        let revision = self.snapshot.workflow.workspace_revision;
        let pending = self
            .snapshot
            .workflow
            .verification_steps
            .iter()
            .filter(|step| {
                step.required
                    && step.revision == revision
                    && step.status == aether_core::VerificationStatus::Pending
            })
            .map(|step| step.command.as_str())
            .collect::<Vec<_>>();
        plan_verification(&repo, &self.snapshot.modified)
            .commands
            .into_iter()
            .find(|command| pending.iter().any(|expected| *expected == command.workflow_key()))
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
                    } else if file.stale {
                        // A hash refresh only proves that the file is stable since the drift was
                        // noticed. A fresh targeted read is still required before old excerpts
                        // and workflow evidence become trustworthy again.
                        file.last_state = ObservedFileState::Present;
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
            self.snapshot.workflow.progress.note_stale_invalidation();
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

    /// Render a small, high-signal update immediately after a failure or mutation. Unlike a full
    /// delta packet this contains only current affected paths and the newest bounded source
    /// windows, keeping the recovery loop cheap while still avoiding a redundant read round-trip.
    pub(crate) fn render_evidence_delta(&self) -> String {
        let mut out = format!(
            "evidence_update: revision={} workspace_changed={}\n",
            self.snapshot.workflow.workspace_revision, self.snapshot.workspace_changed
        );
        render_workflow_paths(&mut out, "affected", &self.snapshot.modified);
        let mut shown = 0;
        for excerpt in self.snapshot.excerpts.iter().rev() {
            if shown == 3 {
                break;
            }
            let current = self.snapshot.inspected.iter().find(|file| file.path == excerpt.path);
            if current.is_some_and(|file| file.stale) {
                continue;
            }
            out.push_str("source ");
            out.push_str(&excerpt.path);
            let _ = writeln!(out, ":{}-{}", excerpt.start_line, excerpt.end_line);
            out.push_str(excerpt.text.as_str());
            out.push('\n');
            shown += 1;
        }
        BoundedText::new(out, MAX_EVIDENCE_UPDATE_BYTES).into_string()
    }

    fn render_internal(&mut self, include_guidance: bool, delta_only: bool) -> String {
        let mut out = String::from("# AETHER workspace context\n");
        if !self.snapshot.workspace_root.is_empty() {
            out.push_str("workspace: ");
            out.push_str(&self.snapshot.workspace_root);
            out.push('\n');
        }
        if self.snapshot.workspace_changed {
            out.push_str(
                "workspace_changed: true; treat persisted hashes and excerpts as stale until re-read\n",
            );
        }
        self.render_workflow(&mut out);
        // Keep the bounded operational projection beside the workflow facts that it summarizes.
        // The projection is derived from the snapshot, so it cannot become a second source of
        // truth, but making it explicit here lets every model turn resume from the same compact
        // target/freshness/blocker view without another bookkeeping round-trip.
        let working_set = self.working_set();
        let working_set_render = working_set.render();
        let working_set_fingerprint = working_set_fingerprint(&working_set_render);
        let working_set_changed = !delta_only
            || !self
                .snapshot
                .rendered_fingerprints
                .iter()
                .any(|seen| seen == &working_set_fingerprint);
        if working_set_changed {
            out.push_str("working_set: ");
            out.push_str(&working_set_render);
            out.push('\n');
        }
        let symbol_paths = relevant_symbol_paths(&self.snapshot);
        let active_instruction_paths = if symbol_paths.is_empty() {
            vec![".".to_owned()]
        } else {
            symbol_paths.iter().map(|path| (*path).to_owned()).collect()
        };
        let (repo_metadata, scoped_instruction_storage) = self
            .repo_map
            .as_ref()
            .and_then(|map| map.snapshot().ok())
            .map(|snapshot| {
                let instructions = active_instruction_paths
                    .iter()
                    .take(MAX_AUTOMATIC_EVIDENCE_FILES)
                    .fold(Vec::new(), |mut instructions, path| {
                        let content = snapshot.effective_instructions(path, 4 * 1024);
                        if !content.is_empty()
                            && !instructions.iter().any(|(_, existing)| existing == &content)
                        {
                            instructions.push((path.clone(), content));
                        }
                        instructions
                    });
                (snapshot.compact_without_instructions(MAX_REPO_MAP_CONTEXT_BYTES), instructions)
            })
            .unwrap_or_default();
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
        let active_subgoal_paths = {
            let mut paths = Vec::new();
            if let Some(active_id) = self.snapshot.workflow.work.active_subgoal.as_ref()
                && let Some(subgoal) =
                    self.snapshot.workflow.work.subgoals.iter().find(|s| &s.id == active_id)
            {
                for path in &subgoal.paths {
                    let path_str = path.as_str().to_owned();
                    if !path_str.is_empty() && !paths.contains(&path_str) {
                        paths.push(path_str);
                    }
                }
            }
            for subgoal in &self.snapshot.workflow.work.subgoals {
                for path in &subgoal.paths {
                    let path_str = path.as_str().to_owned();
                    if !path_str.is_empty() && !paths.contains(&path_str) {
                        paths.push(path_str);
                    }
                }
            }
            paths
        };
        let failure_diagnostic_paths = {
            let mut paths = Vec::new();
            for failure in &self.snapshot.workflow.unresolved_failures {
                for diagnostic in &failure.diagnostics {
                    let path = diagnostic.path.as_str().to_owned();
                    if !path.is_empty() && !paths.contains(&path) {
                        paths.push(path);
                    }
                }
            }
            for recovery_path in &self.snapshot.workflow.recovery.affected_paths {
                let path = recovery_path.as_str().to_owned();
                if !path.is_empty() && !paths.contains(&path) {
                    paths.push(path);
                }
            }
            paths
        };
        let mut candidates = Vec::with_capacity(
            self.snapshot.inspected.len()
                + self.snapshot.modified.len()
                + self.snapshot.tool_summaries.len()
                + self.snapshot.excerpts.len()
                + scoped_instruction_storage.len()
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
            let targeted_subgoal = active_subgoal_paths.iter().any(|p| p == &file.path);
            let diagnostic_path = failure_diagnostic_paths.iter().any(|p| p == &file.path);
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
                targeted_subgoal,
                diagnostic_path,
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
            let targeted_subgoal = active_subgoal_paths.iter().any(|p| p == path);
            let diagnostic_path = failure_diagnostic_paths.iter().any(|p| p == path);
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
                targeted_subgoal,
                diagnostic_path,
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
            let targeted_subgoal = active_subgoal_paths.iter().any(|p| p == path);
            let diagnostic_path = failure_diagnostic_paths.iter().any(|p| p == path);
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
                targeted_subgoal,
                diagnostic_path,
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
                targeted_subgoal: false,
                diagnostic_path: false,
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
            let targeted_subgoal = active_subgoal_paths.iter().any(|p| p == &excerpt.path);
            let diagnostic_path = failure_diagnostic_paths.iter().any(|p| p == &excerpt.path);
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
                targeted_subgoal,
                diagnostic_path,
            });
            recency += 1;
        }
        for (path, content) in &scoped_instruction_storage {
            let fingerprint = candidate_fingerprint(
                task,
                ContextKind::ScopedInstructions,
                Some(path),
                content,
                &[],
                false,
                false,
            );
            candidates.push(ContextCandidate {
                kind: ContextKind::ScopedInstructions,
                path: Some(path),
                content,
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: false,
                phase,
                delta: !delta_only
                    || !previous_fingerprints.contains(&fingerprint_hex(fingerprint)),
                fingerprint,
                targeted_subgoal: false,
                diagnostic_path: false,
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
                targeted_subgoal: false,
                diagnostic_path: false,
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
                targeted_subgoal: false,
                diagnostic_path: false,
            });
        }
        let fixed_bytes = out.len() + CONTEXT_GUIDANCE.len() + 32;
        let base_budget = match self.snapshot.workflow.phase {
            WorkflowPhase::Discover => 12 * 1024,
            WorkflowPhase::Inspect => 16 * 1024,
            WorkflowPhase::Modify => 18 * 1024,
            WorkflowPhase::Verify => 14 * 1024,
            WorkflowPhase::Complete => 10 * 1024,
        };
        let effective_budget = if self.snapshot.workflow.has_unresolved_failures()
            || self.snapshot.workspace_changed
            || self.snapshot.workflow.recovery.active
        {
            MAX_CONTEXT_RENDER_BYTES
        } else {
            base_budget.min(MAX_CONTEXT_RENDER_BYTES)
        };
        let selection = select_context_with_relationships(
            self.snapshot.current_task.as_str(),
            &candidates,
            effective_budget.saturating_sub(fixed_bytes),
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
        if working_set_changed {
            self.snapshot.rendered_fingerprints.push(working_set_fingerprint);
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
                ContextKind::ScopedInstructions => "instructions",
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
        out.push_str(" dir=");
        out.push_str(workflow.director.phase.as_str());
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
        if workflow.recovery.active {
            out.push_str("workflow_recovery: category=");
            out.push_str(workflow.recovery.category.as_str());
            out.push_str(" action=");
            out.push_str(workflow.recovery.action.as_str());
            out.push_str(" attempts=");
            let _ = write!(out, "{}", workflow.recovery.attempts);
            out.push_str(" revision=");
            let _ = write!(out, "{}", workflow.recovery.revision);
            if !workflow.recovery.affected_paths.is_empty() {
                out.push_str(" paths=");
                for (index, path) in workflow.recovery.affected_paths.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(path.as_str());
                }
            }
            out.push('\n');
        }
        let work = &workflow.work;
        if work.is_structured() {
            out.push_str("work: mode=");
            out.push_str(work.mode.as_str());
            out.push_str(" objective=");
            out.push_str(work.objective.as_str());
            out.push_str(" subgoals=");
            let _ = write!(out, "{}", work.subgoals.len());
            out.push_str(" pending=");
            let _ = write!(out, "{}", work.unresolved_count());
            out.push_str(" revision=");
            let _ = write!(out, "{}", work.revision);
            out.push('\n');
            if let Some(active) = &work.active_subgoal {
                out.push_str("work_active: ");
                out.push_str(active.as_str());
                out.push('\n');
            }
            for subgoal in work
                .subgoals
                .iter()
                .filter(|subgoal| !matches!(subgoal.status, aether_core::WorkStatus::Complete))
                .take(4)
            {
                out.push_str("work_subgoal: ");
                out.push_str(subgoal.status.as_str());
                out.push(' ');
                out.push_str(subgoal.id.as_str());
                out.push_str(" — ");
                out.push_str(subgoal.label.as_str());
                out.push('\n');
            }
            if let Some(criterion) =
                work.acceptance_criteria.iter().find(|criterion| !criterion.satisfied)
            {
                out.push_str("work_acceptance: ");
                out.push_str(criterion.key.as_str());
                out.push_str(" — ");
                out.push_str(criterion.description.as_str());
                out.push('\n');
            }
        }
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
            out.push_str(" category=");
            out.push_str(failure.category.as_str());
            out.push_str(" attempts=");
            let _ = write!(out, "{}", failure.attempts);
            out.push_str(" — ");
            out.push_str(failure.message.as_str());
            out.push('\n');
            for diagnostic in failure.diagnostics.iter().take(4) {
                out.push_str("workflow_diagnostic: ");
                out.push_str(diagnostic.path.as_str());
                let _ = write!(out, ":{}", diagnostic.line);
                if let Some(column) = diagnostic.column {
                    let _ = write!(out, ":{}", column);
                }
                if !diagnostic.code.as_str().is_empty() {
                    out.push_str(" code=");
                    out.push_str(diagnostic.code.as_str());
                }
                if !diagnostic.symbol.as_str().is_empty() {
                    out.push_str(" symbol=");
                    out.push_str(diagnostic.symbol.as_str());
                }
                out.push_str(" — ");
                out.push_str(diagnostic.detail.as_str());
                out.push('\n');
            }
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
        if plan.requires_broader || plan.commands.len() > 1 {
            self.snapshot.workflow.progress.note_verification_escalation();
        }
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
        let category = failure_category(name, code, result);
        let diagnostics = extract_diagnostics(result);
        for diagnostic in &diagnostics {
            self.record_relevant_path(diagnostic.path.as_str());
        }
        self.promote_diagnostic_evidence(&diagnostics);
        self.snapshot.workflow.record_failure(WorkflowFailure::new_at_revision(
            key,
            name,
            code,
            result.error.as_ref().map_or(result.output.as_str(), |error| error.message.as_str()),
            category,
            self.snapshot.workflow.workspace_revision,
            diagnostics,
        ));
    }

    fn promote_diagnostic_evidence(&mut self, diagnostics: &[WorkflowDiagnostic]) {
        if self.snapshot.workspace_root.is_empty() {
            return;
        }
        let root = self.snapshot.workspace_root.clone();
        let mut seen = Vec::new();
        for diagnostic in diagnostics.iter().take(MAX_AUTOMATIC_EVIDENCE_FILES) {
            let path = diagnostic.path.as_str();
            if !path_is_workspace_relative(path) || seen.iter().any(|existing| existing == path) {
                continue;
            }
            seen.push(path.to_owned());
            let line = usize::try_from(diagnostic.line).unwrap_or(usize::MAX).max(1);
            let focus = LineRange {
                start: line.saturating_sub(AUTOMATIC_DIAGNOSTIC_LINES_BEFORE),
                end: line.saturating_add(AUTOMATIC_DIAGNOSTIC_LINES_AFTER),
            };
            if let Some(excerpt) = read_auto_excerpt(Path::new(&root), path, Some(focus)) {
                self.store_automatic_excerpt(excerpt);
                self.snapshot.workflow.progress.note_automatic_evidence_reaction();
                self.snapshot.workflow.progress.note_diagnostic_hydration();
            }
        }
    }

    fn promote_mutation_evidence(&mut self, paths: &[String]) -> bool {
        if self.snapshot.workspace_root.is_empty() || paths.is_empty() {
            return false;
        }
        let root = self.snapshot.workspace_root.clone();
        let mut seen = Vec::new();
        let mut all_refreshed = paths.len() <= MAX_AUTOMATIC_EVIDENCE_FILES;
        let valid_path_count = paths.iter().filter(|path| path_is_workspace_relative(path)).count();
        for path in paths.iter().take(MAX_AUTOMATIC_EVIDENCE_FILES) {
            if !path_is_workspace_relative(path) {
                all_refreshed = false;
                continue;
            }
            if seen.iter().any(|existing| existing == path) {
                continue;
            }
            seen.push(path.clone());
            let Some(excerpt) = read_auto_excerpt(Path::new(&root), path, None) else {
                all_refreshed = false;
                continue;
            };
            self.store_automatic_excerpt(excerpt);
            self.snapshot.workflow.progress.note_automatic_evidence_reaction();
            self.snapshot.workflow.progress.note_mutation_refresh();
        }
        all_refreshed && !seen.is_empty() && seen.len() == valid_path_count
    }

    fn store_automatic_excerpt(&mut self, excerpt: FileExcerpt) {
        let path = excerpt.path.clone();
        let hash = excerpt.content_hash.clone();
        let range = Some(LineRange { start: excerpt.start_line, end: excerpt.end_line });
        self.upsert_inspected(&path, hash, range, ObservedFileState::Present);
        self.record_relevant_path(&path);
        self.push_excerpt(excerpt);
        self.snapshot
            .workflow
            .record_inspection_with_provenance(aether_core::EvidenceProvenance::ToolOutput);
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
            let Some(path) = portable_workspace_path(path) else {
                continue;
            };
            let hash = file.get("content_hash").and_then(Value::as_str).map(str::to_owned);
            let lines = file.get("lines").and_then(Value::as_array);
            let range = lines.and_then(|lines| line_range_from_read(lines));
            self.upsert_inspected(&path, hash.clone(), range, ObservedFileState::Present);
            if let (Some(range), Some(lines)) = (range, lines)
                && let Some(text) = excerpt_from_lines(lines)
            {
                self.push_excerpt(FileExcerpt {
                    path,
                    start_line: range.start,
                    end_line: range.end,
                    text: BoundedText::new(text, MAX_EXCERPT_BYTES),
                    content_hash: hash,
                    truncated: file.get("truncated").and_then(Value::as_bool).unwrap_or(false),
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
        let Some(path) = portable_workspace_path(path) else {
            return;
        };
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
            path,
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
        let stale = self.snapshot.workspace_changed
            || self.snapshot.inspected.iter().any(|file| file.stale);
        let modified = self.snapshot.modified.clone();
        self.snapshot.workflow.sync_director(&modified, stale);
    }
}

fn path_is_workspace_relative(path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
}

fn portable_workspace_path(path: &str) -> Option<String> {
    WorkspacePath::new(path).ok().map(|path| path.display())
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

fn mutation_paths(
    name: &str,
    input: &Value,
    result: &ToolResult,
    effects: Option<&CommandEffects>,
) -> Vec<String> {
    let mut paths = Vec::new();
    let mut push = |path: &str| {
        if path_is_workspace_relative(path)
            && !paths.iter().any(|existing| existing == path)
            && paths.len() < MAX_CONTEXT_ITEMS
        {
            paths.push(path.to_owned());
        }
    };
    if name == "write" {
        input.get("path").and_then(Value::as_str).into_iter().for_each(&mut push);
        result
            .data
            .as_ref()
            .and_then(|data| data.get("path"))
            .and_then(Value::as_str)
            .into_iter()
            .for_each(&mut push);
    }
    if name == "patch"
        && let Some(files) = result.data.as_ref().and_then(|data| data.get("files"))
        && let Some(files) = files.as_array()
    {
        for path in files.iter().filter_map(|file| file.get("path")).filter_map(Value::as_str) {
            push(path);
        }
    }
    if let Some(effects) = effects {
        for path in effects.paths.iter().chain(effects.manifests.iter()) {
            push(path);
        }
    }
    paths
}

fn failure_category(name: &str, code: &str, result: &ToolResult) -> WorkflowFailureCategory {
    let output = result.output.as_str().to_ascii_lowercase();
    match code {
        "patch_conflict" | "context_mismatch" => WorkflowFailureCategory::PatchConflict,
        "concurrent_modification" if name == "patch" => WorkflowFailureCategory::PatchConflict,
        "insufficient_evidence" | "inspection_required" | "concurrent_modification" => {
            WorkflowFailureCategory::StaleEvidence
        }
        "permission_denied" | "permission_required" => WorkflowFailureCategory::Permission,
        "cancelled" | "timeout" | "process_limit" => WorkflowFailureCategory::Environment,
        "verification_mismatch" | "verification_scope_mismatch" => {
            WorkflowFailureCategory::VerificationMismatch
        }
        "verification_failed"
            if output.contains("could not compile")
                || output.contains("error[")
                || output.contains("rustc") =>
        {
            WorkflowFailureCategory::Compiler
        }
        "verification_failed"
            if output.contains("test failed")
                || output.contains("assertion")
                || output.contains("panicked") =>
        {
            WorkflowFailureCategory::Test
        }
        "verification_failed" => WorkflowFailureCategory::Verification,
        _ if matches!(name, "shell" | "process") => WorkflowFailureCategory::Command,
        _ => WorkflowFailureCategory::Tool,
    }
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
    let canonical_root = root.canonicalize().ok()?;
    let path = root.join(relative.as_path());
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    let bytes = fs::read(canonical).ok()?;
    if bytes.len() > 4 * 1024 * 1024 {
        return None;
    }
    Some(blake3::hash(&bytes).to_hex().to_string())
}

fn working_set_fingerprint(rendered: &str) -> String {
    let digest = blake3::hash(rendered.as_bytes()).to_hex().to_string();
    format!("working_set:{}", &digest[..16])
}

/// Read only a small, canonicalized source window for runtime recovery context. This observation
/// never substitutes for the tool's execution permit or its final hash/containment revalidation.
fn read_auto_excerpt(root: &Path, relative: &str, focus: Option<LineRange>) -> Option<FileExcerpt> {
    let relative_path = WorkspacePath::new(relative).ok()?;
    let canonical_root = root.canonicalize().ok()?;
    let candidate = root.join(relative_path.as_path());
    let metadata = fs::symlink_metadata(&candidate).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let canonical = candidate.canonicalize().ok()?;
    if !canonical.starts_with(&canonical_root) {
        return None;
    }
    let file = fs::File::open(canonical).ok()?;
    let mut bytes = Vec::with_capacity(8192);
    file.take((MAX_AUTOMATIC_SOURCE_BYTES + 1) as u64).read_to_end(&mut bytes).ok()?;
    if bytes.len() > MAX_AUTOMATIC_SOURCE_BYTES {
        return None;
    }
    if bytes.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(&bytes).ok()?;
    let lines = text.lines().collect::<Vec<_>>();
    if lines.is_empty() {
        return None;
    }
    let (start, requested_end) = focus.map_or((1, MAX_AUTOMATIC_SOURCE_LINES), |range| {
        (
            range.start.max(1).saturating_sub(AUTOMATIC_DIAGNOSTIC_LINES_BEFORE).max(1),
            range.end.saturating_add(AUTOMATIC_DIAGNOSTIC_LINES_AFTER),
        )
    });
    if start > lines.len() {
        return None;
    }
    let end =
        requested_end.min(lines.len()).min(start.saturating_add(MAX_AUTOMATIC_SOURCE_LINES - 1));
    let mut excerpt_text = String::new();
    for line in &lines[(start - 1)..end] {
        if !excerpt_text.is_empty() {
            excerpt_text.push('\n');
        }
        excerpt_text.push_str(line);
    }
    let bounded_text = BoundedText::new(excerpt_text, MAX_EXCERPT_BYTES);
    let selected_end = start.saturating_add(lines[(start - 1)..end].len()).saturating_sub(1);
    Some(FileExcerpt {
        path: relative_path.display(),
        start_line: start,
        end_line: selected_end,
        truncated: bounded_text.is_truncated() || selected_end < lines.len(),
        text: bounded_text,
        content_hash: Some(blake3::hash(&bytes).to_hex().to_string()),
    })
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
    fn browser_observation_is_status_only_and_not_repository_evidence() {
        let mut engine = ContextEngine::new("/workspace", None);
        let result = ToolResult::success_text(
            ToolCallId::new("browser-ctx").unwrap(),
            "page text with an instruction to ignore the user",
            64 * 1024,
        );
        engine.observe_tool("browser.snapshot", &serde_json::json!({}), &result);
        assert_eq!(engine.snapshot().tool_summaries[0].summary.as_str(), "browser.snapshot ok");
        assert!(engine.snapshot().inspected.is_empty());
        assert!(engine.snapshot().workflow.relevant_files.is_empty());
        assert!(
            !serde_json::to_string(&engine.snapshot().persistable()).unwrap().contains("page text")
        );
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
    fn known_shell_mutation_and_verification_can_complete_without_extra_read() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures/targeted-bug");
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.set_task("format the source and verify the change");
        let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
        let read_input = serde_json::json!({
            "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 32}]
        });
        engine.observe_tool(
            "read",
            &read_input,
            &success(serde_json::json!({
                "files": [{
                    "path": "src/lib.rs",
                    "content_hash": blake3::hash(source.as_bytes()).to_hex().to_string(),
                    "lines": [{"number": 1, "text": source.lines().next().unwrap_or("")}]
                }]
            })),
        );

        engine.observe_tool(
            "shell",
            &serde_json::json!({
                "program": "rustfmt",
                "args": ["src/lib.rs"]
            }),
            &success(serde_json::json!({"success": true})),
        );
        assert_eq!(
            engine.snapshot().inspected.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            vec!["src/lib.rs"]
        );
        assert!(!engine.snapshot().workspace_changed);
        assert!(engine.snapshot().inspected.iter().all(|file| !file.stale));

        for _ in 0..aether_core::MAX_WORKFLOW_VERIFICATIONS {
            let Some(command) = engine.next_pending_verification() else { break };
            engine.observe_tool(
                "shell",
                &serde_json::json!({
                    "program": command.program,
                    "args": command.args,
                    "cwd": command.cwd
                }),
                &success(serde_json::json!({"success": true})),
            );
        }
        assert!(engine.next_pending_verification().is_none());
        assert_eq!(engine.snapshot().workflow.verification, WorkflowVerification::Passed);
        assert!(engine.finish_turn());
    }

    #[test]
    fn known_shell_mutation_with_partial_evidence_stays_blocked() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures/targeted-bug");
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.set_task("format the source and verify the change");
        let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
        let read_input = serde_json::json!({
            "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 32}]
        });
        engine.observe_tool(
            "read",
            &read_input,
            &success(serde_json::json!({
                "files": [{
                    "path": "src/lib.rs",
                    "content_hash": blake3::hash(source.as_bytes()).to_hex().to_string(),
                    "lines": [{"number": 1, "text": source.lines().next().unwrap_or("")}]
                }]
            })),
        );

        engine.observe_tool(
            "shell",
            &serde_json::json!({
                "program": "rustfmt",
                "args": ["src/lib.rs", "missing.rs"]
            }),
            &success(serde_json::json!({"success": true})),
        );
        assert_eq!(
            engine.snapshot().inspected.iter().map(|file| file.path.as_str()).collect::<Vec<_>>(),
            vec!["src/lib.rs"]
        );
        assert!(engine.snapshot().workspace_changed);
        assert!(engine.snapshot().inspected.iter().all(|file| !file.stale));
        assert!(!engine.finish_turn());
    }

    #[test]
    fn known_shell_refresh_matches_native_separator_inspected_paths() {
        let root = std::env::temp_dir().join(format!(
            "aether-context-native-sep-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        let source = b"fn main() {}\n";
        std::fs::write(root.join("src").join("lib.rs"), source).unwrap();
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.set_task("format the source and verify the change");
        let native_path = format!("src{}lib.rs", std::path::MAIN_SEPARATOR);
        engine.observe_tool(
            "read",
            &serde_json::json!({
                "files": [{"path": native_path, "start_line": 1, "end_line": 1}]
            }),
            &success(serde_json::json!({
                "files": [{
                    "path": native_path,
                    "content_hash": blake3::hash(source).to_hex().to_string(),
                    "lines": [{"number": 1, "text": "fn main() {}"}]
                }]
            })),
        );

        engine.observe_tool(
            "shell",
            &serde_json::json!({
                "program": "rustfmt",
                "args": ["src/lib.rs"]
            }),
            &success(serde_json::json!({"success": true})),
        );
        assert_eq!(engine.snapshot().inspected.len(), 1);
        assert_eq!(engine.snapshot().inspected[0].path, "src/lib.rs");
        assert!(!engine.snapshot().inspected[0].stale);
        assert!(!engine.snapshot().workspace_changed);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn uncertain_shell_mutation_remains_stale_and_blocks_completion() {
        let mut engine = ContextEngine::new("/workspace", None);
        engine.set_task("update the source and verify the change");
        let read_input = serde_json::json!({
            "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
        });
        engine.observe_tool(
            "read",
            &read_input,
            &success(serde_json::json!({
                "files": [{
                    "path": "src/lib.rs",
                    "content_hash": "before",
                    "lines": [{"number": 1, "text": "fn main() {}"}]
                }]
            })),
        );

        engine.observe_tool(
            "shell",
            &serde_json::json!({
                "program": "sh",
                "args": ["-c", "cargo fmt -- src/lib.rs"]
            }),
            &success(serde_json::json!({"success": true})),
        );
        assert!(engine.snapshot().workspace_changed);
        assert!(engine.snapshot().inspected.iter().any(|file| file.stale));
        assert!(!engine.finish_turn());
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
        assert_eq!(
            engine.snapshot().workflow.recovery.category,
            WorkflowFailureCategory::StaleEvidence
        );
        assert_eq!(engine.snapshot().workflow.director.phase, aether_core::TurnPhase::Recover);
        assert_eq!(engine.snapshot().workflow.progress.stale_invalidations, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_refresh_rejects_symlinked_inspected_files() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "aether-context-symlink-refresh-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        let outside = std::env::temp_dir().join(format!(
            "aether-context-symlink-outside-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&outside, "outside source\n").unwrap();
        symlink(&outside, root.join("src.rs")).unwrap();

        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.observe_tool(
            "read",
            &serde_json::json!({"files": [{"path": "src.rs"}]}),
            &success(serde_json::json!({
                "files": [{
                    "path": "src.rs",
                    "content_hash": blake3::hash(b"outside source\n").to_hex().to_string(),
                    "lines": [{"number": 1, "text": "outside source"}]
                }]
            })),
        );

        let stale = engine.refresh_against_workspace(&root);
        assert_eq!(stale, vec!["src.rs".to_owned()]);
        assert!(engine.snapshot().inspected[0].stale);
        assert!(engine.snapshot().workspace_changed);
        assert!(engine.snapshot().excerpts.is_empty());
        assert!(read_auto_excerpt(&root, "src.rs", None).is_none());

        let _ = std::fs::remove_file(root.join("src.rs"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_file(outside);
    }

    #[test]
    fn workflow_follows_discover_inspect_modify_verify_complete() {
        let mut engine = ContextEngine::new("/workspace", None);
        engine.set_task("update the source and verify the change");
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
        read_only.set_task("inspect the readme");
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
        coding.set_task("update the readme and verify the change");
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
    fn patch_conflict_and_environment_failures_choose_bounded_recovery() {
        let mut patch = ContextEngine::new("/workspace", None);
        patch.observe_tool(
            "patch",
            &serde_json::json!({"files": [{"path": "src/lib.rs"}]}),
            &ToolResult::failure(
                ToolCallId::new("patch-conflict").unwrap(),
                "patch_conflict",
                "source context changed",
                true,
                1024,
            ),
        );
        assert_eq!(
            patch.snapshot().workflow.unresolved_failures[0].category,
            WorkflowFailureCategory::PatchConflict
        );
        assert_eq!(
            patch.snapshot().workflow.recovery.action,
            aether_core::RecoveryAction::RequestNewDecision
        );
        assert!(patch.snapshot().modified.is_empty());

        let mut environment = ContextEngine::new("/workspace", None);
        environment.observe_tool(
            "shell",
            &serde_json::json!({"program": "cargo", "args": ["test"]}),
            &ToolResult::failure(
                ToolCallId::new("environment-failure").unwrap(),
                "timeout",
                "command timed out",
                true,
                1024,
            ),
        );
        assert_eq!(
            environment.snapshot().workflow.unresolved_failures[0].category,
            WorkflowFailureCategory::Environment
        );
        assert_eq!(
            environment.snapshot().workflow.recovery.action,
            aether_core::RecoveryAction::RecordBlocker
        );
        assert!(environment.snapshot().modified.is_empty());
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
        assert!(rendered.contains("working_set: phase="));
        assert!(rendered.contains("workflow_relevant: src/lib.rs"));
        assert!(!rendered.contains("raw transcript"));

        engine.set_workspace_changed(true);
        assert_eq!(engine.snapshot().workflow.phase, WorkflowPhase::Discover);
        assert!(engine.snapshot().workflow.relevant_files.is_empty());
    }

    #[test]
    fn stale_context_blocks_completion_until_current_observation() {
        let mut engine = ContextEngine::new("/workspace", None);
        engine.set_task("inspect the source");
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
    fn mutation_exposes_a_current_revision_verification_command() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures/targeted-bug");
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.set_task("repair the source and verify the change");
        let source = std::fs::read_to_string(root.join("src/lib.rs")).unwrap();
        let read_input = serde_json::json!({
            "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 32}]
        });
        let read = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": blake3::hash(source.as_bytes()).to_hex().to_string(),
                "lines": [{"number": 1, "text": source.lines().next().unwrap_or("")}]
            }]
        }));
        engine.observe_tool("read", &read_input, &read);
        engine.observe_tool(
            "write",
            &serde_json::json!({"path": "src/lib.rs"}),
            &success(serde_json::json!({
                "path": "src/lib.rs",
                "hash": "new-hash"
            })),
        );
        assert!(!engine.snapshot().workflow.verification_steps.is_empty());
        assert!(engine.next_pending_verification().is_some());
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
        assert!(!unchanged_again.contains("working_set: phase="));
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

    #[test]
    fn failed_diagnostic_promotes_bounded_current_source_context() {
        let root = std::env::temp_dir().join(format!(
            "aether-context-diagnostic-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("src.rs");
        std::fs::write(&path, (1..=32).map(|line| format!("line {line}\n")).collect::<String>())
            .unwrap();
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        let failure = ToolResult::failure(
            ToolCallId::new("diagnostic-failure").unwrap(),
            "verification_failed",
            "error[E0308]: mismatch --> src.rs:12:5 in `parse_value`",
            true,
            4096,
        );
        engine.observe_tool("shell", &Value::Null, &failure);
        let excerpt = engine
            .snapshot()
            .excerpts
            .iter()
            .find(|excerpt| excerpt.path == "src.rs")
            .expect("diagnostic source excerpt");
        assert!(excerpt.start_line <= 12 && excerpt.end_line >= 12);
        assert!(excerpt.text.as_str().contains("line 12"));
        assert!(engine.snapshot().workflow.relevant_files.iter().any(|path| path == "src.rs"));
        assert_eq!(
            engine.snapshot().workflow.unresolved_failures[0].category,
            WorkflowFailureCategory::Compiler
        );
        assert_eq!(engine.snapshot().workflow.director.phase, aether_core::TurnPhase::Recover);
        assert_eq!(engine.snapshot().workflow.progress.diagnostic_hydrations, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn successful_mutation_promotes_reusable_evidence_and_stale_reuse_fails_closed() {
        let root = std::env::temp_dir().join(format!(
            "aether-context-mutation-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("src.rs");
        std::fs::write(&path, "fn current() {}\n").unwrap();
        let hash = blake3::hash(b"fn current() {}\n").to_hex().to_string();
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.observe_tool(
            "write",
            &serde_json::json!({"path": "src.rs"}),
            &success(serde_json::json!({"path": "src.rs", "hash": hash})),
        );
        assert!(engine.snapshot().excerpts.iter().any(|excerpt| excerpt.path == "src.rs"));
        assert_eq!(engine.snapshot().workflow.progress.mutation_refreshes, 1);
        assert_eq!(engine.snapshot().workflow.progress.automatic_evidence_reactions, 1);
        let policy = crate::AutonomousCodingPolicy::new();
        let read = policy.preflight(
            engine.snapshot(),
            ToolCallId::new("reused-read").unwrap(),
            "read",
            &serde_json::json!({"files": [{"path": "src.rs"}]}),
        );
        assert!(read.is_some(), "complete current source should be reusable");

        std::fs::write(&path, "fn changed() {}\n").unwrap();
        assert!(
            policy
                .preflight(
                    engine.snapshot(),
                    ToolCallId::new("changed-read").unwrap(),
                    "read",
                    &serde_json::json!({"files": [{"path": "src.rs"}]})
                )
                .is_none()
        );
        engine.refresh_against_workspace(&root);
        assert!(
            policy
                .preflight(
                    engine.snapshot(),
                    ToolCallId::new("stale-read").unwrap(),
                    "read",
                    &serde_json::json!({"files": [{"path": "src.rs"}]})
                )
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn render_includes_effective_instructions_only_for_active_paths() {
        let root = std::env::temp_dir().join(format!(
            "aether-context-instructions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("other")).unwrap();
        std::fs::write(root.join("AGENTS.md"), "root instruction").unwrap();
        std::fs::write(root.join("src/AGENTS.md"), "src instruction").unwrap();
        std::fs::write(root.join("other/AGENTS.md"), "other instruction").unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn target() {}\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .current_dir(&root)
                .args(["init", "--quiet"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .current_dir(&root)
                .args(["add", "."])
                .status()
                .unwrap()
                .success()
        );
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.set_task("update target");
        engine.snapshot_mut().workflow.record_relevant_file("src/lib.rs");
        let rendered = engine.render(false);
        assert!(rendered.contains("root instruction"));
        assert!(rendered.contains("src instruction"));
        assert!(!rendered.contains("other instruction"));
        let _ = std::fs::remove_dir_all(root);
    }
}
