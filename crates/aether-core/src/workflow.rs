use serde::{Deserialize, Serialize};

use crate::{BoundedText, DecisionState, WorkState};

/// Maximum number of workspace-relative paths retained as relevant workflow context.
pub const MAX_WORKFLOW_RELEVANT_FILES: usize = 32;
/// Maximum number of unresolved workflow failures retained.
pub const MAX_WORKFLOW_FAILURES: usize = 8;
/// Maximum bytes retained for one workflow failure field.
pub const MAX_WORKFLOW_FIELD_BYTES: usize = 256;
/// Maximum number of focused verification commands retained.
pub const MAX_WORKFLOW_VERIFICATIONS: usize = 16;
/// Maximum structured diagnostics retained for one workflow failure.
pub const MAX_WORKFLOW_DIAGNOSTICS: usize = 8;
/// Maximum affected paths retained by recovery state.
pub const MAX_WORKFLOW_RECOVERY_PATHS: usize = 8;
/// Maximum completion-gate reasons retained in a bounded review.
pub const MAX_COMPLETION_REVIEW_REASONS: usize = 8;

/// Stable category for a failure that may require a changed hypothesis or action.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowFailureCategory {
    #[default]
    Tool,
    /// A compiler emitted a structured diagnostic that can guide repair.
    Compiler,
    /// A test or assertion failure needs a changed implementation hypothesis.
    Test,
    /// A patch could not be applied to its exact source context.
    PatchConflict,
    /// A direct command failed without being a verification result.
    Command,
    Verification,
    /// Verification completed but did not cover the required current scope.
    VerificationMismatch,
    StaleEvidence,
    Permission,
    Environment,
    Backend,
}

impl WorkflowFailureCategory {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Compiler => "compiler",
            Self::Test => "test",
            Self::PatchConflict => "patch_conflict",
            Self::Command => "command",
            Self::Verification => "verification",
            Self::VerificationMismatch => "verification_mismatch",
            Self::StaleEvidence => "stale_evidence",
            Self::Permission => "permission",
            Self::Environment => "environment",
            Self::Backend => "backend",
        }
    }
}

/// Deterministic action the runtime can take after observing a failure.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryAction {
    /// Refresh only the affected repository evidence before the next decision.
    RefreshEvidence,
    /// Retry an unchanged read-only observation when its preconditions remain valid.
    RetryEquivalent,
    /// Require a new model decision because the previous action is no longer safe.
    RequestNewDecision,
    /// Keep the failure as a blocker instead of guessing a repair.
    RecordBlocker,
    #[default]
    NoAction,
}

impl RecoveryAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RefreshEvidence => "refresh_evidence",
            Self::RetryEquivalent => "retry_equivalent",
            Self::RequestNewDecision => "request_new_decision",
            Self::RecordBlocker => "record_blocker",
            Self::NoAction => "no_action",
        }
    }
}

/// Bounded recovery bookkeeping owned by the runtime, not a second agent.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecoveryState {
    pub active: bool,
    pub category: WorkflowFailureCategory,
    pub action: RecoveryAction,
    pub attempts: u16,
    pub revision: u32,
    pub affected_paths: Vec<BoundedText>,
}

impl RecoveryState {
    pub fn begin(
        &mut self,
        category: WorkflowFailureCategory,
        action: RecoveryAction,
        revision: u32,
        affected_paths: &[String],
    ) {
        self.active = true;
        self.category = category;
        self.action = action;
        self.attempts = self.attempts.saturating_add(1);
        self.revision = revision;
        self.affected_paths = affected_paths
            .iter()
            .take(MAX_WORKFLOW_RECOVERY_PATHS)
            .map(|path| BoundedText::new(path, MAX_WORKFLOW_FIELD_BYTES))
            .filter(|path| !path.as_str().is_empty() && !path.is_truncated())
            .collect();
    }

    pub fn clear(&mut self) {
        self.active = false;
        self.action = RecoveryAction::NoAction;
        self.affected_paths.clear();
    }

