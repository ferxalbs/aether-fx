use serde::{Deserialize, Serialize};

use crate::{AgentEvent, SessionId, StepId, ToolCallId, ToolDefinition, TurnId};

/// Opaque backend continuation data. AETHER never interprets reasoning content.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(transparent)]
pub struct OpaqueContinuation(pub serde_json::Value);

/// A backend-neutral model request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ModelRequest {
    /// Local session identity.
    pub session_id: SessionId,
    /// Local turn identity.
    pub turn_id: TurnId,
    /// Semantic model-step identity used for idempotency.
    pub step_id: StepId,
    /// Optional model selected by the user or Rainy catalog.
    pub model: Option<String>,
    /// Responses-style input assembled by the agent.
    pub input: serde_json::Value,
    /// Exact tool definitions visible to the model.
    pub tools: Vec<ToolDefinition>,
    /// Opaque continuation from the previous model step.
    pub continuation: Option<OpaqueContinuation>,
}

/// Events produced by a backend stream and consumed by the agent loop.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    /// A text delta.
    TextDelta { text: String },
    /// A fully assembled tool call.
    ToolCall { call_id: ToolCallId, name: String, arguments: serde_json::Value },
    /// Usage information.
    Usage { input_tokens: Option<u32>, output_tokens: Option<u32>, total_tokens: Option<u32> },
    /// A non-fatal backend warning.
    Warning { message: String },
    /// The model step completed and may carry opaque continuation data.
    Done { continuation: Option<OpaqueContinuation> },
}

impl From<ModelEvent> for Option<AgentEvent> {
    fn from(event: ModelEvent) -> Self {
        match event {
            ModelEvent::TextDelta { text } => Some(AgentEvent::TextDelta {
                text: crate::BoundedText::new(text, crate::DEFAULT_MAX_OUTPUT_BYTES),
            }),
            ModelEvent::Usage { input_tokens, output_tokens, total_tokens } => {
                Some(AgentEvent::Usage {
                    usage: crate::UsageMetadata { input_tokens, output_tokens, total_tokens },
                })
            }
            ModelEvent::Warning { message } => Some(AgentEvent::Warning {
                message: crate::BoundedText::new(message, crate::DEFAULT_MAX_OUTPUT_BYTES),
            }),
            ModelEvent::ToolCall { .. } | ModelEvent::Done { .. } => None,
        }
    }
}
