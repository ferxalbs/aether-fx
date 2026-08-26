use std::sync::{Arc, OnceLock};

use aether_core::tools::ToolFuture;
use aether_core::{
    ActionClassification, ActionRequirements, BoundedText, PermissionClass, PermissionEngine,
    PermissionRequest, PreparedAction, ToolDefinition, ToolEffect, ToolExecutionContext,
    ToolExecutor, ToolFootprint, ToolInvocation, ToolResource, ToolResult, WorkspacePath,
    analyze_command,
};
use serde_json::json;

use crate::{common::Workspace, find, git, list, patch, process, read, search, shell, write};

pub const TOOL_NAMES: [&str; 9] =
    ["read", "list", "find", "search", "write", "patch", "shell", "process", "git"];

pub struct ToolRegistry {
    workspace: Arc<Workspace>,
    definitions: &'static [ToolDefinition],
}

#[derive(Clone)]
enum PreparedToolInput {
    Read(crate::ReadInput),
    List(crate::ListInput),
    Find(crate::FindInput),
    Search(crate::SearchInput),
    Write(crate::WriteInput),
    Patch(crate::PatchInput),
    Shell(crate::ShellInput),
    Process(crate::ProcessInput),
    Git(crate::GitInput),
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
        self.definitions
    }

    /// Parse and classify a model call exactly once at the tool boundary.
    pub fn prepare_action(&self, invocation: ToolInvocation) -> PreparedAction {
        let permission = self.definition_permission(&invocation.name);
        let mut action = PreparedAction::fallback(invocation, permission);
        let parsed = parse_prepared_input(&action.tool, &action.normalized_input);
        if let Some(parsed) = parsed {
            let command_effects = command_effects_for_input(&parsed);
            action.permission_request =
                permission_request_for_input(&action.call_id, &action.tool, permission, &parsed);
            let classification = command_effects
                .as_ref()
                .map_or_else(|| classification_for_input(&parsed), classification_for_command);
            action.command_effects = command_effects;
            action.effects = action.command_effects.as_ref().map_or_else(
                || footprint_for_input(&parsed),
                aether_core::CommandEffects::footprint,
            );
            action.paths = action.command_effects.as_ref().map_or_else(
                || paths_for_input(&parsed),
                |effects| effects.paths.iter().chain(effects.manifests.iter()).cloned().collect(),
            );
            action.classification = classification;
            action.requirements = ActionRequirements {
                permission,
                user_authorization: action.permission_request.is_some(),
                current_workspace_evidence: classification == ActionClassification::Mutation,
            };
            return action.with_typed_input(parsed);
        }

        // Invalid input remains conservative and gets the old bounded diagnostic path.
        action.permission_request = self.permission_request(&ToolInvocation {
            call_id: action.call_id.clone(),
            name: action.tool.clone(),
            input: action.normalized_input.clone(),
        });
        action.requirements.user_authorization = action.permission_request.is_some();
        action
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

    async fn dispatch_prepared(
        &self,
        action: PreparedAction,
        context: ToolExecutionContext,
    ) -> ToolResult {
        let call_id = action.call_id.clone();
        let fallback = ToolInvocation {
            call_id: action.call_id.clone(),
            name: action.tool.clone(),
            input: action.normalized_input.clone(),
        };
        let Some(parsed) = action.into_typed_input::<PreparedToolInput>() else {
            return self.dispatch_with_context(fallback, context).await;
        };
        match parsed {
            PreparedToolInput::Read(input) => {
                read::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::List(input) => {
                list::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::Find(input) => {
                find::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::Search(input) => {
                search::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::Write(input) => {
                write::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::Patch(input) => {
                patch::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::Shell(input) => {
                shell::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::Process(input) => {
                process::execute_parsed(&self.workspace, call_id, input, context).await
            }
            PreparedToolInput::Git(input) => {
                git::execute_parsed(&self.workspace, call_id, input, context).await
            }
        }
    }

    fn definition_permission(&self, name: &str) -> PermissionClass {
        self.definitions
            .iter()
            .find(|definition| definition.name == name)
            .map_or(PermissionClass::ReadOnly, |definition| definition.permission)
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

fn parse_prepared_input(tool: &str, input: &serde_json::Value) -> Option<PreparedToolInput> {
    match tool {
        "read" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Read),
        "list" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::List),
        "find" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Find),
        "search" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Search),
        "write" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Write),
        "patch" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Patch),
        "shell" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Shell),
        "process" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Process),
        "git" => serde_json::from_value(input.clone()).ok().map(PreparedToolInput::Git),
        _ => None,
    }
}

fn permission_request_for_input(
    call_id: &aether_core::ToolCallId,
    tool: &str,
    permission: PermissionClass,
    input: &PreparedToolInput,
) -> Option<PermissionRequest> {
    if permission == PermissionClass::ReadOnly {
        return None;
    }
    let details = match input {
        PreparedToolInput::Write(input) => serde_json::json!({
            "path": bounded_detail(&input.path),
            "bytes": input.content.len(),
            "create_only": input.create_only.unwrap_or(false),
            "precondition_present": input.expected_hash.is_some()
        }),
        PreparedToolInput::Patch(input) => serde_json::json!({
            "files": input.files.iter().map(|file| serde_json::json!({
                "path": bounded_detail(&file.path),
                "hunks": file.hunks.len(),
                "precondition_present": file.expected_hash.is_some()
            })).collect::<Vec<_>>(),
            "dry_run": input.dry_run.unwrap_or(false)
        }),
        PreparedToolInput::Shell(input) => serde_json::json!({
            "program": bounded_detail(&input.program),
            "arguments": bounded_arguments(input.args.clone().unwrap_or_default()),
            "cwd": bounded_detail(input.cwd.as_deref().unwrap_or("."))
        }),
        PreparedToolInput::Process(input) => serde_json::json!({
            "operation": &input.operation,
            "program": input.program.as_deref().map(bounded_detail),
            "arguments": bounded_arguments(input.args.clone().unwrap_or_default()),
            "cwd": input.cwd.as_deref().map(bounded_detail),
            "process_id": input.process_id,
            "bytes": input.data.as_ref().map_or(0, String::len)
        }),
        _ => serde_json::json!({"input": "structured details unavailable"}),
    };
    Some(PermissionRequest {
        call_id: call_id.clone(),
        tool: tool.to_owned(),
        class: permission,
        operation: tool.to_owned(),
        target: details.get("path").and_then(serde_json::Value::as_str).map(str::to_owned),
        details,
    })
}

fn classification_for_input(input: &PreparedToolInput) -> ActionClassification {
    match input {
        PreparedToolInput::Read(_)
        | PreparedToolInput::List(_)
        | PreparedToolInput::Find(_)
        | PreparedToolInput::Search(_) => ActionClassification::Read,
        PreparedToolInput::Git(input) => match &input.operation {
            crate::GitOperation::Status | crate::GitOperation::Diff => {
                ActionClassification::Verification
            }
            crate::GitOperation::Show
            | crate::GitOperation::Log
            | crate::GitOperation::Branches => ActionClassification::Read,
        },
        PreparedToolInput::Write(_) | PreparedToolInput::Patch(_) => ActionClassification::Mutation,
        PreparedToolInput::Shell(input) => classification_for_command(&analyze_command(
            &input.program,
            input.args.as_deref().unwrap_or(&[]),
            input.cwd.as_deref().unwrap_or(""),
        )),
        PreparedToolInput::Process(input) => match &input.operation {
            crate::ProcessOperation::Read | crate::ProcessOperation::Status => {
                ActionClassification::Read
            }
            crate::ProcessOperation::Start => classification_for_command(&analyze_command(
                input.program.as_deref().unwrap_or(""),
                input.args.as_deref().unwrap_or(&[]),
                input.cwd.as_deref().unwrap_or(""),
            )),
            crate::ProcessOperation::Write
            | crate::ProcessOperation::Signal
            | crate::ProcessOperation::Kill => ActionClassification::Mutation,
        },
    }
}

fn command_effects_for_input(input: &PreparedToolInput) -> Option<aether_core::CommandEffects> {
    match input {
        PreparedToolInput::Shell(input) => Some(analyze_command(
            &input.program,
            input.args.as_deref().unwrap_or(&[]),
            input.cwd.as_deref().unwrap_or(""),
        )),
        PreparedToolInput::Process(input)
            if matches!(&input.operation, crate::ProcessOperation::Start) =>
        {
            Some(analyze_command(
                input.program.as_deref().unwrap_or(""),
                input.args.as_deref().unwrap_or(&[]),
                input.cwd.as_deref().unwrap_or(""),
            ))
        }
        _ => None,
    }
}

fn classification_for_command(effects: &aether_core::CommandEffects) -> ActionClassification {
    if effects.uncertain {
        ActionClassification::Mutation
    } else if effects.class.is_verification() {
        ActionClassification::Verification
    } else if effects.mutates_workspace() {
        ActionClassification::Mutation
    } else if effects.class.is_read_only() {
        ActionClassification::Read
    } else {
        ActionClassification::Other
    }
}

fn paths_for_input(input: &PreparedToolInput) -> Vec<String> {
    match input {
        PreparedToolInput::Read(input) => {
            input.files.iter().map(|file| file.path.clone()).collect()
        }
        PreparedToolInput::List(input) => input.path.clone().into_iter().collect(),
        PreparedToolInput::Find(input) => input.path.clone().into_iter().collect(),
        PreparedToolInput::Search(input) => input.path.clone().into_iter().collect(),
        PreparedToolInput::Write(input) => vec![input.path.clone()],
        PreparedToolInput::Patch(input) => {
            input.files.iter().map(|file| file.path.clone()).collect()
        }
        PreparedToolInput::Shell(input) => {
            let effects = analyze_command(
                &input.program,
                input.args.as_deref().unwrap_or(&[]),
                input.cwd.as_deref().unwrap_or(""),
            );
            effects.paths.into_iter().chain(effects.manifests).collect()
        }
        PreparedToolInput::Process(input) => match &input.operation {
            crate::ProcessOperation::Start => {
                analyze_command(
                    input.program.as_deref().unwrap_or(""),
                    input.args.as_deref().unwrap_or(&[]),
                    input.cwd.as_deref().unwrap_or(""),
                )
                .paths
            }
            _ => Vec::new(),
        },
        PreparedToolInput::Git(input) => input.path.clone().into_iter().collect(),
    }
}

fn footprint_for_input(input: &PreparedToolInput) -> ToolFootprint {
    match input {
        PreparedToolInput::Read(input) => {
            workspace_read_footprint(input.files.iter().map(|file| file.path.clone()))
        }
        PreparedToolInput::List(input) => workspace_read_footprint(std::iter::once(
            input.path.clone().unwrap_or_else(|| ".".to_owned()),
        )),
        PreparedToolInput::Find(input) => workspace_read_footprint(std::iter::once(
            input.path.clone().unwrap_or_else(|| ".".to_owned()),
        )),
        PreparedToolInput::Search(input) => workspace_read_footprint(std::iter::once(
            input.path.clone().unwrap_or_else(|| ".".to_owned()),
        )),
        PreparedToolInput::Git(input) => workspace_read_footprint(std::iter::once(
            input.path.clone().unwrap_or_else(|| ".".to_owned()),
        )),
        PreparedToolInput::Write(_) | PreparedToolInput::Patch(_) => {
            ToolFootprint::exclusive_workspace()
        }
        PreparedToolInput::Shell(input) => analyze_command(
            &input.program,
            input.args.as_deref().unwrap_or(&[]),
            input.cwd.as_deref().unwrap_or(""),
        )
        .footprint(),
        PreparedToolInput::Process(input) => process_footprint(input),
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
        self.definitions
    }

    fn prepare(&self, invocation: ToolInvocation) -> PreparedAction {
        self.prepare_action(invocation)
    }

    fn permission_request(&self, invocation: &ToolInvocation) -> Option<PermissionRequest> {
        self.permission_request(invocation)
    }

    fn footprint(&self, invocation: &ToolInvocation) -> ToolFootprint {
        match invocation.name.as_str() {
            "read" => serde_json::from_value::<crate::ReadInput>(invocation.input.clone())
                .map(|input| {
                    workspace_read_footprint(input.files.into_iter().map(|file| file.path))
                })
                .unwrap_or_else(|_| ToolFootprint::unknown()),
            "list" => serde_json::from_value::<crate::ListInput>(invocation.input.clone())
                .map(|input| {
                    workspace_read_footprint(std::iter::once(
                        input.path.unwrap_or_else(|| ".".to_owned()),
                    ))
                })
                .unwrap_or_else(|_| ToolFootprint::unknown()),
            "find" => serde_json::from_value::<crate::FindInput>(invocation.input.clone())
                .map(|input| {
                    workspace_read_footprint(std::iter::once(
                        input.path.unwrap_or_else(|| ".".to_owned()),
                    ))
                })
                .unwrap_or_else(|_| ToolFootprint::unknown()),
            "search" => serde_json::from_value::<crate::SearchInput>(invocation.input.clone())
                .map(|input| {
                    workspace_read_footprint(std::iter::once(
                        input.path.unwrap_or_else(|| ".".to_owned()),
                    ))
                })
                .unwrap_or_else(|_| ToolFootprint::unknown()),
            "git" => serde_json::from_value::<crate::GitInput>(invocation.input.clone())
                .map(|input| {
                    workspace_read_footprint(std::iter::once(
                        input.path.unwrap_or_else(|| ".".to_owned()),
                    ))
                })
                .unwrap_or_else(|_| ToolFootprint::unknown()),
            "write" | "patch" => ToolFootprint::exclusive_workspace(),
            "shell" => serde_json::from_value::<crate::ShellInput>(invocation.input.clone())
                .map(|input| {
                    analyze_command(
                        &input.program,
                        &input.args.unwrap_or_default(),
                        input.cwd.as_deref().unwrap_or(""),
                    )
                    .footprint()
                })
                .unwrap_or_else(|_| ToolFootprint::unknown()),
            "process" => serde_json::from_value::<crate::ProcessInput>(invocation.input.clone())
                .map(|input| process_footprint(&input))
                .unwrap_or_else(|_| ToolFootprint::unknown()),
            _ => ToolFootprint::unknown(),
        }
    }

    fn execute<'a>(
        &'a self,
        invocation: ToolInvocation,
        context: ToolExecutionContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move { self.dispatch_with_context(invocation, context).await })
    }

    fn execute_prepared<'a>(
        &'a self,
        action: PreparedAction,
        context: ToolExecutionContext,
    ) -> ToolFuture<'a> {
        Box::pin(async move { self.dispatch_prepared(action, context).await })
    }
}