    pub fn enforce_bounds(&mut self) {
        self.affected_paths.truncate(MAX_WORKFLOW_RECOVERY_PATHS);
        for path in &mut self.affected_paths {
            *path = BoundedText::new(path.as_str(), MAX_WORKFLOW_FIELD_BYTES);
        }
        self.affected_paths.retain(|path| !path.as_str().is_empty() && !path.is_truncated());
    }
}

/// Operational phase used to select evidence, recovery, verification, and completion behavior.
/// The legacy `WorkflowPhase` remains in the persisted contract for compatibility; this richer
/// director state is the runtime's authoritative phase model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    #[default]
    Discover,
    Understand,
    Implement,
    Recover,
    Verify,
    Review,
    Done,
}

impl TurnPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::Understand => "understand",
            Self::Implement => "implement",
            Self::Recover => "recover",
            Self::Verify => "verify",
            Self::Review => "review",
            Self::Done => "done",
        }
    }
}

/// Minimal durable state for the bounded turn director.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnDirectorState {
    pub phase: TurnPhase,
    pub revision: u32,
    pub transitions: u16,
    pub last_reason: BoundedText,
}

impl TurnDirectorState {
    fn transition(&mut self, phase: TurnPhase, revision: u32, reason: &str) {
        if self.phase != phase || self.revision != revision {
            self.transitions = self.transitions.saturating_add(1);
        }
        self.phase = phase;
        self.revision = revision;
        self.last_reason = BoundedText::new(reason, MAX_WORKFLOW_FIELD_BYTES);
    }

    pub fn enforce_bounds(&mut self) {
        self.last_reason = BoundedText::new(self.last_reason.as_str(), MAX_WORKFLOW_FIELD_BYTES);
    }
}

/// Bounded current-revision evidence used by the completion gate.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionReview {
    pub objective_present: bool,
    pub acceptance_criteria_satisfied: bool,
    pub work_complete: bool,
    pub blockers_clear: bool,
    pub evidence_current: bool,
    pub verification_current: bool,
    pub user_changes_preserved: bool,
    pub workspace_revision: u32,
    pub task_revision: u32,
    pub reasons: Vec<BoundedText>,
}

impl CompletionReview {
    pub fn ready(&self) -> bool {
        self.objective_present
            && self.acceptance_criteria_satisfied
            && self.work_complete
            && self.blockers_clear
            && self.evidence_current
            && self.verification_current
            && self.user_changes_preserved
    }

    fn add_reason(&mut self, reason: &str) {
        if self.reasons.len() < MAX_COMPLETION_REVIEW_REASONS {
            self.reasons.push(BoundedText::new(reason, MAX_WORKFLOW_FIELD_BYTES));
        }
    }
}

/// One bounded compiler/test diagnostic associated with a failed observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowDiagnostic {
    pub path: BoundedText,
    pub line: u32,
    pub column: Option<u32>,
    pub code: BoundedText,
    pub symbol: BoundedText,
    pub detail: BoundedText,
}

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
    /// Deterministic reactions that updated the active working set.
    #[serde(default)]
    pub automatic_evidence_reactions: u16,
    /// Bounded source windows hydrated from structured diagnostics.
    #[serde(default)]
    pub diagnostic_hydrations: u16,
    /// Changed regions refreshed after successful mutations.
    #[serde(default)]
    pub mutation_refreshes: u16,
    /// Inspected observations invalidated by live workspace drift.
    #[serde(default)]
    pub stale_invalidations: u16,
    /// Verification plans that contain a broader escalation after targeted checks.
    #[serde(default)]
    pub verification_escalations: u16,
    /// Completion attempts rejected by the deterministic review gate.
    #[serde(default)]
    pub completion_rejections: u16,
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

    pub fn note_automatic_evidence_reaction(&mut self) {
        Self::increment(&mut self.automatic_evidence_reactions);
    }

    pub fn note_diagnostic_hydration(&mut self) {
        Self::increment(&mut self.diagnostic_hydrations);
    }

    pub fn note_mutation_refresh(&mut self) {
        Self::increment(&mut self.mutation_refreshes);
    }

    pub fn note_stale_invalidation(&mut self) {
        Self::increment(&mut self.stale_invalidations);
    }

    pub fn note_verification_escalation(&mut self) {
        Self::increment(&mut self.verification_escalations);
    }

    pub fn note_completion_rejection(&mut self) {
        Self::increment(&mut self.completion_rejections);
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
    /// Typed category used to choose recovery rather than blindly retrying.
    #[serde(default)]
    pub category: WorkflowFailureCategory,
    /// Number of observations for this stable failure key.
    #[serde(default)]
    pub attempts: u16,
    /// Workspace revision at which this failure was observed.
    #[serde(default)]
    pub revision: u32,
    /// Bounded diagnostics extracted from compiler/test output.
    #[serde(default)]
    pub diagnostics: Vec<WorkflowDiagnostic>,
}

