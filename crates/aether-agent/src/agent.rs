use std::sync::Arc;

use aether_core::{
    AgentEvent, BoundedText, DEFAULT_MAX_OUTPUT_BYTES, ModelEvent, ModelRequest,
    OpaqueContinuation, SessionId, StepId, ToolCallId, ToolExecutor, ToolInvocation, TurnId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::{BackendError, CancellationToken, ModelBackend};

/// A user-requested agent turn.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentRequest {
    /// Local session identity.
    pub session_id: SessionId,
    /// Local turn identity.
    pub turn_id: TurnId,
    /// User prompt.
    pub prompt: String,
    /// Optional opaque model identifier.
    pub model: Option<String>,
    /// Maximum model/tool continuation steps.
    pub max_steps: u16,
}

impl AgentRequest {
    /// Construct a bounded-default request.
    pub fn new(session_id: SessionId, turn_id: TurnId, prompt: impl Into<String>) -> Self {
        Self { session_id, turn_id, prompt: prompt.into(), model: None, max_steps: 16 }
    }
}

/// A completed turn summary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentRunResult {
    /// Bounded accumulated assistant text.
    pub text: BoundedText,
    /// Number of semantic model steps used.
    pub steps: u16,
    /// Whether the turn ended due to cancellation.
    pub cancelled: bool,
}

/// Agent loop failures are typed and secret-free.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Backend mapping or transport failure.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// The event consumer disappeared.
    #[error("agent event consumer closed")]
    EventChannelClosed,
    /// Tool execution returned an internal failure.
    #[error("tool execution failed: {0}")]
    Tool(String),
    /// The turn exceeded its semantic step cap.
    #[error("agent step limit exceeded")]
    StepLimit,
    /// The user cancelled the turn.
    #[error("agent turn cancelled")]
    Cancelled,
}

/// Backend-neutral agent orchestration.
pub struct Agent<B, T> {
    backend: Arc<B>,
    tools: Arc<T>,
}

impl<B, T> Agent<B, T>
where
    B: ModelBackend + 'static,
    T: ToolExecutor + 'static,
{
    /// Compose one backend and one tool executor.
    pub fn new(backend: B, tools: T) -> Self {
        Self { backend: Arc::new(backend), tools: Arc::new(tools) }
    }

    /// Execute one bounded turn, emitting ordered events into a bounded channel.
    pub async fn run(
        &self,
        request: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        cancellation: CancellationToken,
    ) -> Result<AgentRunResult, AgentError> {
        let mut accumulated = String::new();
        let mut input = json!([{"type":"message","role":"user","content":request.prompt}]);
        let mut continuation: Option<OpaqueContinuation> = None;
        let mut steps = 0_u16;

        loop {
            if cancellation.is_cancelled() {
                let _ = events
                    .send(AgentEvent::Error {
                        message: BoundedText::new("agent turn cancelled", DEFAULT_MAX_OUTPUT_BYTES),
                    })
                    .await;
                return Err(AgentError::Cancelled);
            }
            if steps >= request.max_steps {
                return Err(AgentError::StepLimit);
            }
            steps = steps.saturating_add(1);
            let step_id = StepId::new(format!("{}.{}", request.turn_id, steps))
                .map_err(|error| AgentError::Tool(error.to_string()))?;
            let model_request = ModelRequest {
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                step_id,
                model: request.model.clone(),
                input: input.clone(),
                tools: self.tools.definitions().to_vec(),
                continuation: continuation.clone(),
            };
            let backend_future = self.backend.stream_step(model_request);
            let mut stream = tokio::select! {
                _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                result = backend_future => result?,
            };
            let mut tool_calls = Vec::new();
            loop {
                let event = tokio::select! {
                    _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                    event = stream.recv() => event,
                };
                let Some(event) = event else {
                    break;
                };
                let event = event?;
                match event {
                    ModelEvent::TextDelta { text } => {
                        append_bounded(&mut accumulated, &text, DEFAULT_MAX_OUTPUT_BYTES);
                        send_event(
                            &events,
                            AgentEvent::TextDelta {
                                text: BoundedText::new(text, DEFAULT_MAX_OUTPUT_BYTES),
                            },
                        )
                        .await?;
                    }
                    ModelEvent::ToolCall { call_id, name, arguments } => {
                        let permission = self
                            .tools
                            .definitions()
                            .iter()
                            .find(|definition| definition.name == name)
                            .map(|definition| definition.permission.to_string())
                            .unwrap_or_else(|| "unknown".to_owned());
                        send_event(
                            &events,
                            AgentEvent::ToolStarted {
                                call_id: call_id.clone(),
                                name: name.clone(),
                                permission,
                                operation: name.clone(),
                                step_id: None,
                            },
                        )
                        .await?;
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelEvent::Usage { input_tokens, output_tokens, total_tokens } => {
                        send_event(
                            &events,
                            AgentEvent::Usage {
                                usage: aether_core::UsageMetadata {
                                    input_tokens,
                                    output_tokens,
                                    total_tokens,
                                },
                            },
                        )
                        .await?;
                    }
                    ModelEvent::Warning { message } => {
                        send_event(
                            &events,
                            AgentEvent::Warning {
                                message: BoundedText::new(message, DEFAULT_MAX_OUTPUT_BYTES),
                            },
                        )
                        .await?;
                    }
                    ModelEvent::Done { continuation: next } => {
                        continuation = next;
                    }
                }
            }

            if tool_calls.is_empty() {
                send_event(&events, AgentEvent::Done).await?;
                return Ok(AgentRunResult {
                    text: BoundedText::new(accumulated, DEFAULT_MAX_OUTPUT_BYTES),
                    steps,
                    cancelled: false,
                });
            }

            let mut outputs = Vec::with_capacity(tool_calls.len());
            for (call_id, name, arguments) in tool_calls {
                if cancellation.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                let tool_future = self.tools.execute(ToolInvocation {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    input: arguments,
                });
                let result = tokio::select! {
                    _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                    result = tool_future => result,
                };
                send_event(
                    &events,
                    AgentEvent::ToolOutput {
                        call_id: call_id.clone(),
                        output: result.output.clone(),
                    },
                )
                .await?;
                send_event(
                    &events,
                    AgentEvent::ToolFinished { call_id: call_id.clone(), ok: result.ok },
                )
                .await?;
                outputs.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id.as_str(),
                    "output": result.output.as_str(),
                    "ok": result.ok,
                }));
            }
            input = serde_json::Value::Array(outputs);
        }
    }
}

