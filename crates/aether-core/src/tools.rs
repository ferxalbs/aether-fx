use std::{any::Any, fmt, future::Future, pin::Pin, sync::Arc};

use serde::{Deserialize, Serialize};

use crate::{
    BoundedText, CancellationFlag, CommandEffects, CoreError, CoreResult, PermissionClass,
    PermissionRequest, ToolCallId,
};

/// Maximum number of explicitly tracked resources in one tool footprint.
pub const MAX_TOOL_FOOTPRINT_RESOURCES: usize = 64;

/// The bounded semantic class shared by policy, scheduling, and loop guardrails.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ActionClassification {
    /// A read-only observation of the workspace or a process.
    Read,
    /// A workspace or process mutation.
    Mutation,
    /// A command whose result is used as verification evidence.
    Verification,
    /// An action whose effect cannot be classified more narrowly.
    #[default]
    Other,
}

/// The authority boundary required before a prepared action may execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionRequirements {
    /// Typed permission class required by the tool implementation.
    pub permission: PermissionClass,
    /// Whether an external/user permission decision is required.
    pub user_authorization: bool,
    /// Whether current workspace evidence and an exact precondition are required.
    pub current_workspace_evidence: bool,
}

/// Provenance of information used to produce a prepared action or observation.
///
/// Model-originated actions remain model-originated even when their arguments quote tool,
/// repository, or network output. Only an explicit user decision can satisfy user authority.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum EvidenceProvenance {
    User,
    Repository,
    ToolOutput,
    Network,
    #[default]
    Model,
}

/// One model tool call normalized once for all local decision and execution paths.
///
/// `normalized_input` is retained as the one structured representation crossing the generic
/// agent boundary. Tool implementations may attach their typed parse through `with_typed_input`
/// so execution does not deserialize the same JSON a second time.
pub struct PreparedAction {
    pub call_id: ToolCallId,
    pub tool: String,
    pub normalized_input: serde_json::Value,
    pub fingerprint: [u8; 16],
    pub effects: ToolFootprint,
    pub requirements: ActionRequirements,
    pub paths: Vec<String>,
    pub classification: ActionClassification,
    pub provenance: EvidenceProvenance,
    pub permission_request: Option<PermissionRequest>,
    pub command_effects: Option<CommandEffects>,
    typed_input: Option<Arc<dyn Any + Send + Sync>>,
}

impl fmt::Debug for PreparedAction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedAction")
            .field("call_id", &self.call_id)
            .field("tool", &self.tool)
            .field("normalized_input", &self.normalized_input)
            .field("fingerprint", &self.fingerprint)
            .field("effects", &self.effects)
            .field("requirements", &self.requirements)
            .field("paths", &self.paths)
            .field("classification", &self.classification)
            .field("provenance", &self.provenance)
            .field("permission_request", &self.permission_request)
            .field("command_effects", &self.command_effects)
            .finish_non_exhaustive()
    }
}

impl Clone for PreparedAction {
    fn clone(&self) -> Self {
        Self {
            call_id: self.call_id.clone(),
            tool: self.tool.clone(),
            normalized_input: self.normalized_input.clone(),
            fingerprint: self.fingerprint,
            effects: self.effects.clone(),
            requirements: self.requirements,
            paths: self.paths.clone(),
            classification: self.classification,
            provenance: self.provenance,
            permission_request: self.permission_request.clone(),
            command_effects: self.command_effects.clone(),
            typed_input: self.typed_input.clone(),
        }
    }
}

impl PreparedAction {
    /// Construct a conservative action when a tool does not provide typed preparation.
    pub fn fallback(invocation: ToolInvocation, permission: PermissionClass) -> Self {
        let fingerprint = fingerprint(&invocation.name, &invocation.input);
        let classification = match permission {
            PermissionClass::ReadOnly => ActionClassification::Read,
            _ => ActionClassification::Mutation,
        };
        let paths = fallback_paths(&invocation.input);
        Self {
            call_id: invocation.call_id,
            tool: invocation.name,
            normalized_input: invocation.input,
            fingerprint,
            effects: ToolFootprint::unknown(),
            requirements: ActionRequirements {
                permission,
                user_authorization: permission != PermissionClass::ReadOnly,
                current_workspace_evidence: classification == ActionClassification::Mutation,
            },
            paths,
            classification,
            provenance: EvidenceProvenance::Model,
            permission_request: None,
            command_effects: None,
            typed_input: None,
        }
    }

