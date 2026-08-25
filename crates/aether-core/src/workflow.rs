use serde::{Deserialize, Serialize};

use crate::{BoundedText, DecisionState};

/// Maximum number of workspace-relative paths retained as relevant workflow context.
pub const MAX_WORKFLOW_RELEVANT_FILES: usize = 32;
/// Maximum number of unresolved workflow failures retained.
pub const MAX_WORKFLOW_FAILURES: usize = 8;
/// Maximum bytes retained for one workflow failure field.
pub const MAX_WORKFLOW_FIELD_BYTES: usize = 256;
/// Maximum number of focused verification commands retained.
pub const MAX_WORKFLOW_VERIFICATIONS: usize = 16;

/// Result retained for one planned verification command.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationStatus {
    #[default]
    Pending,
    Passed,
    Failed,
    Stale,
}

impl VerificationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Stale => "stale",
        }
    }
}

/// One deterministic, bounded verification step for the current workspace revision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerificationStep {
    pub command: BoundedText,
    pub scope: BoundedText,
    pub revision: u32,
    pub required: bool,
    pub status: VerificationStatus,
}

impl VerificationStep {
    pub fn new(command: impl AsRef<str>, scope: impl AsRef<str>, revision: u32) -> Self {
        Self {
            command: BoundedText::new(command, MAX_WORKFLOW_FIELD_BYTES),
            scope: BoundedText::new(scope, MAX_WORKFLOW_FIELD_BYTES),
            revision,
            required: true,
            status: VerificationStatus::Pending,
        }
    }
}

/// Deterministic phase of a repository-aware coding task.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPhase {
    /// Locate the relevant repository surface.
    #[default]
    Discover,
    /// Read and understand the relevant implementation.
    Inspect,
    /// Apply an authorized workspace mutation.
    Modify,
    /// Check the result of a successful mutation.
    Verify,
    /// The task has met its completion criteria.
    Complete,
}

impl WorkflowPhase {
    /// Return the stable compact spelling used in model context and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::Inspect => "inspect",
            Self::Modify => "modify",
            Self::Verify => "verify",
            Self::Complete => "complete",
        }
    }
}

/// Verification state for the current bounded work item.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowVerification {
    /// No mutation has established a verification requirement.
    #[default]
    NotRequired,
    /// A mutation succeeded and focused verification is pending.
    Pending,
    /// The latest focused verification passed.
    Passed,
    /// Focused verification was attempted and failed.
    Failed,
}

impl WorkflowVerification {
    /// Return the stable compact spelling used in model context.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::Pending => "pending",
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

/// Saturating progress counters retained for compact recovery and diagnostics.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowProgress {
    /// Successful discovery observations.
    pub discoveries: u16,
    /// Successful targeted file reads.
    pub inspections: u16,
    /// Successful workspace mutations.
    pub mutations: u16,
    /// Successful focused verification observations.
    pub verifications: u16,
    /// Tool results folded into the workflow.
    pub tool_results: u16,
}

impl WorkflowProgress {
    fn increment(counter: &mut u16) {
        *counter = counter.saturating_add(1);
    }

    pub(crate) fn note_discovery(&mut self) {
        Self::increment(&mut self.discoveries);
    }

    pub(crate) fn note_inspection(&mut self) {
        Self::increment(&mut self.inspections);
    }

    pub(crate) fn note_mutation(&mut self) {
        Self::increment(&mut self.mutations);
    }

    pub(crate) fn note_verification(&mut self) {
        Self::increment(&mut self.verifications);
    }

    pub fn note_tool_result(&mut self) {
        Self::increment(&mut self.tool_results);
    }
}

/// One bounded unresolved tool or verification failure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowFailure {
    /// Stable retry-matching key derived from the tool and target.
    pub key: BoundedText,
    /// Tool that produced the failure.
    pub tool: BoundedText,
    /// Stable failure category.
    pub code: BoundedText,
    /// Bounded actionable message.
    pub message: BoundedText,
}

impl WorkflowFailure {
    /// Construct a bounded failure record.
    pub fn new(
        key: impl AsRef<str>,
        tool: impl AsRef<str>,
        code: impl AsRef<str>,
        message: impl AsRef<str>,
    ) -> Self {
        Self {
            key: BoundedText::new(key, MAX_WORKFLOW_FIELD_BYTES),
            tool: BoundedText::new(tool, MAX_WORKFLOW_FIELD_BYTES),
            code: BoundedText::new(code, MAX_WORKFLOW_FIELD_BYTES),
            message: BoundedText::new(message, MAX_WORKFLOW_FIELD_BYTES),
        }
    }
}