impl WorkflowFailure {
    /// Construct a bounded failure record.
    pub fn new(
        key: impl AsRef<str>,
        tool: impl AsRef<str>,
        code: impl AsRef<str>,
        message: impl AsRef<str>,
    ) -> Self {
        Self::new_at_revision(
            key,
            tool,
            code,
            message,
            WorkflowFailureCategory::Tool,
            0,
            Vec::new(),
        )
    }

    /// Construct a failure with recovery category, revision, and structured diagnostics.
    pub fn new_at_revision(
        key: impl AsRef<str>,
        tool: impl AsRef<str>,
        code: impl AsRef<str>,
        message: impl AsRef<str>,
        category: WorkflowFailureCategory,
        revision: u32,
        diagnostics: Vec<WorkflowDiagnostic>,
    ) -> Self {
        Self {
            key: BoundedText::new(key, MAX_WORKFLOW_FIELD_BYTES),
            tool: BoundedText::new(tool, MAX_WORKFLOW_FIELD_BYTES),
            code: BoundedText::new(code, MAX_WORKFLOW_FIELD_BYTES),
            message: BoundedText::new(message, MAX_WORKFLOW_FIELD_BYTES),
            category,
            attempts: 1,
            revision,
            diagnostics,
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
    /// Compact long-horizon objective, subgoals, evidence, and recovery state.
    #[serde(default)]
    pub work: WorkState,
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
    /// Runtime-owned recovery state for the latest actionable failure.
    #[serde(default)]
    pub recovery: RecoveryState,
    /// Rich operational phase used by the adaptive turn director.
    #[serde(default)]
    pub director: TurnDirectorState,
    /// Task/constraint revision that completion evidence must match.
    #[serde(default)]
    pub task_revision: u32,
    /// Task revision at which the last completion review succeeded.
    #[serde(default)]
    pub completed_task_revision: Option<u32>,
}

impl WorkflowState {
    /// Return a fresh workflow state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Recompute the operational phase from observed repository state.
    ///
    /// This is deliberately a recommendation/state projection, not a scripted workflow. The
    /// model remains free to choose any safe action; the runtime only records which evidence and
    /// recovery behavior is currently appropriate.
    pub fn sync_director(&mut self, modified: &[String], stale_evidence: bool) {
        let (phase, reason) = if self.recovery.active || !self.unresolved_failures.is_empty() {
            (TurnPhase::Recover, "an actionable failure needs bounded recovery")
        } else if self.completed_task_revision == Some(self.task_revision)
            && self.phase == WorkflowPhase::Complete
        {
            (TurnPhase::Done, "current-revision completion review passed")
        } else if stale_evidence {
            (TurnPhase::Recover, "observed evidence is stale and must be refreshed")
        } else if matches!(
            self.verification,
            WorkflowVerification::Pending | WorkflowVerification::Failed
        ) {
            (TurnPhase::Verify, "current mutations require scoped verification")
        } else if self.phase == WorkflowPhase::Verify && !modified.is_empty() {
            (TurnPhase::Review, "verification passed; review the affected scope before finishing")
        } else if self.phase == WorkflowPhase::Modify {
            (TurnPhase::Implement, "an authorized implementation action is in progress")
        } else if self.relevant_files.is_empty() {
            (TurnPhase::Discover, "no relevant repository path has been observed")
        } else {
            (TurnPhase::Understand, "relevant repository evidence is the next decision input")
        };
        self.director.transition(phase, self.workspace_revision, reason);
    }

    /// Return the bounded, deterministic evidence used by the completion gate.
    pub fn completion_review(
        &self,
        modified: &[String],
        evidence_current: bool,
        user_changes_preserved: bool,
    ) -> CompletionReview {
        let verification_required = !modified.is_empty()
            || matches!(
                self.verification,
                WorkflowVerification::Pending | WorkflowVerification::Failed
            );
        let mut review = CompletionReview {
            objective_present: !self.work.objective.as_str().is_empty(),
            acceptance_criteria_satisfied: self.work.criteria_satisfied(),
            work_complete: !self.work.blocks_completion(),
            blockers_clear: self.unresolved_failures.is_empty() && self.work.blockers.is_empty(),
            evidence_current,
            verification_current: self.verification_steps.iter().filter(|step| step.required).all(
                |step| {
                    step.revision == self.workspace_revision
                        && step.status == VerificationStatus::Passed
                },
            ) && self
                .work
                .verifications
                .iter()
                .filter(|verification| verification.revision == self.workspace_revision)
                .all(|verification| verification.status == VerificationStatus::Passed)
                && (!verification_required || self.verification == WorkflowVerification::Passed),
            user_changes_preserved,
            workspace_revision: self.workspace_revision,
            task_revision: self.task_revision,
            reasons: Vec::new(),
        };
        if !review.objective_present {
            review.add_reason("objective is missing from the current task state");
        }
        if !review.acceptance_criteria_satisfied {
            review.add_reason("one or more acceptance criteria remain unsatisfied");
        }
        if !review.work_complete {
            review.add_reason("structured work contains unresolved subgoals or blockers");
        }
        if !review.blockers_clear {
            review.add_reason("unresolved runtime blockers remain");
        }
        if !review.evidence_current {
            review.add_reason("required repository evidence is stale");
        }
        if !review.verification_current {
            review.add_reason(
                "verification is missing, failed, stale, or scoped to an older revision",
            );
        }
        if !review.user_changes_preserved {
            review.add_reason("agent-owned paths overlap user-owned dirty paths");
        }
        review
    }

    /// Clear a recovery state after a fresh targeted observation has restored its preconditions.
    pub fn clear_recovery(&mut self) {
        self.recovery.clear();
        if self.unresolved_failures.is_empty() {
            self.sync_director(&[], false);
        }
    }

    /// Record one repository-derived relevant path and enter inspection when discovering.
    pub fn record_relevant_file(&mut self, path: &str) {
        self.record_relevant_file_with_provenance(crate::EvidenceProvenance::Repository, path);
    }

    /// Record a relevant path with its explicit evidence origin.
    pub fn record_relevant_file_with_provenance(
        &mut self,
        provenance: crate::EvidenceProvenance,
        path: &str,
    ) {
        let path = BoundedText::new(path, MAX_WORKFLOW_FIELD_BYTES);
        if path.is_truncated() || path.as_str().is_empty() {
            return;
        }
        if self.relevant_files.iter().any(|existing| existing == path.as_str()) {
            return;
        }
        let path = path.into_string();
        self.decision.record_evidence_with_provenance(
            provenance,
            crate::DecisionEvidenceKind::Discovery,
            &path,
            "relevant path observed",
            24,
            self.workspace_revision,
        );
        self.work.record_evidence(
            "discovery",
            &path,
            "relevant path observed",
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
        self.sync_director(&[], false);
    }

    /// Record a successful targeted repository inspection.
    pub fn record_inspection(&mut self) {
        self.record_inspection_with_provenance(crate::EvidenceProvenance::Repository);
    }

    /// Record an inspection with its explicit evidence origin.
    pub fn record_inspection_with_provenance(&mut self, provenance: crate::EvidenceProvenance) {
        self.progress.note_inspection();
        self.decision.record_evidence_with_provenance(
            provenance,
            crate::DecisionEvidenceKind::Inspection,
            "",
            "targeted inspection completed",
            36,
            self.workspace_revision,
        );
        self.work.record_evidence(
            "inspection",
            "",
            "targeted inspection completed",
            self.workspace_revision,
        );
        if !matches!(self.phase, WorkflowPhase::Verify) {
            self.phase = WorkflowPhase::Inspect;
        }
        self.sync_director(&[], false);
    }

    /// Record a mutation attempt before execution.
    pub fn begin_modification(&mut self) {
        if !matches!(self.phase, WorkflowPhase::Verify) {
            self.phase = WorkflowPhase::Modify;
        }
        self.sync_director(&[], false);
    }

    /// Record a successful mutation and require focused verification.
    pub fn record_mutation(&mut self) {
        self.record_mutation_scope("mutation", &[]);
    }

    /// Record a successful mutation and the exact affected paths.
    pub fn record_mutation_scope(&mut self, operation: &str, paths: &[String]) {
        self.progress.note_mutation();
        self.workspace_revision = self.workspace_revision.saturating_add(1);
        for step in &mut self.verification_steps {
            step.status = VerificationStatus::Stale;
        }
        self.phase = WorkflowPhase::Verify;
        self.verification = WorkflowVerification::Pending;
        self.completed_task_revision = None;
        self.decision.progress_confidence = self.decision.progress_confidence.min(80);
        self.work.record_mutation(operation, paths, self.workspace_revision);
        self.sync_director(paths, false);
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
        self.sync_director(&[], false);
        true
    }

    /// Record the outcome of a focused verification attempt.
    pub fn record_verification(&mut self, passed: bool) {
        self.record_verification_scope(passed, "", &[], Vec::new());
    }

    /// Record verification with the command scope and structured diagnostics.
    pub fn record_verification_scope(
        &mut self,
        passed: bool,
        command: &str,
        affected_paths: &[String],
        diagnostics: Vec<WorkflowDiagnostic>,
    ) {
        if passed {
            self.progress.note_verification();
            self.verification = WorkflowVerification::Passed;
            self.phase = WorkflowPhase::Verify;
        } else {
            self.verification = WorkflowVerification::Failed;
            self.phase = WorkflowPhase::Verify;
        }
        self.work.record_verification(
            command,
            self.decision.verification_scope.as_str(),
            passed,
            self.workspace_revision,
            &diagnostics
                .iter()
                .map(|diagnostic| diagnostic.detail.as_str().to_owned())
                .collect::<Vec<_>>(),
            affected_paths,
        );
        self.sync_director(affected_paths, false);
    }

    /// Retain verification evidence without changing the aggregate verification gate. This is
    /// used when one command in a multi-command plan passes while later commands remain pending.
    pub fn record_work_verification(
        &mut self,
        passed: bool,
        command: &str,
        affected_paths: &[String],
        diagnostics: Vec<WorkflowDiagnostic>,
    ) {
        self.work.record_verification(
            command,
            self.decision.verification_scope.as_str(),
            passed,
            self.workspace_revision,
            &diagnostics
                .iter()
                .map(|diagnostic| diagnostic.detail.as_str().to_owned())
                .collect::<Vec<_>>(),
            affected_paths,
        );
        self.sync_director(affected_paths, false);
    }

    /// Fold a bounded failure into the retryable failure set.
    pub fn record_failure(&mut self, failure: WorkflowFailure) {
        let category = failure.category;
        let revision = failure.revision;
        let affected_paths = failure
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.path.as_str().to_owned())
            .collect::<Vec<_>>();
        if let Some(existing) =
            self.unresolved_failures.iter_mut().find(|existing| existing.key == failure.key)
        {
            existing.tool = failure.tool;
            existing.code = failure.code;
            existing.message = failure.message;
            existing.category = failure.category;
            existing.revision = failure.revision;
            existing.diagnostics = failure.diagnostics;
            existing.attempts = existing.attempts.saturating_add(1);
            self.work.record_blocker(
                existing.key.as_str(),
                existing.category.as_str(),
                existing.message.as_str(),
                existing.revision,
            );
            self.decision.record_question(
                format!("failure:{}", existing.key.as_str()),
                format!(
                    "recover from {} failure: {}",
                    existing.category.as_str(),
                    existing.message.as_str()
                ),
            );
        } else {
            self.work.record_blocker(
                failure.key.as_str(),
                failure.category.as_str(),
                failure.message.as_str(),
                failure.revision,
            );
            self.decision.record_question(
                format!("failure:{}", failure.key.as_str()),
                format!(
                    "recover from {} failure: {}",
                    failure.category.as_str(),
                    failure.message.as_str()
                ),
            );
            self.unresolved_failures.push(failure);
        }
        if self.unresolved_failures.len() > MAX_WORKFLOW_FAILURES {
            self.unresolved_failures.remove(0);
        }
        if self.phase == WorkflowPhase::Complete {
            self.phase = self.phase_when_incomplete();
        }
        let action = match category {
            WorkflowFailureCategory::StaleEvidence => RecoveryAction::RefreshEvidence,
            WorkflowFailureCategory::Permission | WorkflowFailureCategory::Environment => {
                RecoveryAction::RecordBlocker
            }
            WorkflowFailureCategory::PatchConflict
            | WorkflowFailureCategory::Compiler
            | WorkflowFailureCategory::Test
            | WorkflowFailureCategory::Verification
            | WorkflowFailureCategory::VerificationMismatch
            | WorkflowFailureCategory::Command
            | WorkflowFailureCategory::Tool
            | WorkflowFailureCategory::Backend => RecoveryAction::RequestNewDecision,
        };
        self.recovery.begin(category, action, revision, &affected_paths);
        self.sync_director(&[], false);
    }

    /// Clear the failure associated with a successful retry.
    pub fn clear_failure(&mut self, key: &str) {
        self.unresolved_failures.retain(|failure| failure.key.as_str() != key);
        self.work.clear_blocker(key);
        self.decision.clear_question(&format!("failure:{key}"));
        if self.unresolved_failures.is_empty() {
            self.recovery.clear();
        }
        self.sync_director(&[], false);
    }

    /// Reset stale discovery and verification state after workspace drift.
    pub fn reset_for_workspace_change(&mut self) {
        let affected_paths = self.relevant_files.clone();
        self.phase = WorkflowPhase::Discover;
        self.relevant_files.clear();
        self.verification = WorkflowVerification::NotRequired;
        for step in &mut self.verification_steps {
            step.status = VerificationStatus::Stale;
        }
        self.decision.reset_for_workspace_change();
        self.work.invalidate_for_workspace_change(self.workspace_revision);
        self.recovery.begin(
            WorkflowFailureCategory::StaleEvidence,
            RecoveryAction::RefreshEvidence,
            self.workspace_revision,
            &affected_paths,
        );
        self.sync_director(&[], true);
    }

    /// Mark the workflow complete only when all bounded completion criteria hold.
    pub fn complete_if_ready(&mut self, modified: &[String]) -> bool {
        self.complete_if_ready_with_evidence(modified, true, true)
    }

    /// Run the bounded current-revision completion review with live context facts.
    pub fn complete_if_ready_with_evidence(
        &mut self,
        modified: &[String],
        evidence_current: bool,
        user_changes_preserved: bool,
    ) -> bool {
        self.director.transition(
            TurnPhase::Review,
            self.workspace_revision,
            "runtime completion review is evaluating current evidence",
        );
        let ready = !self.relevant_files.is_empty()
            && (!modified.is_empty() || self.progress.inspections > 0)
            && self.completion_review(modified, evidence_current, user_changes_preserved).ready();
        if ready {
            self.phase = WorkflowPhase::Complete;
            self.completed_task_revision = Some(self.task_revision);
            self.director.transition(
                TurnPhase::Done,
                self.workspace_revision,
                "current-revision completion review passed",
            );
        } else if self.phase == WorkflowPhase::Complete {
            self.phase = self.phase_when_incomplete();
            self.completed_task_revision = None;
            self.sync_director(modified, !evidence_current);
        }
        if !ready {
            self.progress.note_completion_rejection();
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
            failure.diagnostics.truncate(MAX_WORKFLOW_DIAGNOSTICS);
            for diagnostic in &mut failure.diagnostics {
                diagnostic.path =
                    BoundedText::new(diagnostic.path.as_str(), MAX_WORKFLOW_FIELD_BYTES);
                diagnostic.code =
                    BoundedText::new(diagnostic.code.as_str(), MAX_WORKFLOW_FIELD_BYTES);
                diagnostic.symbol =
                    BoundedText::new(diagnostic.symbol.as_str(), MAX_WORKFLOW_FIELD_BYTES);
                diagnostic.detail =
                    BoundedText::new(diagnostic.detail.as_str(), MAX_WORKFLOW_FIELD_BYTES);
            }
        }
        self.decision.enforce_bounds();
        self.work.enforce_bounds();
        self.recovery.enforce_bounds();
        self.director.enforce_bounds();
        self.task_revision = self.task_revision.max(self.work.task_revision);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_transitions_and_completion_are_deterministic() {
        let mut state = WorkflowState::new();
        state.work.initialize_from_prompt("update the library");
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
        state.work.initialize_from_prompt("update the library");
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
        state.work.initialize_from_prompt("inspect the library");
        state.record_relevant_file("src/lib.rs");
        assert!(!state.complete_if_ready(&[]));
        state.record_inspection();
        assert!(state.complete_if_ready(&[]));
    }

    #[test]
    fn verification_results_become_stale_after_later_mutation() {
        let mut state = WorkflowState::new();
        state.work.initialize_from_prompt("update the library");
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
        state.work.initialize_from_prompt("update the library");
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

    #[test]
    fn turn_director_tracks_recovery_review_and_done_from_observations() {
        let mut state = WorkflowState::new();
        state.work.initialize_from_prompt("repair the library");
        assert_eq!(state.director.phase, TurnPhase::Discover);
        state.record_relevant_file("src/lib.rs");
        assert_eq!(state.director.phase, TurnPhase::Understand);
        state.record_inspection();
        state.begin_modification();
        assert_eq!(state.director.phase, TurnPhase::Implement);
        state.record_mutation_scope("write", &["src/lib.rs".to_owned()]);
        assert_eq!(state.director.phase, TurnPhase::Verify);
        state.record_failure(WorkflowFailure::new_at_revision(
            "verify",
            "shell",
            "verification_failed",
            "test failed",
            WorkflowFailureCategory::Test,
            state.workspace_revision,
            Vec::new(),
        ));
        assert_eq!(state.director.phase, TurnPhase::Recover);
        assert_eq!(state.recovery.action, RecoveryAction::RequestNewDecision);
        state.clear_failure("verify");
        state.record_verification(true);
        assert!(state.complete_if_ready(&["src/lib.rs".to_owned()]));
        assert_eq!(state.director.phase, TurnPhase::Done);
    }

    #[test]
    fn completion_review_fails_closed_for_stale_revision_and_user_overlap() {
        let mut state = WorkflowState::new();
        state.record_relevant_file("src/lib.rs");
        state.record_inspection();
        assert!(!state.completion_review(&[], false, true).ready());
        assert!(!state.completion_review(&[], true, false).ready());
        let review = state.completion_review(&[], true, true);
        assert!(!review.objective_present);
        assert!(review.reasons.iter().any(|reason| reason.as_str().contains("objective")));
        assert!(!state.complete_if_ready_with_evidence(&[], true, false));
        assert_eq!(state.director.phase, TurnPhase::Review);
    }
}
