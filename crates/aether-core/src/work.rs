//! Compact, durable work state for long-horizon coding tasks.
//!
//! This is intentionally a small fact store rather than a second planner. The model still
//! chooses the implementation strategy; the runtime records the objective, observable work
//! units, evidence, mutations, blockers, and verification needed to resume that strategy.

use serde::{Deserialize, Serialize};

use crate::{BoundedText, VerificationStatus};

/// Maximum bytes retained for the normalized task objective.
pub const MAX_WORK_OBJECTIVE_BYTES: usize = 512;
/// Maximum bytes retained for one work-state text field.
pub const MAX_WORK_FIELD_BYTES: usize = 192;
/// Maximum acceptance criteria retained for one task.
pub const MAX_WORK_CRITERIA: usize = 8;
/// Compatibility spelling for callers that describe criteria as acceptance criteria.
pub const MAX_WORK_ACCEPTANCE_CRITERIA: usize = MAX_WORK_CRITERIA;
/// Maximum subgoals retained for one task.
pub const MAX_WORK_SUBGOALS: usize = 16;
/// Maximum evidence records retained for one task.
pub const MAX_WORK_EVIDENCE: usize = 32;
/// Maximum mutations retained for one task.
pub const MAX_WORK_MUTATIONS: usize = 24;
/// Maximum verification records retained for one task.
pub const MAX_WORK_VERIFICATIONS: usize = 24;
/// Maximum blockers retained for one task.
pub const MAX_WORK_BLOCKERS: usize = 8;
/// Maximum dependencies or paths retained on one subgoal.
pub const MAX_WORK_RELATIONS: usize = 8;

/// Whether a task needs explicit work-unit tracking.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkMode {
    #[default]
    Simple,
    Structured,
}

impl WorkMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Simple => "simple",
            Self::Structured => "structured",
        }
    }
}

/// Lifecycle of one bounded work unit.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    #[default]
    Pending,
    Active,
    Complete,
    Blocked,
    Stale,
}

impl WorkStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Complete => "complete",
            Self::Blocked => "blocked",
            Self::Stale => "stale",
        }
    }
}

/// One compact acceptance criterion. Criteria are informational until the model or an
/// integration supplies evidence that satisfies them; they are never guessed as complete merely
/// because a command succeeded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkCriterion {
    pub key: BoundedText,
    pub description: BoundedText,
    pub satisfied: bool,
}

/// One bounded work unit with explicit ordering relationships.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkSubgoal {
    pub id: BoundedText,
    pub label: BoundedText,
    pub status: WorkStatus,
    pub depends_on: Vec<BoundedText>,
    pub paths: Vec<BoundedText>,
    pub attempts: u16,
    pub completed_revision: Option<u32>,
}

/// One fact retained to explain why the current strategy is safe or incomplete.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkEvidence {
    pub kind: BoundedText,
    pub path: BoundedText,
    pub detail: BoundedText,
    pub revision: u32,
    pub valid: bool,
}

/// One successful mutation retained as a compact audit trail.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkMutation {
    pub operation: BoundedText,
    pub paths: Vec<BoundedText>,
    pub revision: u32,
}

/// One verification observation and its current validity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkVerification {
    pub command: BoundedText,
    pub scope: BoundedText,
    pub revision: u32,
    pub status: VerificationStatus,
    pub diagnostics: Vec<BoundedText>,
}

/// One unresolved blocker with bounded retry history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkBlocker {
    pub key: BoundedText,
    pub category: BoundedText,
    pub detail: BoundedText,
    pub attempts: u16,
    pub revision: u32,
}

/// Minimum structured state needed to resume a complex coding task.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkState {
    pub mode: WorkMode,
    pub objective: BoundedText,
    pub acceptance_criteria: Vec<WorkCriterion>,
    pub subgoals: Vec<WorkSubgoal>,
    pub active_subgoal: Option<BoundedText>,
    pub blockers: Vec<WorkBlocker>,
    pub evidence: Vec<WorkEvidence>,
    pub mutations: Vec<WorkMutation>,
    pub verifications: Vec<WorkVerification>,
    pub revision: u32,
}

