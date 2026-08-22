use std::sync::Arc;

use aether_core::tools::ToolFuture;
use aether_core::{
    BoundedText, PermissionClass, PermissionEngine, PermissionRequest, ToolDefinition,
    ToolExecutionContext, ToolExecutor, ToolInvocation, ToolResult,
};
use serde_json::json;

use crate::{common::Workspace, find, git, list, patch, process, read, search, shell, write};

pub const TOOL_NAMES: [&str; 9] =
    ["read", "list", "find", "search", "write", "patch", "shell", "process", "git"];

pub struct ToolRegistry {
    workspace: Arc<Workspace>,
    definitions: Vec<ToolDefinition>,
}

impl ToolRegistry {
    pub fn new(workspace_root: impl AsRef<std::path::Path>) -> Result<Self, String> {
        Workspace::new(workspace_root).map(Self::with_workspace).map_err(|error| error.to_string())
    }

    pub fn with_workspace(workspace: Workspace) -> Self {
        Self { workspace: Arc::new(workspace), definitions: definitions() }
    }

    pub fn with_policy(
        workspace_root: impl AsRef<std::path::Path>,
        permissions: PermissionEngine,
    ) -> Result<Self, String> {
        let workspace = Workspace::new(workspace_root)
            .map_err(|error| error.to_string())?
            .with_policy(permissions);
        Ok(Self::with_workspace(workspace))
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    pub async fn dispatch(&self, invocation: ToolInvocation) -> ToolResult {
        let context = match self.context_for(&invocation) {
            Ok(value) => value,
            Err(error) => return self.workspace.result_error(invocation.call_id, error),
        };
        self.dispatch_with_context(invocation, context).await
    }

    /// Dispatch one invocation with explicit authorization and cancellation context.
    pub async fn dispatch_with_context(
        &self,
        invocation: ToolInvocation,
        context: ToolExecutionContext,
    ) -> ToolResult {
        let call_id = invocation.call_id.clone();
        match invocation.name.as_str() {
            "read" => read::execute(&self.workspace, call_id, invocation.input, context).await,
            "list" => list::execute(&self.workspace, call_id, invocation.input, context).await,
            "find" => find::execute(&self.workspace, call_id, invocation.input, context).await,
            "search" => search::execute(&self.workspace, call_id, invocation.input, context).await,
            "write" => write::execute(&self.workspace, call_id, invocation.input, context).await,
            "patch" => patch::execute(&self.workspace, call_id, invocation.input, context).await,
            "shell" => shell::execute(&self.workspace, call_id, invocation.input, context).await,
            "process" => {
                process::execute(&self.workspace, call_id, invocation.input, context).await
            }
            "git" => git::execute(&self.workspace, call_id, invocation.input, context).await,
            _ => ToolResult::failure(
                call_id,
                "unknown_tool",
                "tool is not in the v0.1 registry",
                false,
                self.workspace.max_output_bytes(),
            ),
        }
    }

    fn context_for(
        &self,
        invocation: &ToolInvocation,
    ) -> crate::common::ToolResultInternal<ToolExecutionContext> {
        let definition = self
            .definitions
            .iter()
            .find(|definition| definition.name == invocation.name)
            .ok_or_else(|| crate::common::ToolInternalError::Input("unknown tool".to_owned()))?;
        let request = self.permission_request(invocation).unwrap_or_else(|| PermissionRequest {
            call_id: invocation.call_id.clone(),
            tool: invocation.name.clone(),
            class: definition.permission,
            operation: invocation.name.clone(),
            target: None,
            details: serde_json::json!({}),
        });
        self.workspace.direct_context(&request)
    }

    /// Return the structured approval request for a mutating/executing invocation.
    pub fn permission_request(&self, invocation: &ToolInvocation) -> Option<PermissionRequest> {
        let definition =
            self.definitions.iter().find(|definition| definition.name == invocation.name)?;
        if definition.permission == PermissionClass::ReadOnly {
            return None;
        }
        let details = match invocation.name.as_str() {
            "write" => serde_json::from_value::<crate::WriteInput>(invocation.input.clone())
                .map(|input| {
                    serde_json::json!({
                        "path": bounded_detail(&input.path),
                        "bytes": input.content.len(),
                        "create_only": input.create_only.unwrap_or(false),
                        "precondition_present": input.expected_hash.is_some()
                    })
                })
                .unwrap_or_else(|_| serde_json::json!({"invalid_input": true})),
            "patch" => serde_json::from_value::<crate::PatchInput>(invocation.input.clone())
                .map(|input| {
                    serde_json::json!({
                        "files": input.files.iter().map(|file| serde_json::json!({
                            "path": bounded_detail(&file.path),
                            "hunks": file.hunks.len(),
                            "precondition_present": file.expected_hash.is_some()
                        })).collect::<Vec<_>>(),
                        "dry_run": input.dry_run.unwrap_or(false)
                    })
                })
                .unwrap_or_else(|_| serde_json::json!({"invalid_input": true})),
            "shell" => serde_json::from_value::<crate::ShellInput>(invocation.input.clone())
                .map(|input| {
                    serde_json::json!({
                        "program": bounded_detail(&input.program),
                        "arguments": bounded_arguments(input.args.unwrap_or_default()),
                        "cwd": bounded_detail(&input.cwd.unwrap_or_else(|| ".".to_owned()))
                    })
                })
                .unwrap_or_else(|_| serde_json::json!({"invalid_input": true})),
            "process" => serde_json::from_value::<crate::ProcessInput>(invocation.input.clone())
                .map(|input| {
                    serde_json::json!({
                        "operation": input.operation,
                        "program": input.program.as_deref().map(bounded_detail),
                        "arguments": bounded_arguments(input.args.unwrap_or_default()),
                        "cwd": input.cwd.as_deref().map(bounded_detail),
                        "process_id": input.process_id,
                        "bytes": input.data.as_ref().map_or(0, String::len)
                    })
                })
                .unwrap_or_else(|_| serde_json::json!({"invalid_input": true})),
            _ => serde_json::json!({"input": "structured details unavailable"}),
        };
        Some(PermissionRequest {
            call_id: invocation.call_id.clone(),
            tool: invocation.name.clone(),
            class: definition.permission,
            operation: invocation.name.clone(),
            target: details.get("path").and_then(serde_json::Value::as_str).map(str::to_owned),
            details,
        })
    }
}

fn bounded_detail(value: &str) -> String {
    BoundedText::new(value, 512).into_string()
}

fn bounded_arguments(arguments: Vec<String>) -> Vec<String> {
    let count = arguments.len();
    let mut bounded = arguments
        .into_iter()
        .take(32)
        .map(|argument| bounded_detail(&argument))
        .collect::<Vec<_>>();
    if count > 32 {
        bounded.push(format!("… {} additional arguments omitted", count - 32));
    }
    bounded
}

impl ToolExecutor for ToolRegistry {
    fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    fn permission_request(&self, invocation: &ToolInvocation) -> Option<PermissionRequest> {
        self.permission_request(invocation)
    }