fn workspace_read_footprint(paths: impl IntoIterator<Item = String>) -> ToolFootprint {
    let mut resources = Vec::new();
    for path in paths {
        let Ok(path) = WorkspacePath::new(path) else {
            return ToolFootprint::unknown();
        };
        resources.push(ToolResource::WorkspacePath(path.display()));
    }
    ToolFootprint::from_effects(resources.into_iter().map(aether_core::ToolEffect::Read).collect())
}

fn process_footprint(input: &crate::ProcessInput) -> ToolFootprint {
    use crate::ProcessOperation;

    match input.operation {
        ProcessOperation::Start => input
            .program
            .as_deref()
            .map(|program| {
                analyze_command(
                    program,
                    input.args.as_deref().unwrap_or(&[]),
                    input.cwd.as_deref().unwrap_or(""),
                )
                .footprint()
            })
            .unwrap_or_else(ToolFootprint::unknown),
        ProcessOperation::Read | ProcessOperation::Status => input
            .process_id
            .map(|process_id| {
                ToolFootprint::from_effects(vec![ToolEffect::Read(ToolResource::Process(
                    process_id,
                ))])
            })
            .unwrap_or_else(ToolFootprint::unknown),
        ProcessOperation::Write | ProcessOperation::Signal | ProcessOperation::Kill => input
            .process_id
            .map(|process_id| {
                ToolFootprint::from_effects(vec![ToolEffect::Exclusive(ToolResource::Process(
                    process_id,
                ))])
            })
            .unwrap_or_else(ToolFootprint::unknown),
    }
}