impl WorkState {
    /// Initialize the compact task objective and complexity mode from the user request.
    pub fn initialize_from_prompt(&mut self, prompt: &str) {
        let objective = normalized_objective(prompt);
        let mode = classify_work_mode(prompt);
        if self.objective.as_str().is_empty() {
            self.reset_task(objective, mode);
        } else {
            // A later prompt in the same session is commonly a continuation or repair
            // instruction. Keep completed work and evidence; update only the compact objective
            // and promote the mode if the new instruction is more complex.
            self.objective = bounded(&objective);
            if mode == WorkMode::Structured {
                self.mode = WorkMode::Structured;
            }
        }
        if mode == WorkMode::Structured && self.subgoals.is_empty() {
            self.subgoals.push(WorkSubgoal {
                id: bounded("task"),
                label: bounded("complete the requested repository task"),
                status: WorkStatus::Active,
                depends_on: Vec::new(),
                paths: Vec::new(),
                attempts: 0,
                completed_revision: None,
            });
            self.active_subgoal = Some(bounded("task"));
        }
        if self.acceptance_criteria.is_empty() {
            self.acceptance_criteria = extract_criteria(prompt);
        }
    }

    /// Return whether this task has enough independent structure to justify work units.
    pub const fn is_structured(&self) -> bool {
        matches!(self.mode, WorkMode::Structured)
    }

    /// Add an explicit bounded work unit supplied by an integration or future model adapter.
    /// The runtime does not infer semantic dependencies; it only preserves and validates them.
    pub fn add_subgoal(&mut self, id: &str, label: &str, depends_on: &[String]) -> bool {
        if id.is_empty() || self.subgoals.iter().any(|subgoal| subgoal.id.as_str() == id) {
            return false;
        }
        self.mode = WorkMode::Structured;
        let dependencies = depends_on
            .iter()
            .take(MAX_WORK_RELATIONS)
            .map(|dependency| bounded(dependency))
            .collect();
        push_bounded(
            &mut self.subgoals,
            WorkSubgoal {
                id: bounded(id),
                label: bounded(label),
                status: WorkStatus::Pending,
                depends_on: dependencies,
                paths: Vec::new(),
                attempts: 0,
                completed_revision: None,
            },
            MAX_WORK_SUBGOALS,
        );
        self.select_active_subgoal();
        true
    }

    /// Update an explicit work-unit status without deriving correctness from it.
    pub fn set_subgoal_status(&mut self, id: &str, status: WorkStatus, revision: u32) -> bool {
        let Some(subgoal) = self.subgoals.iter_mut().find(|subgoal| subgoal.id.as_str() == id)
        else {
            return false;
        };
        subgoal.status = status;
        subgoal.completed_revision = (status == WorkStatus::Complete).then_some(revision);
        self.select_active_subgoal();
        true
    }

    /// Record one bounded repository fact, deduplicating identical observations.
    pub fn record_evidence(&mut self, kind: &str, path: &str, detail: &str, revision: u32) {
        let evidence = WorkEvidence {
            kind: bounded(kind),
            path: bounded(path),
            detail: bounded(detail),
            revision,
            valid: true,
        };
        if self.evidence.iter().any(|existing| existing == &evidence) {
            return;
        }
        push_bounded(&mut self.evidence, evidence, MAX_WORK_EVIDENCE);
    }

    /// Record successful mutations and create one observable work unit per affected path when
    /// structured tracking is active.
    pub fn record_mutation(&mut self, operation: &str, paths: &[String], revision: u32) {
        self.revision = revision;
        let paths = paths.iter().map(|path| bounded(path)).collect::<Vec<_>>();
        push_bounded(
            &mut self.mutations,
            WorkMutation { operation: bounded(operation), paths: paths.clone(), revision },
            MAX_WORK_MUTATIONS,
        );
        for path in paths {
            self.record_evidence("mutation", path.as_str(), operation, revision);
            if self.is_structured() {
                self.ensure_path_subgoal(path.as_str());
            }
        }
        self.select_active_subgoal();
    }

