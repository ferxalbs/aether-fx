use std::fs;

use aether_core::{CancellationFlag, PermissionClass, ToolCallId, ToolExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{
    MAX_INPUT_BYTES, ToolInternalError, Workspace, atomic_write, hash_bytes, hash_file,
    spawn_blocking_tool,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WriteInput {
    pub path: String,
    pub content: String,
    pub expected_hash: Option<String>,
    pub create_only: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WriteOutput {
    pub path: String,
    pub bytes: usize,
    pub hash: String,
    pub replaced: bool,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    let parsed: WriteInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    if parsed.content.len() > MAX_INPUT_BYTES {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("write content exceeds the bounded input limit".to_owned()),
        );
    }
    if let Err(error) =
        workspace.require_permit(&context, &call_id, "write", PermissionClass::WorkspaceWrite)
    {
        return workspace.result_error(call_id, error);
    }
    spawn_blocking_tool(workspace, call_id, &context, move |workspace, call_id, cancellation| {
        execute_blocking(workspace, call_id, parsed, cancellation)
    })
    .await
}

fn execute_blocking(
    workspace: Workspace,
    call_id: ToolCallId,
    parsed: WriteInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    let (display, path) = match workspace.resolve_for_write(&parsed.path) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let exists = path.exists();
    if parsed.create_only.unwrap_or(false) && exists {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("create_only refused to replace an existing file".to_owned()),
        );
    }
    if let Some(expected_hash) = parsed.expected_hash.as_deref() {
        if !exists {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("expected_hash requires an existing file".to_owned()),
            );
        }
        let current_hash = match hash_file(&path, &cancellation) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        if current_hash != expected_hash {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("write precondition failed: file hash changed".to_owned()),
            );
        }
    }
    if let Err(error) = atomic_write(&path, parsed.content.as_bytes(), &cancellation) {
        return workspace.result_error(call_id, error);
    }
    let output = WriteOutput {
        path: display,
        bytes: parsed.content.len(),
        hash: hash_bytes(parsed.content.as_bytes()),
        replaced: exists,
    };
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(output),
        workspace.max_output_bytes(),
    )
}

#[allow(dead_code)]
fn _metadata_is_file(path: &std::path::Path) -> bool {
    fs::metadata(path).is_ok_and(|metadata| metadata.is_file())
}