    fn execute<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: ToolExecutionContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move { self.dispatch_with_context(invocation, context).await })
    }
}

fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "read".to_owned(),
            description: "Read one or more bounded workspace files with optional line ranges.".to_owned(),
            permission: PermissionClass::ReadOnly,
            input_schema: json!({
                "type": "object",
                "required": ["files"],
                "properties": {
                    "files": {"type": "array", "minItems": 1, "maxItems": 64, "items": {
                        "type": "object", "required": ["path"], "properties": {
                            "path": {"type": "string"},
                            "start_line": {"type": "integer", "minimum": 1},
                            "end_line": {"type": "integer", "minimum": 1}
                        }, "additionalProperties": false
                    }},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 4194304}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "list".to_owned(),
            description: "List bounded workspace directory entries with depth and ignore controls.".to_owned(),
            permission: PermissionClass::ReadOnly,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "depth": {"type": "integer", "minimum": 0, "maximum": 32},
                    "include_hidden": {"type": "boolean"},
                    "respect_ignore": {"type": "boolean"},
                    "max_entries": {"type": "integer", "minimum": 1, "maximum": 100000}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "find".to_owned(),
            description: "Find workspace paths by one or more glob patterns.".to_owned(),
            permission: PermissionClass::ReadOnly,
            input_schema: json!({
                "type": "object",
                "required": ["patterns"],
                "properties": {
                    "patterns": {"type": "array", "minItems": 1, "maxItems": 32, "items": {"type": "string"}},
                    "path": {"type": "string"},
                    "include_hidden": {"type": "boolean"},
                    "respect_ignore": {"type": "boolean"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 100000}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "search".to_owned(),
            description: "Search bounded workspace text using literal or regular-expression patterns.".to_owned(),
            permission: PermissionClass::ReadOnly,
            input_schema: json!({
                "type": "object",
                "required": ["patterns"],
                "properties": {
                    "patterns": {"type": "array", "minItems": 1, "maxItems": 32, "items": {"type": "string"}},
                    "path": {"type": "string"},
                    "globs": {"type": "array", "maxItems": 32, "items": {"type": "string"}},
                    "regex": {"type": "boolean"},
                    "case_insensitive": {"type": "boolean"},
                    "before_context": {"type": "integer", "minimum": 0, "maximum": 20},
                    "after_context": {"type": "integer", "minimum": 0, "maximum": 20},
                    "include_hidden": {"type": "boolean"},
                    "respect_ignore": {"type": "boolean"},
                    "max_results": {"type": "integer", "minimum": 1, "maximum": 100000}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "write".to_owned(),
            description: "Atomically create or replace one bounded workspace file with an optional hash precondition.".to_owned(),
            permission: PermissionClass::WorkspaceWrite,
            input_schema: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string", "maxLength": 4194304},
                    "expected_hash": {"type": "string"},
                    "create_only": {"type": "boolean"}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "patch".to_owned(),
            description: "Apply strict, preconditioned unified-style line hunks to multiple files.".to_owned(),
            permission: PermissionClass::WorkspaceWrite,
            input_schema: json!({
                "type": "object",
                "required": ["files"],
                "properties": {
                    "dry_run": {"type": "boolean"},
                    "files": {"type": "array", "minItems": 1, "maxItems": 64, "items": {
                        "type": "object", "required": ["path", "hunks"], "properties": {
                            "path": {"type": "string"},
                            "expected_hash": {"type": "string"},
                            "hunks": {"type": "array", "minItems": 1, "maxItems": 256, "items": {
                                "type": "object", "required": ["old_start", "old_count", "new_start", "new_count", "lines"],
                                "properties": {
                                    "old_start": {"type": "integer", "minimum": 1},
                                    "old_count": {"type": "integer", "minimum": 0},
                                    "new_start": {"type": "integer", "minimum": 1},
                                    "new_count": {"type": "integer", "minimum": 0},
                                    "lines": {"type": "array", "items": {"type": "string"}}
                                }, "additionalProperties": false
                            }}
                        }, "additionalProperties": false
                    }}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "shell".to_owned(),
            description: "Run one finite program by direct argv spawning with bounded output.".to_owned(),
            permission: PermissionClass::ProcessExecute,
            input_schema: json!({
                "type": "object",
                "required": ["program"],
                "properties": {
                    "program": {"type": "string"},
                    "args": {"type": "array", "maxItems": 256, "items": {"type": "string"}},
                    "cwd": {"type": "string"},
                    "max_output_bytes": {"type": "integer", "minimum": 1, "maximum": 4194304},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 600000}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "process".to_owned(),
            description: "Start and control bounded persistent direct-argv processes.".to_owned(),
            permission: PermissionClass::ProcessPersistent,
            input_schema: json!({
                "type": "object",
                "required": ["operation"],
                "properties": {
                    "operation": {"enum": ["start", "read", "write", "signal", "kill", "status"]},
                    "program": {"type": "string"},
                    "args": {"type": "array", "maxItems": 256, "items": {"type": "string"}},
                    "cwd": {"type": "string"},
                    "process_id": {"type": "integer", "minimum": 1},
                    "stream": {"enum": ["stdout", "stderr"]},
                    "data": {"type": "string", "maxLength": 65536},
                    "signal": {"enum": ["kill"]},
                    "max_bytes": {"type": "integer", "minimum": 1, "maximum": 65536},
                    "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 600000}
                },
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "git".to_owned(),
            description: "Run bounded read-only Git status, diff, show, log, and branch inspection.".to_owned(),
            permission: PermissionClass::ReadOnly,
            input_schema: json!({
                "type": "object",
                "required": ["operation"],
                "properties": {
                    "operation": {"enum": ["status", "diff", "show", "log", "branches"]},
                    "reference": {"type": "string"},
                    "path": {"type": "string"},
                    "staged": {"type": "boolean"},
                    "max_entries": {"type": "integer", "minimum": 1, "maximum": 1000}
                },
                "additionalProperties": false
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_exactly_nine_tools() {
        let definitions = definitions();
        assert_eq!(definitions.len(), TOOL_NAMES.len());
        assert_eq!(
            definitions.iter().map(|definition| definition.name.as_str()).collect::<Vec<_>>(),
            TOOL_NAMES
        );
    }
}