/// Minimal bounded state used to drive deterministic repository workflow transitions.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowState {
    /// Current deterministic phase.
    pub phase: WorkflowPhase,
    /// Paths identified as relevant by successful discovery or tool observations.
    pub relevant_files: Vec<String>,
    /// Verification state for successful mutations in this task.
    pub verification: WorkflowVerification,
    /// Failures that still block completion.
    pub unresolved_failures: Vec<WorkflowFailure>,
    /// Bounded counters showing forward progress without retaining transcripts.
    pub progress: WorkflowProgress,
    /// Monotonic local workspace revision advanced by observed mutations.
    #[serde(default)]
    pub workspace_revision: u32,
    /// Ordered, deduplicated verification plan and its observed outcomes.
    #[serde(default)]
    pub verification_steps: Vec<VerificationStep>,
    /// Unified bounded evidence, candidate, scope, and next-action state.
    #[serde(default)]
    pub decision: DecisionState,
}

impl WorkflowState {
    /// Return a fresh workflow state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one relevant workspace-relative path and enter inspection when discovering.
    pub fn record_relevant_file(&mut self, path: &str) {
        let path = BoundedText::new(path, MAX_WORKFLOW_FIELD_BYTES);
        if path.is_truncated() || path.as_str().is_empty() {
            return;
        }
        if self.relevant_files.iter().any(|existing| existing == path.as_str()) {
            return;
        }
        let path = path.into_string();
        self.decision.record_evidence(
            crate::DecisionEvidenceKind::Discovery,
            &path,
            "relevant path observed",
            24,
            self.workspace_revision,
        );
        self.relevant_files.push(path);
        if self.relevant_files.len() > MAX_WORKFLOW_RELEVANT_FILES {
            self.relevant_files.remove(0);
        }
        self.progress.note_discovery();
        if matches!(self.phase, WorkflowPhase::Discover | WorkflowPhase::Complete) {
            self.phase = WorkflowPhase::Inspect;
        }
    }

    /// Record a successful targeted inspection.
    pub fn record_inspection(&mut self) {
        self.progress.note_inspection();
        self.decision.record_evidence(
            crate::DecisionEvidenceKind::Inspection,
            "",
            "targeted inspection completed",
            36,
            self.workspace_revision,
        );
        if !matches!(self.phase, WorkflowPhase::Verify) {
            self.phase = WorkflowPhase::Inspect;
        }
    }

    /// Record a mutation attempt before execution.
    pub fn begin_modification(&mut self) {
        if !matches!(self.phase, WorkflowPhase::Verify) {
            self.phase = WorkflowPhase::Modify;
        }
    }

    /// Record a successful mutation and require focused verification.
    pub fn record_mutation(&mut self) {
        self.progress.note_mutation();
        self.workspace_revision = self.workspace_revision.saturating_add(1);
        for step in &mut self.verification_steps {
            step.status = VerificationStatus::Stale;
        }
        self.phase = WorkflowPhase::Verify;
        self.verification = WorkflowVerification::Pending;
        self.decision.progress_confidence = self.decision.progress_confidence.min(80);
    }

    /// Add deterministic planning output, deduplicated within the current revision.
    pub fn set_verification_plan(&mut self, steps: impl IntoIterator<Item = VerificationStep>) {
        for mut step in steps {
            step.revision = self.workspace_revision;
            if step.command.as_str().is_empty()
                || self.verification_steps.iter().any(|existing| {
                    existing.revision == step.revision && existing.command == step.command
                })
            {
                continue;
            }
            self.verification_steps.push(step);
        }
        if self.verification_steps.len() > MAX_WORKFLOW_VERIFICATIONS {
            let excess = self.verification_steps.len() - MAX_WORKFLOW_VERIFICATIONS;
            self.verification_steps.drain(..excess);
        }
    }

