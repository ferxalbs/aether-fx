use std::path::{Path, PathBuf};

use aether_core::{
    BoundedText, CONTEXT_GUIDANCE, CommandEffects, CompactToolSummary, ContextSnapshot,
    FileExcerpt, GitSnapshot, InspectedFile, LineRange, MAX_CONTEXT_ITEMS,
    MAX_CONTEXT_RENDER_BYTES, MAX_EXCERPT_BYTES, MAX_FILE_EXCERPTS, MAX_INSPECTED_FILES,
    MAX_STORED_TOOL_SUMMARIES, MAX_SUMMARY_PATHS, MAX_TASK_BYTES, MAX_WORKFLOW_FIELD_BYTES,
    ObservedFileState, OpaqueContinuation, ToolResult, WorkflowFailure, WorkflowPhase,
    WorkflowVerification, WorkspacePath, analyze_command, compact_tool_result, merge_line_ranges,
};
use serde_json::Value;

use crate::repo_map::RepoMap;
use crate::{
    ContextCandidate, ContextKind, RelationshipHint, SymbolHint, plan_verification,
    select_context_with_relationships,
};

const MAX_REPO_MAP_CONTEXT_BYTES: usize = 8 * 1024;

fn relevant_symbol_paths(snapshot: &ContextSnapshot) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for path in snapshot
        .inspected
        .iter()
        .map(|file| file.path.as_str())
        .chain(snapshot.modified.iter().map(String::as_str))
        .chain(snapshot.workflow.relevant_files.iter().map(String::as_str))
    {
        let path = PathBuf::from(path);
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
        self.snapshot.workflow.complete_if_ready(&self.snapshot.modified)
    }

    /// Observe a completed tool call and fold it into bounded working state.
    pub fn observe_tool(&mut self, name: &str, input: &Value, result: &ToolResult) {
        let failure_key = workflow_failure_key(name, input);
        let focused_verification = self.is_focused_verification(name, input);
        let command_effects = command_effects_for_tool(name, input);
        let mutation = is_workspace_mutation(name, input, command_effects.as_ref());
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
        if repository_observation {
            self.observe_read(result);
            if read_contains_files(result) {
                self.snapshot.workflow.record_inspection();
            }
        }
        if result.ok {
            self.snapshot.workflow.clear_failure(&failure_key);
        } else {
            self.record_workflow_failure(&failure_key, name, result);
        }
        if mutation && mutation_applied(name, input, result, command_effects.as_ref()) {
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
                self.snapshot.workflow.clear_failure(&failure_key);
            } else {
                self.snapshot.workflow.record_failure(WorkflowFailure::new(
                    failure_key,
                    name,
                    "verification_failed",
                    verification_message(result),
                ));
            }
        }
        if name == "read" && result.ok && read_contains_files(result) {
            self.snapshot.workflow.record_inspection();
        }
        self.enforce_bounds();
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
        let mut valid = Vec::new();
        for mut file in self.snapshot.inspected.drain(..) {
            if !path_is_workspace_relative(&file.path) {
                self.snapshot.workspace_changed = true;
                continue;
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
                    valid.push(file);
                }
                None => {
                    if file.last_state == ObservedFileState::Present {
                        file.stale = true;
                        file.last_state = ObservedFileState::Missing;
                        stale_paths.push(file.path.clone());
                    }
                    valid.push(file);
                }
            }
        }
        self.snapshot.inspected = valid;
        if !stale_paths.is_empty() {
            self.snapshot.workspace_changed = true;
            let stale = stale_paths.iter().cloned().collect::<std::collections::HashSet<_>>();
            self.snapshot.excerpts.retain(|excerpt| !stale.contains(&excerpt.path));
        }
        if root_changed || !stale_paths.is_empty() {
            self.reset_workflow_for_workspace_change();
        }
        self.snapshot.modified.retain(|path| path_is_workspace_relative(path));
        stale_paths
    }

    /// Render a token-conscious packet for model reconstruction. Full files are never included.
    pub fn render(&self, include_guidance: bool) -> String {
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
                map.lookup_symbols(self.snapshot.current_task.as_str(), &symbol_paths, 64).ok()
            })
            .unwrap_or_default();
        let relationship_matches = self
            .repo_map
            .as_ref()
            .and_then(|map| {
                map.lookup_relationships(self.snapshot.current_task.as_str(), &symbol_paths, 64)
                    .ok()
            })
            .unwrap_or_default();
        let symbol_hint_storage = symbol_matches
            .iter()
            .map(|symbol| (symbol.path.to_string_lossy().into_owned(), symbol.symbol.name.clone()))
            .collect::<Vec<_>>();
        let symbol_hints = symbol_hint_storage
            .iter()
            .map(|(path, name)| SymbolHint { path, name })
            .collect::<Vec<_>>();
        let relationship_hint_storage = relationship_matches
            .iter()
            .map(|relationship| {
                (
                    relationship.path.to_string_lossy().into_owned(),
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
        for file in &self.snapshot.inspected {
            candidates.push(ContextCandidate {
                kind: ContextKind::InspectedFile,
                path: Some(&file.path),
                content: "",
                start_line: 0,
                end_line: 0,
                recency,
                modified: self.snapshot.modified.iter().any(|path| path == &file.path),
                stale: file.stale,
            });
            recency += 1;
        }
        for path in &self.snapshot.modified {
            candidates.push(ContextCandidate {
                kind: ContextKind::ModifiedPath,
                path: Some(path),
                content: "",
                start_line: 0,
                end_line: 0,
                recency,
                modified: true,
                stale: false,
            });
            recency += 1;
        }
        for summary in &self.snapshot.tool_summaries {
            candidates.push(ContextCandidate {
                kind: ContextKind::ToolSummary,
                path: None,
                content: summary.summary.as_str(),
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: false,
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
            candidates.push(ContextCandidate {
                kind: ContextKind::Excerpt,
                path: Some(&excerpt.path),
                content: excerpt.text.as_str(),
                start_line: excerpt.start_line,
                end_line: excerpt.end_line,
                recency,
                modified: self.snapshot.modified.iter().any(|path| path == &excerpt.path),
                stale,
            });
            recency += 1;
        }
        if let Some(git) = &self.snapshot.git {
            candidates.push(ContextCandidate {
                kind: ContextKind::GitMetadata,
                path: git.branch.as_deref(),
                content: git.status.as_str(),
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: self.snapshot.workspace_changed,
            });
            recency += 1;
        }
        if !repo_metadata.is_empty() {
            candidates.push(ContextCandidate {
                kind: ContextKind::RepositoryMetadata,
                path: None,
                content: &repo_metadata,
                start_line: 0,
                end_line: 0,
                recency,
                modified: false,
                stale: self.snapshot.workspace_changed,
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
        if !selection.items.is_empty() {
            out.push_str("selected context (relevance-ranked):\n");
        }
        for item in selection.items {
            let candidate = candidates[item.index];
            out.push_str("- ");
            out.push_str(match candidate.kind {
                ContextKind::ModifiedPath => "modified",
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
                out.push_str(&format!(":{}-{}", candidate.start_line, candidate.end_line));
            }
            if candidate.kind == ContextKind::InspectedFile
                && let Some(path) = candidate.path
                && let Some(file) = self.snapshot.inspected.iter().find(|file| file.path == path)
                && !file.ranges.is_empty()
            {
                out.push_str(" ranges=");
                out.push_str(&format_ranges(&file.ranges));
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
        out.push_str(&workflow.relevant_files.len().to_string());
        out.push_str(" modified=");
        out.push_str(&self.snapshot.modified.len().to_string());
        out.push_str(" failures=");
        out.push_str(&workflow.unresolved_failures.len().to_string());
        out.push_str(" progress=");
        out.push_str(&workflow.progress.discoveries.to_string());
        out.push('/');
        out.push_str(&workflow.progress.inspections.to_string());
        out.push('/');
        out.push_str(&workflow.progress.mutations.to_string());
        out.push('/');
        out.push_str(&workflow.progress.verifications.to_string());
        out.push('\n');
        let decision = &workflow.decision;
        out.push_str("decision: action=");
        out.push_str(decision.next_action.as_str());
        out.push_str(" confidence=");
        out.push_str(&decision.progress_confidence.to_string());
        out.push_str(" candidates=");
        out.push_str(&decision.candidate_files.len().to_string());
        out.push_str(" questions=");
        out.push_str(&decision.unresolved_questions.len().to_string());
        if let Some(target) = &decision.next_target {
            out.push_str(" target=");
            out.push_str(target.as_str());
        }
        out.push('\n');
        if let Some(candidate) = decision.candidate_files.first() {
            out.push_str("decision_candidate: ");
            out.push_str(candidate.path.as_str());
            out.push_str(" score=");
            out.push_str(&candidate.score.to_string());
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
        let affected = effects.paths.iter().chain(effects.manifests.iter()).collect::<Vec<_>>();
        if effects.uncertain || affected.is_empty() {
            for file in &mut self.snapshot.inspected {
                file.stale = true;
            }
            self.snapshot.excerpts.clear();
        } else {
            for file in &mut self.snapshot.inspected {
                if affected.iter().any(|path| {
                    file.path == path.as_str()
                        || file.path.starts_with(&format!("{}/", path.trim_end_matches('/')))
                        || path.starts_with(&format!("{}/", file.path.trim_end_matches('/')))
                }) {
                    file.stale = true;
                }
            }
            self.snapshot
                .excerpts
                .retain(|excerpt| !affected.iter().any(|path| excerpt.path == path.as_str()));
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
            self.snapshot.workflow.record_relevant_file(path);
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
    }

    fn upsert_inspected(
        &mut self,
        path: &str,
        hash: Option<String>,
        range: Option<LineRange>,
        last_state: ObservedFileState,
    ) {
        if self.snapshot.inspected.iter().any(|file| file.path == path) {
            let hash_changed = self.snapshot.inspected.iter().any(|file| {
                file.path == path
                    && hash.as_ref().is_some_and(|new_hash| {
                        file.content_hash.as_ref().is_some_and(|old| old != new_hash)
                    })
            });
            if let Some(existing) =
                self.snapshot.inspected.iter_mut().find(|file| file.path == path)
            {
                if let Some(hash) = hash.clone() {
                    existing.content_hash = Some(hash);
                }
                existing.last_state = last_state;
                existing.stale = false;
                if let Some(range) = range {
                    existing.ranges.push(range);
                    merge_line_ranges(&mut existing.ranges);
                }
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

fn format_ranges(ranges: &[LineRange]) -> String {
    ranges
        .iter()
        .map(|range| format!("{}-{}", range.start, range.end))
        .collect::<Vec<_>>()
        .join(",")
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
}
