use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use aether_core::{ModelEvent, ModelRequest, ToolCallId};
use serde_json::json;
use tokio::sync::mpsc;

use crate::{BackendError, BackendFuture, BackendStream, ModelBackend, ModelCatalogItem};

/// A deterministic tool call used by tests and local development.
#[derive(Clone, Debug)]
pub struct FakeToolCall {
    /// Registry name to invoke.
    pub name: String,
    /// Typed JSON arguments.
    pub arguments: serde_json::Value,
}

/// One deterministic model response, useful for offline end-to-end evaluation.
#[derive(Clone, Debug, Default)]
pub struct FakeStep {
    /// Optional assistant text emitted before the tool calls.
    pub text: Option<String>,
    /// Ordered calls requested by this model step.
    pub tool_calls: Vec<FakeToolCall>,
}

/// A deterministic backend that never contacts the network.
#[derive(Clone, Debug)]
pub struct FakeBackend {
    response: String,
    tool_call: Option<FakeToolCall>,
    steps: Option<Arc<Vec<FakeStep>>>,
}

impl FakeBackend {
    /// Construct a fake backend with a fixed response.
    pub fn new(response: impl Into<String>) -> Self {
        Self { response: response.into(), tool_call: None, steps: None }
    }

    /// Configure one tool call before the final response step.
    pub fn with_tool_call(mut self, name: impl Into<String>, arguments: serde_json::Value) -> Self {
        self.tool_call = Some(FakeToolCall { name: name.into(), arguments });
        self
    }

    /// Configure a complete deterministic multi-step model trace.
    pub fn with_steps(mut self, steps: Vec<FakeStep>) -> Self {
        self.steps = Some(Arc::new(steps));
        self
    }
}

impl Default for FakeBackend {
    fn default() -> Self {
        Self::new("fake backend ready")
    }
}

impl ModelBackend for FakeBackend {
    fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a> {
        let response = self.response.clone();
        let tool_call = self.tool_call.clone();
        let steps = self.steps.clone();
        Box::pin(async move {
            let (sender, receiver) = mpsc::channel(8);
            if let Some(steps) = steps {
                let index = request
                    .continuation
                    .as_ref()
                    .and_then(|continuation| continuation.0.get("fake_step"))
                    .and_then(serde_json::Value::as_str)
                    .and_then(|step| step.rsplit('.').next())
                    .and_then(|step| step.parse::<usize>().ok())
                    .unwrap_or(0);
                if let Some(step) = steps.get(index) {
                    if let Some(text) = &step.text {
                        sender
                            .send(Ok(ModelEvent::TextDelta { text: text.clone() }))
                            .await
                            .map_err(|_| BackendError::Cancelled)?;
                    }
                    for (call_index, call) in step.tool_calls.iter().enumerate() {
                        let call_id = ToolCallId::new(format!("fake-call-{index}-{call_index}"))
                            .map_err(|error| BackendError::Mapping {
                                message: error.to_string(),
                            })?;
                        sender
                            .send(Ok(ModelEvent::ToolCall {
                                call_id,
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                            }))
                            .await
                            .map_err(|_| BackendError::Cancelled)?;
                    }
                }
            } else if request.continuation.is_none() {
                if let Some(call) = tool_call {
                    let call_id = ToolCallId::new("fake-call-1")
                        .map_err(|error| BackendError::Mapping { message: error.to_string() })?;
                    sender
                        .send(Ok(ModelEvent::ToolCall {
                            call_id,
                            name: call.name,
                            arguments: call.arguments,
                        }))
                        .await
                        .map_err(|_| BackendError::Cancelled)?;
                } else {
                    sender
                        .send(Ok(ModelEvent::TextDelta { text: response }))
                        .await
                        .map_err(|_| BackendError::Cancelled)?;
                }
            } else {
                sender
                    .send(Ok(ModelEvent::TextDelta { text: response }))
                    .await
                    .map_err(|_| BackendError::Cancelled)?;
            }
            sender
                .send(Ok(ModelEvent::Done {
                    continuation: Some(aether_core::OpaqueContinuation(json!({
                        "fake_step": request.step_id.as_str()
                    }))),
                }))
                .await
                .map_err(|_| BackendError::Cancelled)?;
            Ok(receiver)
        })
    }

    fn discover_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelCatalogItem>, BackendError>> + Send + 'a>>
    {
        Box::pin(async {
            Ok(vec![ModelCatalogItem {
                id: "fake".to_owned(),
                display_name: Some("Deterministic fake backend".to_owned()),
                context_window: None,
                capabilities: json!({"streaming": true, "tools": true}),
            }])
        })
    }
}

/// Keep the compiler aware that the fake receiver is intentionally bounded.
#[allow(dead_code)]
fn _stream_type_is_send(
    stream: BackendStream,
) -> Pin<Box<dyn Future<Output = BackendStream> + Send>> {
    Box::pin(async move { stream })
}
