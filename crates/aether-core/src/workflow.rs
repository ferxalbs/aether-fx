use serde::{Deserialize, Serialize};

use crate::BoundedText;

/// Maximum number of workspace-relative paths retained as relevant workflow context.
pub const MAX_WORKFLOW_RELEVANT_FILES: usize = 32;
/// Maximum number of unresolved workflow failures retained.
pub const MAX_WORKFLOW_FAILURES: usize = 8;
/// Maximum bytes retained for one workflow failure field.
pub const MAX_WORKFLOW_FIELD_BYTES: usize = 256;

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
        self.relevant_files.push(path.into_string());
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
        self.phase = WorkflowPhase::Verify;
        self.verification = WorkflowVerification::Pending;
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
    }

    /// Mark the workflow complete only when all bounded completion criteria hold.
    pub fn complete_if_ready(&mut self, modified: &[String]) -> bool {
        let verification_required = !modified.is_empty()
            || matches!(
                self.verification,
                WorkflowVerification::Pending | WorkflowVerification::Failed
            );
        let ready = !self.relevant_files.is_empty()
            && self.unresolved_failures.is_empty()
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
        for failure in &mut self.unresolved_failures {
            failure.key = BoundedText::new(failure.key.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            failure.tool = BoundedText::new(failure.tool.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            failure.code = BoundedText::new(failure.code.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            failure.message = BoundedText::new(failure.message.as_str(), MAX_WORKFLOW_FIELD_BYTES);
        }
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
}
