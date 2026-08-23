#![deny(unsafe_code)]

//! Domain primitives shared by the AETHER Fx workspace.

pub mod cancellation;
pub mod context;
pub mod error;
pub mod events;
pub mod ids;
pub mod limits;
pub mod model;
pub mod path;
pub mod permissions;
pub mod session;
pub mod tools;

pub use cancellation::CancellationFlag;
pub use context::{
    CONTEXT_GUIDANCE, CompactToolSummary, ContextSnapshot, FileExcerpt, GitSnapshot, InspectedFile,
    LineRange, MAX_COMPACT_SUMMARY_BYTES, MAX_CONTEXT_ITEMS, MAX_CONTEXT_RENDER_BYTES,
    MAX_EXCERPT_BYTES, MAX_FILE_EXCERPTS, MAX_GIT_SNAPSHOT_BYTES, MAX_INSPECTED_FILES,
    MAX_PROMPT_BYTES, MAX_SESSION_FILE_BYTES, MAX_SESSION_LINE_BYTES, MAX_SESSION_LINES,
    MAX_STORED_TOOL_SUMMARIES, MAX_STORED_TURNS, MAX_SUMMARY_PATHS, MAX_TASK_BYTES,
    ObservedFileState, compact_tool_result, merge_line_ranges, payload_contains_secrets,
    persistable_continuation,
};
pub use error::{CoreError, CoreResult};
pub use events::{AgentEvent, EventSequence, UsageMetadata};
pub use ids::{SessionId, StepId, ToolCallId, TurnId};
pub use limits::{BoundedText, DEFAULT_MAX_OUTPUT_BYTES, OutputLimit};
pub use model::{ModelEvent, ModelRequest, OpaqueContinuation};
pub use path::{WorkspacePath, WorkspaceRoot};
pub use permissions::{
    PermissionClass, PermissionDecision, PermissionEngine, PermissionPolicy, PermissionRequest,
};
pub use session::{SESSION_SCHEMA_VERSION, SessionLine, SessionRecord};
pub use tools::{
    ExecutionPermit, ToolDefinition, ToolExecutionContext, ToolExecutor, ToolInvocation, ToolResult,
};