    /// Record an exact planned command outcome for the current revision.
    pub fn record_verification_command(&mut self, command: &str, passed: bool) -> bool {
        let Some(step) = self.verification_steps.iter_mut().find(|step| {
            step.revision == self.workspace_revision
                && step.status == VerificationStatus::Pending
                && step.command.as_str() == command
        }) else {
            return false;
        };
        step.status = if passed { VerificationStatus::Passed } else { VerificationStatus::Failed };
        let all_passed = self
            .verification_steps
            .iter()
            .filter(|step| step.required && step.revision == self.workspace_revision)
            .all(|step| step.status == VerificationStatus::Passed);
        self.verification = if all_passed {
            self.progress.note_verification();
            WorkflowVerification::Passed
        } else if passed {
            WorkflowVerification::Pending
        } else {
            WorkflowVerification::Failed
        };
        self.phase = WorkflowPhase::Verify;
        true
    }

    /// Record the outcome of a focused verification attempt.
    pub fn record_verification(&mut self, passed: bool) {
        if passed {
            self.progress.note_verification();
            self.verification = WorkflowVerification::Passed;
            self.phase = WorkflowPhase::Verify;
        } else {
            self.verification = WorkflowVerification::Failed;
            self.phase = WorkflowPhase::Verify;
        }
    }

    /// Fold a bounded failure into the retryable failure set.
    pub fn record_failure(&mut self, failure: WorkflowFailure) {
        self.unresolved_failures.retain(|existing| existing.key != failure.key);
        self.unresolved_failures.push(failure);
        if self.unresolved_failures.len() > MAX_WORKFLOW_FAILURES {
            self.unresolved_failures.remove(0);
        }
        if self.phase == WorkflowPhase::Complete {
            self.phase = self.phase_when_incomplete();
        }
    }

    /// Clear the failure associated with a successful retry.
    pub fn clear_failure(&mut self, key: &str) {
        self.unresolved_failures.retain(|failure| failure.key.as_str() != key);
    }

    /// Reset stale discovery and verification state after workspace drift.
    pub fn reset_for_workspace_change(&mut self) {
        self.phase = WorkflowPhase::Discover;
        self.relevant_files.clear();
        self.verification = WorkflowVerification::NotRequired;
        for step in &mut self.verification_steps {
            step.status = VerificationStatus::Stale;
        }
        self.decision.reset_for_workspace_change();
    }

    /// Mark the workflow complete only when all bounded completion criteria hold.
    pub fn complete_if_ready(&mut self, modified: &[String]) -> bool {
        let verification_required = !modified.is_empty()
            || matches!(
                self.verification,
                WorkflowVerification::Pending | WorkflowVerification::Failed
            );
        let ready = !self.relevant_files.is_empty()
            && (!modified.is_empty() || self.progress.inspections > 0)
            && self.unresolved_failures.is_empty()
            && self
                .verification_steps
                .iter()
                .filter(|step| step.required && step.revision == self.workspace_revision)
                .all(|step| step.status == VerificationStatus::Passed)
            && (!verification_required || self.verification == WorkflowVerification::Passed);
        if ready {
            self.phase = WorkflowPhase::Complete;
        } else if self.phase == WorkflowPhase::Complete {
            self.phase = self.phase_when_incomplete();
        }
        ready
    }

    /// Return whether any unresolved failure blocks completion.
    pub fn has_unresolved_failures(&self) -> bool {
        !self.unresolved_failures.is_empty()
    }

    fn phase_when_incomplete(&self) -> WorkflowPhase {
        if matches!(self.verification, WorkflowVerification::Pending | WorkflowVerification::Failed)
        {
            WorkflowPhase::Verify
        } else if self.relevant_files.is_empty() {
            WorkflowPhase::Discover
        } else {
            WorkflowPhase::Inspect
        }
    }