    /// Attach a provider-owned typed input without adding another structured serialization.
    pub fn with_typed_input<T: Any + Send + Sync>(mut self, input: T) -> Self {
        self.typed_input = Some(Arc::new(input));
        self
    }

    /// Borrow the typed input attached by a tool registry.
    pub fn typed_input<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.typed_input.as_ref()?.downcast_ref()
    }

    /// Reuse the registry-owned typed input for execution, moving it when uniquely owned and
    /// otherwise cloning only the already-parsed representation.
    pub fn into_typed_input<T: Any + Send + Sync + Clone>(self) -> Option<T> {
        let typed = self.typed_input?;
        Arc::downcast::<T>(typed)
            .ok()
            .map(|typed| Arc::try_unwrap(typed).unwrap_or_else(|typed| (*typed).clone()))
    }

    /// Convert the prepared action back to the compatibility invocation boundary.
    pub fn into_invocation(self) -> ToolInvocation {
        ToolInvocation { call_id: self.call_id, name: self.tool, input: self.normalized_input }
    }
}

fn fingerprint(tool: &str, value: &serde_json::Value) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(tool.as_bytes());
    hash_value(&mut hasher, value);
    let hash = hasher.finalize();
    let mut result = [0; 16];
    result.copy_from_slice(&hash.as_bytes()[..16]);
    result
}

fn fallback_paths(value: &serde_json::Value) -> Vec<String> {
    let mut paths = Vec::new();
    if let Some(path) = value.get("path").and_then(serde_json::Value::as_str) {
        paths.push(path.to_owned());
    }
    if let Some(files) = value.get("files").and_then(serde_json::Value::as_array) {
        for path in
            files.iter().filter_map(|file| file.get("path")).filter_map(|path| path.as_str())
        {
            if paths.len() == MAX_TOOL_FOOTPRINT_RESOURCES {
                break;
            }
            paths.push(path.to_owned());
        }
    }
    paths
}

fn hash_value(hasher: &mut blake3::Hasher, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => {
            hasher.update(b"null");
        }
        serde_json::Value::Bool(value) => {
            hasher.update(if *value { b"true" } else { b"false" });
        }
        serde_json::Value::Number(value) => {
            hasher.update(b"number");
            hash_bytes(hasher, value.to_string().as_bytes());
        }
        serde_json::Value::String(value) => {
            hasher.update(b"string");
            hash_bytes(hasher, value.as_bytes());
        }
        serde_json::Value::Array(values) => {
            hasher.update(b"array");
            hasher.update(&(values.len() as u64).to_le_bytes());
            for value in values {
                hash_value(hasher, value);
            }
        }
        serde_json::Value::Object(values) => {
            hasher.update(b"object");
            hasher.update(&(values.len() as u64).to_le_bytes());
            let mut keys = values.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            for key in keys {
                hash_bytes(hasher, key.as_bytes());
                hash_value(hasher, &values[key]);
            }
        }
    }
}

fn hash_bytes(hasher: &mut blake3::Hasher, bytes: &[u8]) {
    hasher.update(&(bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

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

    /// Normalize one model call once before policy, scheduling, or execution.
    fn prepare(&self, invocation: ToolInvocation) -> PreparedAction {
        let permission = self
            .definitions()
            .iter()
            .find(|definition| definition.name == invocation.name)
            .map_or(PermissionClass::ReadOnly, |definition| definition.permission);
        let effects = self.footprint(&invocation);
        let mut action = PreparedAction::fallback(invocation, permission);
        action.effects = effects;
        action.permission_request = self.permission_request(&ToolInvocation {
            call_id: action.call_id.clone(),
            name: action.tool.clone(),
            input: action.normalized_input.clone(),
        });
        action.requirements.user_authorization = action.permission_request.is_some();
        action
    }

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

    /// Execute a prepared action. Registries with typed preparation can override this to avoid
    /// deserializing `normalized_input` again; the default preserves the old executor contract.
    fn execute_prepared<'a>(
        &'a self,
        action: PreparedAction,
        context: ToolExecutionContext,
    ) -> ToolFuture<'a> {
        self.execute(
            ToolInvocation {
                call_id: action.call_id.clone(),
                name: action.tool.clone(),
                input: action.normalized_input.clone(),
            },
            context,
        )
    }
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
