use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::{BoundedText, CancellationFlag, CoreError, CoreResult, PermissionClass, ToolCallId};

/// Maximum number of explicitly tracked resources in one tool footprint.
pub const MAX_TOOL_FOOTPRINT_RESOURCES: usize = 64;

/// A resource that a tool may inspect or mutate.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum ToolResource {
    /// A normalized path relative to the canonical workspace.
    WorkspacePath(String),
    /// The workspace as a whole, used for broad scans and mutations.
    Workspace,
    /// A process or process registry resource.
    Process(u64),
    /// A resource that cannot be narrowed safely.
    Global,
}

/// Access mode for one typed tool resource.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ToolEffect {
    /// Read-only access; concurrent reads are safe.
    Read(ToolResource),
    /// Mutating access; conflicts with reads and writes.
    Write(ToolResource),
    /// Access whose ordering cannot be relaxed.
    Exclusive(ToolResource),
}

/// Bounded typed resource footprint for one tool invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolFootprint {
    effects: Vec<ToolEffect>,
    unknown: bool,
}

impl ToolFootprint {
    /// Return an empty footprint for calls that will not execute.
    pub fn empty() -> Self {
        Self { effects: Vec::new(), unknown: false }
    }

    /// Return a conservative footprint for an unbounded or unknown operation.
    pub fn unknown() -> Self {
        Self { effects: Vec::new(), unknown: true }
    }

    /// Construct a footprint from a bounded effect list.
    pub fn from_effects(effects: Vec<ToolEffect>) -> Self {
        if effects.len() > MAX_TOOL_FOOTPRINT_RESOURCES {
            return Self::unknown();
        }
        Self { effects, unknown: false }
    }

    /// Return a read-only workspace path footprint.
    pub fn read_workspace(paths: impl IntoIterator<Item = String>) -> Self {
        Self::from_effects(
            paths
                .into_iter()
                .map(|path| ToolEffect::Read(ToolResource::WorkspacePath(path)))
                .collect(),
        )
    }

    /// Return an exclusive workspace footprint.
    pub fn exclusive_workspace() -> Self {
        Self::from_effects(vec![ToolEffect::Exclusive(ToolResource::Workspace)])
    }

    /// Return an exclusive global footprint.
    pub fn exclusive_global() -> Self {
        Self::from_effects(vec![ToolEffect::Exclusive(ToolResource::Global)])
    }

    /// Return the typed effects in this footprint.
    pub fn effects(&self) -> &[ToolEffect] {
        &self.effects
    }

    /// Return whether this footprint must be serialized conservatively.
    pub fn is_unknown(&self) -> bool {
        self.unknown
    }

    /// Return whether two footprints may execute concurrently.
    pub fn conflicts(&self, other: &Self) -> bool {
        if self.unknown || other.unknown {
            return true;
        }
        self.effects
            .iter()
            .any(|left| other.effects.iter().any(|right| effects_conflict(left, right)))
    }
}

fn effects_conflict(left: &ToolEffect, right: &ToolEffect) -> bool {
    let (left_resource, left_writes) = effect_parts(left);
    let (right_resource, right_writes) = effect_parts(right);
    if !left_writes && !right_writes {
        return false;
    }
    resources_overlap(left_resource, right_resource)
}

fn effect_parts(effect: &ToolEffect) -> (&ToolResource, bool) {
    match effect {
        ToolEffect::Read(resource) => (resource, false),
        ToolEffect::Write(resource) | ToolEffect::Exclusive(resource) => (resource, true),
    }
}

fn resources_overlap(left: &ToolResource, right: &ToolResource) -> bool {
    match (left, right) {
        (ToolResource::Global, _) | (_, ToolResource::Global) => true,
        (ToolResource::Workspace, _) | (_, ToolResource::Workspace) => true,
        (ToolResource::Process(left), ToolResource::Process(right)) => left == right,
        (ToolResource::WorkspacePath(left), ToolResource::WorkspacePath(right)) => {
            let left = std::path::Path::new(left);
            let right = std::path::Path::new(right);
            left == right || left.starts_with(right) || right.starts_with(left)
        }
        _ => false,
    }
}

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

    /// Describe the bounded resources touched by one invocation.
    ///
    /// Implementations that cannot prove a bounded footprint must return
    /// [`ToolFootprint::unknown`], which forces exclusive scheduling.
    fn footprint(&self, _: &ToolInvocation) -> ToolFootprint {
        ToolFootprint::unknown()
    }

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