    /// Record verification evidence and close work units whose affected paths are covered by a
    /// passing current-revision check.
    pub fn record_verification(
        &mut self,
        command: &str,
        scope: &str,
        passed: bool,
        revision: u32,
        diagnostics: &[String],
        affected_paths: &[String],
    ) {
        self.revision = revision;
        let status = if passed { VerificationStatus::Passed } else { VerificationStatus::Failed };
        let diagnostics = diagnostics.iter().map(|item| bounded(item)).take(4).collect();
        if let Some(existing) = self
            .verifications
            .iter_mut()
            .find(|existing| existing.command.as_str() == command && existing.revision == revision)
        {
            existing.status = status;
            existing.scope = bounded(scope);
            existing.diagnostics = diagnostics;
        } else {
            push_bounded(
                &mut self.verifications,
                WorkVerification {
                    command: bounded(command),
                    scope: bounded(scope),
                    revision,
                    status,
                    diagnostics,
                },
                MAX_WORK_VERIFICATIONS,
            );
        }
        if passed {
            let completed_ids = self
                .subgoals
                .iter()
                .filter(|subgoal| {
                    !subgoal.paths.is_empty()
                        && subgoal.paths.iter().all(|path| {
                            affected_paths.iter().any(|affected| affected == path.as_str())
                        })
                        && dependencies_complete(subgoal, &self.subgoals)
                })
                .map(|subgoal| subgoal.id.clone())
                .collect::<Vec<_>>();
            for subgoal in &mut self.subgoals {
                if completed_ids.iter().any(|id| id == &subgoal.id) {
                    subgoal.status = WorkStatus::Complete;
                    subgoal.completed_revision = Some(revision);
                }
            }
            let path_work_complete = self
                .subgoals
                .iter()
                .filter(|subgoal| !subgoal.paths.is_empty())
                .all(|subgoal| subgoal.status == WorkStatus::Complete);
            if path_work_complete
                && self.blockers.is_empty()
                && let Some(root) =
                    self.subgoals.iter_mut().find(|subgoal| subgoal.id.as_str() == "task")
            {
                root.status = WorkStatus::Complete;
                root.completed_revision = Some(revision);
            }
            self.select_active_subgoal();
        }
    }

    /// Record a blocker, preserving retry count for the same stable action key.
    pub fn record_blocker(&mut self, key: &str, category: &str, detail: &str, revision: u32) {
        if let Some(existing) =
            self.blockers.iter_mut().find(|existing| existing.key.as_str() == key)
        {
            existing.category = bounded(category);
            existing.detail = bounded(detail);
            existing.attempts = existing.attempts.saturating_add(1);
            existing.revision = revision;
            return;
        }
        push_bounded(
            &mut self.blockers,
            WorkBlocker {
                key: bounded(key),
                category: bounded(category),
                detail: bounded(detail),
                attempts: 1,
                revision,
            },
            MAX_WORK_BLOCKERS,
        );
        let active_id = self.active_subgoal.clone();
        for subgoal in &mut self.subgoals {
            if active_id.as_ref().is_some_and(|active| active.as_str() == subgoal.id.as_str()) {
                subgoal.status = WorkStatus::Blocked;
            }
        }
    }

    /// Clear a blocker after a changed action or successful retry.
    pub fn clear_blocker(&mut self, key: &str) {
        self.blockers.retain(|blocker| blocker.key.as_str() != key);
        for subgoal in &mut self.subgoals {
            if subgoal.status == WorkStatus::Blocked {
                subgoal.status = WorkStatus::Active;
            }
        }
        self.select_active_subgoal();
    }

