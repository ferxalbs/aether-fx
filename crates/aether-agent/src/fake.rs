use std::future::Future;
use std::pin::Pin;

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

/// A deterministic backend that never contacts the network.
#[derive(Clone, Debug)]
pub struct FakeBackend {
    response: String,
    tool_call: Option<FakeToolCall>,
}

impl FakeBackend {
    /// Construct a fake backend with a fixed response.
    pub fn new(response: impl Into<String>) -> Self {
        Self { response: response.into(), tool_call: None }
    }

    /// Configure one tool call before the final response step.
    pub fn with_tool_call(mut self, name: impl Into<String>, arguments: serde_json::Value) -> Self {
        self.tool_call = Some(FakeToolCall { name: name.into(), arguments });
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
        Box::pin(async move {
            let (sender, receiver) = mpsc::channel(8);
            let is_continuation = request.continuation.is_some();
            if !is_continuation {
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
