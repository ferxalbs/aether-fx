use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::{BoundedText, CancellationFlag, CoreError, CoreResult, PermissionClass, ToolCallId};

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

/// Authorization evidence bound to one exact model tool call.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecutionPermit {
    /// Exact backend/model call identity.
    pub call_id: ToolCallId,
    /// Exact model-visible tool name.
    pub tool: String,
    /// Permission class approved for this call.
    pub class: PermissionClass,
}

impl ExecutionPermit {
    /// Construct a permit at the runtime authorization boundary.
    pub fn new(call_id: ToolCallId, tool: impl Into<String>, class: PermissionClass) -> Self {
        Self { call_id, tool: tool.into(), class }
    }

    /// Validate that a permit cannot be reused across calls, tools, or classes.
    pub fn validate(
        &self,
        call_id: &ToolCallId,
        tool: &str,
        class: PermissionClass,
    ) -> CoreResult<()> {
        if &self.call_id != call_id || self.tool != tool || self.class != class {
            return Err(CoreError::PermissionDenied {
                operation: tool.to_owned(),
                reason: "execution permit is not bound to this call, tool, and permission class"
                    .to_owned(),
            });
        }
        Ok(())
    }
}

/// Runtime context explicitly passed to every tool execution.
#[derive(Clone, Debug)]
pub struct ToolExecutionContext {
    cancellation: CancellationFlag,
    permit: ExecutionPermit,
}

impl ToolExecutionContext {
    /// Construct a context from cooperative cancellation and authorization evidence.
    pub fn new(cancellation: CancellationFlag, permit: ExecutionPermit) -> Self {
        Self { cancellation, permit }
    }

    /// Return the shared cancellation flag.
    pub fn cancellation(&self) -> &CancellationFlag {
        &self.cancellation
    }

    /// Return the authorization permit.
    pub fn permit(&self) -> &ExecutionPermit {
        &self.permit
    }
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

    /// Build a structured permission request for calls that are not read-only.
    fn permission_request(&self, invocation: &ToolInvocation) -> Option<crate::PermissionRequest>;

    /// Execute one typed invocation and return a structured result.
    fn execute<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: ToolExecutionContext,
    ) -> ToolFuture<'a>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_permit_is_bound_to_call_tool_and_class() {
        let call_id = ToolCallId::new("call-1").unwrap();
        let permit =
            ExecutionPermit::new(call_id.clone(), "write", PermissionClass::WorkspaceWrite);
        assert!(permit.validate(&call_id, "write", PermissionClass::WorkspaceWrite).is_ok());
        assert!(
            permit
                .validate(
                    &ToolCallId::new("call-2").unwrap(),
                    "write",
                    PermissionClass::WorkspaceWrite
                )
                .is_err()
        );
        assert!(permit.validate(&call_id, "shell", PermissionClass::ProcessExecute).is_err());
        assert!(permit.validate(&call_id, "write", PermissionClass::ProcessExecute).is_err());
    }
}