    /// Mark workspace-derived facts stale while preserving the task objective and work-unit
    /// history needed for recovery.
    pub fn invalidate_for_workspace_change(&mut self, revision: u32) {
        self.revision = revision;
        for evidence in &mut self.evidence {
            evidence.valid = false;
        }
        for verification in &mut self.verifications {
            if verification.revision <= revision {
                verification.status = VerificationStatus::Stale;
            }
        }
        for subgoal in &mut self.subgoals {
            if subgoal.status == WorkStatus::Complete {
                subgoal.status = WorkStatus::Stale;
                subgoal.completed_revision = None;
            }
        }
        self.active_subgoal = None;
        self.select_active_subgoal();
    }

    /// Mark a criterion as satisfied only when an explicit caller supplies that fact.
    pub fn satisfy_criterion(&mut self, key: &str) {
        if let Some(criterion) =
            self.acceptance_criteria.iter_mut().find(|criterion| criterion.key.as_str() == key)
        {
            criterion.satisfied = true;
        }
    }

    /// Return a bounded count of work still pending or blocked.
    pub fn unresolved_count(&self) -> usize {
        self.subgoals
            .iter()
            .filter(|subgoal| !matches!(subgoal.status, WorkStatus::Complete))
            .count()
            + self.blockers.len()
    }

    /// Return whether structured work contains a concrete unresolved unit that should block a
    /// completion claim. A structure-only root for a read-only task is not by itself a blocker;
    /// concrete mutated paths and typed blockers are.
    pub fn blocks_completion(&self) -> bool {
        !self.blockers.is_empty()
            || self
                .subgoals
                .iter()
                .any(|subgoal| !subgoal.paths.is_empty() && subgoal.status != WorkStatus::Complete)
    }

    /// Enforce all bounds after restoring a persisted session.
    pub fn enforce_bounds(&mut self) {
        self.objective = BoundedText::new(self.objective.as_str(), MAX_WORK_OBJECTIVE_BYTES);
        self.acceptance_criteria.truncate(MAX_WORK_CRITERIA);
        self.subgoals.truncate(MAX_WORK_SUBGOALS);
        self.blockers.truncate(MAX_WORK_BLOCKERS);
        self.evidence.truncate(MAX_WORK_EVIDENCE);
        self.mutations.truncate(MAX_WORK_MUTATIONS);
        self.verifications.truncate(MAX_WORK_VERIFICATIONS);
        for criterion in &mut self.acceptance_criteria {
            criterion.key = bounded(criterion.key.as_str());
            criterion.description = bounded(criterion.description.as_str());
        }
        for subgoal in &mut self.subgoals {
            subgoal.id = bounded(subgoal.id.as_str());
            subgoal.label = bounded(subgoal.label.as_str());
            subgoal.depends_on.truncate(MAX_WORK_RELATIONS);
            subgoal.paths.truncate(MAX_WORK_RELATIONS);
            for dependency in &mut subgoal.depends_on {
                *dependency = bounded(dependency.as_str());
            }
            for path in &mut subgoal.paths {
                *path = bounded(path.as_str());
            }
        }
        for blocker in &mut self.blockers {
            blocker.key = bounded(blocker.key.as_str());
            blocker.category = bounded(blocker.category.as_str());
            blocker.detail = bounded(blocker.detail.as_str());
        }
        for evidence in &mut self.evidence {
            evidence.kind = bounded(evidence.kind.as_str());
            evidence.path = bounded(evidence.path.as_str());
            evidence.detail = bounded(evidence.detail.as_str());
        }
        for mutation in &mut self.mutations {
            mutation.operation = bounded(mutation.operation.as_str());
            mutation.paths.truncate(MAX_WORK_RELATIONS);
            for path in &mut mutation.paths {
                *path = bounded(path.as_str());
            }
        }
        for verification in &mut self.verifications {
            verification.command = bounded(verification.command.as_str());
            verification.scope = bounded(verification.scope.as_str());
            verification.diagnostics.truncate(4);
            for diagnostic in &mut verification.diagnostics {
                *diagnostic = bounded(diagnostic.as_str());
            }
        }
        self.active_subgoal = self.active_subgoal.as_ref().map(|active| bounded(active.as_str()));
        self.select_active_subgoal();
    }

