use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::{BoundedText, PermissionClass, ToolCallId};

/// A concise model-visible tool schema.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolDefinition {
    /// Exact model-visible name.
    pub name: String,
    /// Short description suitable for model context.
    pub description: String,
    /// JSON Schema for the typed input.
    pub input_schema: serde_json::Value,
    /// Permission category used by the execution policy.
    pub permission: PermissionClass,
}

/// A typed invocation handed to a tool executor.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolInvocation {
    /// Model/backend call identifier.
    pub call_id: ToolCallId,
    /// Exact registry name.
    pub name: String,
    /// JSON input decoded by the tool implementation into a typed struct.
    pub input: serde_json::Value,
}

/// A structured tool error safe to show to a model.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolErrorInfo {
    /// Stable category.
    pub code: String,
    /// Bounded, secret-free message.
    pub message: String,
    /// Whether a semantic retry might be appropriate.
    pub retryable: bool,
}

/// A bounded, structured tool result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ToolResult {
    /// Original call identifier.
    pub call_id: ToolCallId,
    /// Whether the operation completed successfully.
    pub ok: bool,
    /// Bounded human/model-readable output.
    pub output: BoundedText,
    /// Optional structured payload, also expected to be bounded by the producer.
    pub data: Option<serde_json::Value>,
    /// Structured error when `ok` is false.
    pub error: Option<ToolErrorInfo>,
}

impl ToolResult {
    /// Construct a successful result from a JSON value with a byte ceiling.
    pub fn success_json(call_id: ToolCallId, value: serde_json::Value, max_bytes: usize) -> Self {
        match serde_json::to_string(&value) {
            Ok(serialized) => {
                let output = BoundedText::new(&serialized, max_bytes);
                Self {
                    call_id,
                    ok: true,
                    data: (!output.is_truncated()).then_some(value),
                    output,
                    error: None,
                }
            }
            Err(error) => Self::failure(
                call_id,
                "serialization",
                format!("tool output serialization failed: {error}"),
                false,
                max_bytes,
            ),
        }
    }

    /// Construct a successful textual result.
    pub fn success_text(call_id: ToolCallId, text: impl AsRef<str>, max_bytes: usize) -> Self {
        Self {
            call_id,
            ok: true,
            output: BoundedText::new(text, max_bytes),
            data: None,
            error: None,
        }
    }

    /// Construct a structured failure.
    pub fn failure(
        call_id: ToolCallId,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        max_bytes: usize,
    ) -> Self {
        let message = message.into();
        Self {
            call_id,
            ok: false,
            output: BoundedText::new(&message, max_bytes),
            data: None,
            error: Some(ToolErrorInfo {
                code: code.into(),
                message: BoundedText::new(&message, max_bytes).into_string(),
                retryable,
            }),
        }
    }
}

/// Boxed future used instead of `async-trait` at the foundational boundary.
pub type ToolFuture<'a> = Pin<Box<dyn Future<Output = ToolResult> + Send + 'a>>;

/// Async tool execution boundary owned by the composition root.
pub trait ToolExecutor: Send + Sync {
    /// Return the exact model-visible registry.
    fn definitions(&self) -> &[ToolDefinition];

    /// Execute one typed invocation and return a structured result.
    fn execute<'a>(&'a self, invocation: ToolInvocation) -> ToolFuture<'a>;
}
