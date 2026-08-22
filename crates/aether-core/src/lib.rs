#![deny(unsafe_code)]

//! Domain primitives shared by the AETHER Fx workspace.

pub mod cancellation;
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