    fn reset_task(&mut self, objective: String, mode: WorkMode) {
        self.mode = mode;
        self.objective = bounded(&objective);
        self.acceptance_criteria.clear();
        self.subgoals.clear();
        self.active_subgoal = None;
        self.blockers.clear();
        self.evidence.clear();
        self.mutations.clear();
        self.verifications.clear();
        self.revision = 0;
    }

    fn ensure_path_subgoal(&mut self, path: &str) {
        let id = format!("path:{path}");
        if let Some(subgoal) = self.subgoals.iter_mut().find(|subgoal| subgoal.id.as_str() == id) {
            if !subgoal.paths.iter().any(|existing| existing.as_str() == path) {
                push_bounded(&mut subgoal.paths, bounded(path), MAX_WORK_RELATIONS);
            }
            if subgoal.status == WorkStatus::Stale {
                subgoal.status = WorkStatus::Pending;
            }
            return;
        }
        push_bounded(
            &mut self.subgoals,
            WorkSubgoal {
                id: bounded(&id),
                label: bounded(&format!("complete changes for {path}")),
                status: WorkStatus::Pending,
                depends_on: Vec::new(),
                paths: vec![bounded(path)],
                attempts: 0,
                completed_revision: None,
            },
            MAX_WORK_SUBGOALS,
        );
    }

    fn select_active_subgoal(&mut self) {
        let active_id = self
            .subgoals
            .iter()
            .find(|subgoal| {
                matches!(
                    subgoal.status,
                    WorkStatus::Pending | WorkStatus::Active | WorkStatus::Stale
                ) && dependencies_complete(subgoal, &self.subgoals)
            })
            .map(|subgoal| subgoal.id.clone());
        if let Some(active_id) = active_id.as_ref()
            && let Some(subgoal) = self.subgoals.iter_mut().find(|subgoal| &subgoal.id == active_id)
        {
            subgoal.status = WorkStatus::Active;
        }
        self.active_subgoal = active_id;
    }
}

fn dependencies_complete(subgoal: &WorkSubgoal, subgoals: &[WorkSubgoal]) -> bool {
    subgoal.depends_on.iter().all(|dependency| {
        subgoals
            .iter()
            .find(|candidate| candidate.id == *dependency)
            .is_none_or(|candidate| candidate.status == WorkStatus::Complete)
    })
}

fn push_bounded<T>(items: &mut Vec<T>, item: T, limit: usize) {
    items.push(item);
    if items.len() > limit {
        items.remove(0);
    }
}

fn bounded(value: &str) -> BoundedText {
    BoundedText::new(value, MAX_WORK_FIELD_BYTES)
}

fn classify_work_mode(prompt: &str) -> WorkMode {
    let lower = prompt.to_ascii_lowercase();
    let markers = [
        "multiple",
        "several",
        "cross-file",
        "cross-crate",
        "across",
        "migrate",
        "refactor",
        "workspace",
        "package",
        "crate",
        "module",
        "then",
        "after",
    ];
    let marker_count = markers.iter().filter(|marker| lower.contains(**marker)).count();
    if marker_count > 0
        || prompt.chars().filter(|character| matches!(character, ';' | '\n')).count() >= 2
    {
        WorkMode::Structured
    } else {
        WorkMode::Simple
    }
}

fn normalized_objective(prompt: &str) -> String {
    let line = prompt.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("");
    let mut objective =
        redact_secret_assignments(line).split_whitespace().collect::<Vec<_>>().join(" ");
    if objective.ends_with('.') {
        objective.pop();
    }
    BoundedText::new(objective, MAX_WORK_OBJECTIVE_BYTES).into_string()
}