async fn send_event(
    sender: &mpsc::Sender<AgentEvent>,
    event: AgentEvent,
) -> Result<(), AgentError> {
    sender.send(event).await.map_err(|_| AgentError::EventChannelClosed)
}

fn append_bounded(target: &mut String, value: &str, max_bytes: usize) {
    if target.len() >= max_bytes {
        return;
    }
    let remaining = max_bytes - target.len();
    let bounded = BoundedText::new(value, remaining);
    target.push_str(bounded.as_str());
}

#[allow(dead_code)]
fn _tool_call_id_type_is_used(id: ToolCallId) -> String {
    id.into_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::tools::ToolFuture;
    use aether_core::{PermissionClass, ToolDefinition, ToolResult};

    struct FakeTools {
        definitions: Vec<ToolDefinition>,
    }

    impl ToolExecutor for FakeTools {
        fn definitions(&self) -> &[ToolDefinition] {
            &self.definitions
        }

        fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a> {
            Box::pin(
                async move { ToolResult::success_text(invocation.call_id, "tool output", 1024) },
            )
        }
    }

    fn tools() -> FakeTools {
        FakeTools {
            definitions: vec![ToolDefinition {
                name: "read".to_owned(),
                description: "test read".to_owned(),
                input_schema: json!({"type": "object"}),
                permission: PermissionClass::ReadOnly,
            }],
        }
    }

    #[tokio::test]
    async fn fake_backend_proves_tool_continuation_and_event_order() {
        let backend =
            crate::FakeBackend::new("completed").with_tool_call("read", json!({"files": []}));
        let agent = Agent::new(backend, tools());
        let session = SessionId::new("session-test").unwrap();
        let turn = TurnId::new("turn-test").unwrap();
        let mut request = AgentRequest::new(session, turn, "inspect");
        request.max_steps = 4;
        let (sender, mut receiver) = mpsc::channel(32);
        let result = agent.run(request, sender, CancellationToken::new()).await.unwrap();
        let mut kinds = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            kinds.push(event.kind());
        }
        assert_eq!(result.text.as_str(), "completed");
        assert_eq!(
            kinds,
            vec!["tool_started", "tool_output", "tool_finished", "text_delta", "done"]
        );
    }

    #[tokio::test]
    async fn cancellation_is_first_class() {
        let agent = Agent::new(crate::FakeBackend::default(), tools());
        let request = AgentRequest::new(
            SessionId::new("session-cancel").unwrap(),
            TurnId::new("turn-cancel").unwrap(),
            "stop",
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (sender, _receiver) = mpsc::channel(4);
        assert!(matches!(
            agent.run(request, sender, cancellation).await,
            Err(AgentError::Cancelled)
        ));
    }
}