    /// Enforce all persisted workflow bounds after deserialization.
    pub fn enforce_bounds(&mut self) {
        self.relevant_files.truncate(MAX_WORKFLOW_RELEVANT_FILES);
        for path in &mut self.relevant_files {
            *path = BoundedText::new(path.as_str(), MAX_WORKFLOW_FIELD_BYTES).into_string();
        }
        self.relevant_files.retain(|path| !path.is_empty());
        self.unresolved_failures.truncate(MAX_WORKFLOW_FAILURES);
        self.verification_steps.truncate(MAX_WORKFLOW_VERIFICATIONS);
        for step in &mut self.verification_steps {
            step.command = BoundedText::new(step.command.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            step.scope = BoundedText::new(step.scope.as_str(), MAX_WORKFLOW_FIELD_BYTES);
        }
        for failure in &mut self.unresolved_failures {
            failure.key = BoundedText::new(failure.key.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            failure.tool = BoundedText::new(failure.tool.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            failure.code = BoundedText::new(failure.code.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            failure.message = BoundedText::new(failure.message.as_str(), MAX_WORKFLOW_FIELD_BYTES);
        }
        self.decision.enforce_bounds();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_transitions_and_completion_are_deterministic() {
        let mut state = WorkflowState::new();
        state.record_relevant_file("src/lib.rs");
        assert_eq!(state.phase, WorkflowPhase::Inspect);
        state.record_inspection();
        state.begin_modification();
        assert_eq!(state.phase, WorkflowPhase::Modify);
        state.record_mutation();
        assert_eq!(state.verification, WorkflowVerification::Pending);
        assert!(!state.complete_if_ready(&["src/lib.rs".to_owned()]));
        state.record_verification(true);
        assert!(state.complete_if_ready(&["src/lib.rs".to_owned()]));
        assert_eq!(state.phase, WorkflowPhase::Complete);
    }

    #[test]
    fn workflow_failures_are_bounded_and_retryable() {
        let mut state = WorkflowState::new();
        for index in 0..(MAX_WORKFLOW_FAILURES + 2) {
            state.record_failure(WorkflowFailure::new(
                format!("key-{index}"),
                "read",
                "io",
                "failed",
            ));
        }
        assert_eq!(state.unresolved_failures.len(), MAX_WORKFLOW_FAILURES);
        state.clear_failure("key-3");
        assert!(!state.unresolved_failures.iter().any(|failure| failure.key.as_str() == "key-3"));
    }

    #[test]
    fn pending_verification_and_late_failures_never_complete() {
        let mut state = WorkflowState::new();
        state.record_relevant_file("src/lib.rs");
        state.record_inspection();
        assert!(state.complete_if_ready(&[]));
        assert_eq!(state.phase, WorkflowPhase::Complete);

        state.record_mutation();
        assert!(!state.complete_if_ready(&[]));
        assert_eq!(state.phase, WorkflowPhase::Verify);

        state.record_verification(true);
        assert!(state.complete_if_ready(&[]));
        state.record_failure(WorkflowFailure::new("read:src/lib.rs", "read", "io", "retry"));
        assert_ne!(state.phase, WorkflowPhase::Complete);
        assert!(!state.complete_if_ready(&[]));
    }

    #[test]
    fn discovery_without_inspection_cannot_complete() {
        let mut state = WorkflowState::new();
        state.record_relevant_file("src/lib.rs");
        assert!(!state.complete_if_ready(&[]));
        state.record_inspection();
        assert!(state.complete_if_ready(&[]));
    }

    #[test]
    fn verification_results_become_stale_after_later_mutation() {
        let mut state = WorkflowState::new();
        state.record_relevant_file("src/lib.rs");
        state.record_mutation();
        state.set_verification_plan([VerificationStep::new(
            "cargo test -p demo",
            "demo",
            state.workspace_revision,
        )]);
        assert!(state.record_verification_command("cargo test -p demo", true));
        assert!(state.complete_if_ready(&["src/lib.rs".into()]));

        state.record_mutation();
        assert_eq!(state.verification_steps[0].status, VerificationStatus::Stale);
        assert!(!state.complete_if_ready(&["src/lib.rs".into()]));
    }

    #[test]
    fn duplicate_commands_are_ignored_within_revision() {
        let mut state = WorkflowState::new();
        state.record_mutation();
        let step = VerificationStep::new("cargo check", "workspace", state.workspace_revision);
        state.set_verification_plan([step.clone(), step]);
        assert_eq!(state.verification_steps.len(), 1);
    }

    #[test]
    fn all_required_commands_must_pass_before_completion() {
        let mut state = WorkflowState::new();
        state.record_relevant_file("src/lib.rs");
        state.record_mutation();
        state.set_verification_plan([
            VerificationStep::new("cargo test", "crate", state.workspace_revision),
            VerificationStep::new("cargo check", "crate", state.workspace_revision),
        ]);
        assert!(state.record_verification_command("cargo test", true));
        assert!(!state.complete_if_ready(&["src/lib.rs".into()]));
        assert!(state.record_verification_command("cargo check", false));
        assert!(!state.complete_if_ready(&["src/lib.rs".into()]));
    }
}