fn redact_secret_assignments(text: &str) -> String {
    let secret_keys = ["token", "password", "secret", "api_key", "apikey"];
    let mut words = Vec::new();
    let mut pending_key = None;
    for word in text.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if let Some(key) = pending_key.take() {
            if word == "=" {
                pending_key = Some(key);
                continue;
            }
            words.push(format!("{key}=<redacted>"));
            continue;
        }
        if let Some(key) = secret_keys
            .iter()
            .find(|key| lower.strip_prefix(*key).is_some_and(|rest| rest.starts_with('=')))
        {
            words.push(format!("{key}=<redacted>"));
        } else if let Some(key) = secret_keys.iter().find(|key| lower == **key) {
            pending_key = Some(*key);
            words.push(word.to_owned());
        } else {
            words.push(word.to_owned());
        }
    }
    if let Some(key) = pending_key {
        words.push(format!("{key}=<redacted>"));
    }
    words.join(" ")
}

fn extract_criteria(prompt: &str) -> Vec<WorkCriterion> {
    let mut criteria = Vec::new();
    for line in prompt.lines().map(str::trim) {
        let lower = line.to_ascii_lowercase();
        let Some(rest) = lower
            .strip_prefix("acceptance:")
            .or_else(|| lower.strip_prefix("criteria:"))
            .or_else(|| lower.strip_prefix("acceptance criteria:"))
        else {
            continue;
        };
        let original = line[line.len().saturating_sub(rest.len())..].trim();
        for (index, criterion) in
            original.split(';').map(str::trim).filter(|item| !item.is_empty()).enumerate()
        {
            let description = bounded(&redact_secret_assignments(criterion));
            criteria.push(WorkCriterion {
                key: bounded(&format!("criterion-{index}")),
                description,
                satisfied: false,
            });
            if criteria.len() == MAX_WORK_CRITERIA {
                return criteria;
            }
        }
    }
    criteria
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complex_prompt_creates_resumeable_work_units_without_raw_prompt_storage() {
        let mut state = WorkState::default();
        state.initialize_from_prompt(
            "Refactor the workspace across two crates, then verify it. token = do-not-store\nacceptance: tests pass; password = also-do-not-store; no unrelated files change",
        );
        assert_eq!(state.mode, WorkMode::Structured);
        assert!(state.objective.as_str().contains("<redacted>"));
        assert!(!state.objective.as_str().contains("do-not-store"));
        assert_eq!(state.acceptance_criteria.len(), 3);
        assert!(!state.acceptance_criteria[1].description.as_str().contains("also-do-not-store"));
        assert_eq!(state.active_subgoal.as_ref().map(BoundedText::as_str), Some("task"));
    }

    #[test]
    fn mutation_and_verification_advance_path_subgoals() {
        let mut state = WorkState::default();
        state.initialize_from_prompt("update multiple modules");
        assert!(state.add_subgoal("tests", "update the affected tests", &[]));
        assert_eq!(state.mode, WorkMode::Structured);
        state.record_mutation("write", &["src/lib.rs".to_owned()], 1);
        assert!(state.subgoals.iter().any(|subgoal| subgoal.id.as_str() == "path:src/lib.rs"));
        state.record_verification("cargo test", "crate", true, 1, &[], &["src/lib.rs".to_owned()]);
        assert!(state.subgoals.iter().any(|subgoal| subgoal.id.as_str() == "path:src/lib.rs"
            && subgoal.status == WorkStatus::Complete));
    }

    #[test]
    fn repeated_blockers_count_attempts_and_workspace_drift_stales_evidence() {
        let mut state = WorkState::default();
        state.initialize_from_prompt("fix module");
        state.record_blocker("write:src/lib.rs", "stale", "changed", 0);
        state.record_blocker("write:src/lib.rs", "stale", "changed again", 0);
        assert_eq!(state.blockers[0].attempts, 2);
        state.record_evidence("read", "src/lib.rs", "current", 0);
        state.invalidate_for_workspace_change(1);
        assert!(!state.evidence[0].valid);
    }
}