fn definitions() -> &'static [ToolDefinition] {
    static DEFINITIONS: OnceLock<Vec<ToolDefinition>> = OnceLock::new();
    DEFINITIONS
        .get_or_init(|| {
            vec![
        ToolDefinition {
            name: "read".to_owned(),
            description: "Read targeted workspace file ranges. Do not reread unchanged inspected files; prefer line ranges over whole files.".to_owned(),
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
            description: "List bounded workspace directory entries. Start discovery here or with find; do not scan the whole repository.".to_owned(),
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
            description: "Find workspace paths by glob. Use before search or read; do not preload every file.".to_owned(),
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
            description: "Search bounded workspace text after find/list. Follow with targeted reads of matching ranges.".to_owned(),
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
            description: "Atomically create or replace one bounded workspace file. Inspect git status/diff first and pass expected_hash when replacing.".to_owned(),
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
            description: "Apply strict preconditioned hunks. Inspect git status/diff first and do not overwrite unrelated user changes.".to_owned(),
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
            description: "Inspect read-only git status, diff, branch, and recent changes before mutating files.".to_owned(),
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
        })
        .as_slice()
}

#[cfg(test)]
mod command_footprint_tests {
    use super::*;
    use crate::ProcessInput;

    #[test]
    fn direct_read_only_processes_get_narrow_parallel_footprints() {
        let first = ProcessInput {
            operation: crate::ProcessOperation::Start,
            program: Some("rg".to_owned()),
            args: Some(vec!["needle".to_owned(), "src".to_owned()]),
            cwd: None,
            process_id: None,
            stream: None,
            data: None,
            signal: None,
            max_bytes: None,
            timeout_ms: None,
        };
        let second = ProcessInput {
            operation: crate::ProcessOperation::Start,
            program: Some("find".to_owned()),
            args: Some(vec!["tests".to_owned(), "-name".to_owned(), "*.rs".to_owned()]),
            cwd: None,
            process_id: None,
            stream: None,
            data: None,
            signal: None,
            max_bytes: None,
            timeout_ms: None,
        };
        assert!(!process_footprint(&first).conflicts(&process_footprint(&second)));
    }

    #[test]
    fn process_lifecycle_effects_are_serialized_per_process() {
        let status = ProcessInput {
            operation: crate::ProcessOperation::Status,
            program: None,
            args: None,
            cwd: None,
            process_id: Some(7),
            stream: None,
            data: None,
            signal: None,
            max_bytes: None,
            timeout_ms: None,
        };
        let write = ProcessInput {
            operation: crate::ProcessOperation::Write,
            program: None,
            args: None,
            cwd: None,
            process_id: Some(7),
            stream: None,
            data: Some("input".to_owned()),
            signal: None,
            max_bytes: None,
            timeout_ms: None,
        };
        assert!(process_footprint(&status).conflicts(&process_footprint(&write)));
    }
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
