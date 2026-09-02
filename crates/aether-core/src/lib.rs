#![deny(unsafe_code)]

//! Domain primitives shared by the AETHER Fx workspace.

pub mod cancellation;
pub mod command;
pub mod context;
pub mod decision;
pub mod error;
pub mod events;
pub mod ids;
pub mod limits;
pub mod model;
pub mod path;
pub mod permissions;
pub mod session;
pub mod tools;
pub mod work;
pub mod workflow;

pub use cancellation::CancellationFlag;
pub use command::{
    CommandClass, CommandEffects, MAX_COMMAND_ARGS, MAX_COMMAND_BYTES, MAX_COMMAND_FIELD_BYTES,
    MAX_COMMAND_PACKAGES, MAX_COMMAND_PATHS, analyze_command, classify_command,
};
pub use context::{
    CONTEXT_GUIDANCE, CompactToolSummary, ContextSnapshot, FileExcerpt, GitSnapshot, InspectedFile,
    LineRange, MAX_COMPACT_SUMMARY_BYTES, MAX_CONTEXT_FINGERPRINT_BYTES, MAX_CONTEXT_FINGERPRINTS,
    MAX_CONTEXT_ITEMS, MAX_CONTEXT_PATH_BYTES, MAX_CONTEXT_RENDER_BYTES,
    MAX_DIAGNOSTIC_DETAIL_BYTES, MAX_DIAGNOSTIC_ITEMS, MAX_EXCERPT_BYTES, MAX_FILE_EXCERPTS,
    MAX_GIT_SNAPSHOT_BYTES, MAX_INSPECTED_FILES, MAX_PROMPT_BYTES, MAX_SESSION_FILE_BYTES,
    MAX_SESSION_LINE_BYTES, MAX_SESSION_LINES, MAX_STORED_TOOL_SUMMARIES, MAX_STORED_TURNS,
    MAX_SUMMARY_PATHS, MAX_TASK_BYTES, MAX_USER_MODIFIED_FILES, ObservedFileState,
    compact_tool_result, extract_diagnostics, merge_line_ranges, payload_contains_secrets,
    persistable_continuation,
};
pub use decision::{
    DecisionAction, DecisionCandidate, DecisionEvidence, DecisionEvidenceKind, DecisionQuestion,
    DecisionState, MAX_DECISION_CANDIDATES, MAX_DECISION_EVIDENCE, MAX_DECISION_FIELD_BYTES,
    MAX_DECISION_QUESTIONS, MAX_DECISION_SCOPE, ProgressEffect,
};
pub use error::{CoreError, CoreResult};
pub use events::{AgentEvent, EventSequence, UsageMetadata};
pub use ids::{SessionId, StepId, ToolCallId, TurnId};
pub use limits::{BoundedText, DEFAULT_MAX_OUTPUT_BYTES, OutputLimit};
pub use model::{ModelEvent, ModelMessage, ModelRequest, ModelToolCall, OpaqueContinuation};
pub use path::{WorkspacePath, WorkspaceRoot};
pub use permissions::{
    PermissionClass, PermissionDecision, PermissionEngine, PermissionPolicy, PermissionRequest,
};
pub use session::{
    MIN_SUPPORTED_SESSION_SCHEMA_VERSION, SESSION_SCHEMA_VERSION, SessionLine, SessionRecord,
    TurnSnapshot,
};
pub use tools::{
    ActionClassification, ActionRequirements, EvidenceProvenance, ExecutionPermit,
    MAX_TOOL_FOOTPRINT_RESOURCES, PreparedAction, ToolDefinition, ToolEffect, ToolExecutionContext,
    ToolExecutor, ToolFootprint, ToolInvocation, ToolResource, ToolResult,
};
pub use work::{
    MAX_WORK_ACCEPTANCE_CRITERIA, MAX_WORK_BLOCKERS, MAX_WORK_EVIDENCE, MAX_WORK_FIELD_BYTES,
    MAX_WORK_MUTATIONS, MAX_WORK_OBJECTIVE_BYTES, MAX_WORK_RELATIONS, MAX_WORK_SUBGOALS,
    MAX_WORK_VERIFICATIONS, WorkBlocker, WorkCriterion, WorkEvidence, WorkMode, WorkMutation,
    WorkState, WorkStatus, WorkSubgoal, WorkVerification,
};
pub use workflow::{
    CompletionReview, MAX_COMPLETION_REVIEW_REASONS, MAX_WORKFLOW_DIAGNOSTICS,
    MAX_WORKFLOW_FAILURES, MAX_WORKFLOW_FIELD_BYTES, MAX_WORKFLOW_RECOVERY_PATHS,
    MAX_WORKFLOW_RELEVANT_FILES, MAX_WORKFLOW_VERIFICATIONS, RecoveryAction, RecoveryState,
    TurnDirectorState, TurnPhase, VerificationStatus, VerificationStep, WorkflowDiagnostic,
    WorkflowFailure, WorkflowFailureCategory, WorkflowPhase, WorkflowProgress, WorkflowState,
    WorkflowVerification,
};
