use serde::{Deserialize, Serialize};

use crate::{BoundedText, PermissionDecision, PermissionRequest, StepId, ToolCallId};

/// Monotonic ordering metadata attached to a stream event.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventSequence {
    /// Sequence number within a single agent turn.
    pub value: u64,
}

impl EventSequence {
    /// Create a sequence value.
    pub const fn new(value: u64) -> Self {
        Self { value }
    }
}

/// Compact usage information from a model backend.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct UsageMetadata {
    /// Input token count, when provided.
    pub input_tokens: Option<u32>,
    /// Output token count, when provided.
    pub output_tokens: Option<u32>,
    /// Total token count, when provided.
    pub total_tokens: Option<u32>,
}

/// Events emitted by the agent and consumed by the terminal/session layer.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A bounded text delta from the model.
    TextDelta { text: BoundedText },
    /// A tool invocation has been accepted for execution.
    ToolStarted {
        call_id: ToolCallId,
        name: String,
        permission: String,
        operation: String,
        step_id: Option<StepId>,
    },
    /// A mutating/executing tool needs a terminal-neutral user decision.
    PermissionRequested { request: PermissionRequest },
    /// The permission decision associated with a request.
    PermissionResolved { call_id: ToolCallId, decision: PermissionDecision },
    /// Bounded structured/text output from a tool.
    ToolOutput { call_id: ToolCallId, output: BoundedText },
    /// A tool invocation finished.
    ToolFinished { call_id: ToolCallId, ok: bool },
    /// Usage metadata from the backend.
    Usage { usage: UsageMetadata },
    /// Non-fatal diagnostic.
    Warning { message: BoundedText },
    /// User-visible error without a backtrace.
    Error { message: BoundedText },
    /// The turn reached a terminal state.
    Done,
}

impl AgentEvent {
    /// Return a compact event name for diagnostics and benchmarks.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::TextDelta { .. } => "text_delta",
            Self::ToolStarted { .. } => "tool_started",
            Self::PermissionRequested { .. } => "permission_requested",
            Self::PermissionResolved { .. } => "permission_resolved",
            Self::ToolOutput { .. } => "tool_output",
            Self::ToolFinished { .. } => "tool_finished",
            Self::Usage { .. } => "usage",
            Self::Warning { .. } => "warning",
            Self::Error { .. } => "error",
            Self::Done => "done",
        }
    }
}
