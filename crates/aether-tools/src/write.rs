use aether_core::{CancellationFlag, PermissionClass, ToolCallId, ToolExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{
    MAX_INPUT_BYTES, ToolInternalError, Workspace, file_state, hash_bytes, hash_file,
    install_replacement, spawn_blocking_tool, stage_replacement,
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
    execute_parsed(workspace, call_id, parsed, context).await
}

pub(crate) async fn execute_parsed(
    workspace: &Workspace,
    call_id: ToolCallId,
    parsed: WriteInput,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
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
    let key = match workspace.mutation_key(&path) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let _mutation_guard = match workspace.acquire_mutations([key]) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let initial_state = match file_state(&path) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let create_only = parsed.create_only.unwrap_or(false);
    let initial_hash = if matches!(initial_state, crate::common::FileState::Missing) {
        if parsed.expected_hash.is_some() {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("expected_hash requires an existing file".to_owned()),
            );
        }
        None
    } else if create_only && parsed.expected_hash.is_none() {
        None
    } else {
        let current_hash = match hash_file(&path, &cancellation) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        if let Some(expected_hash) = parsed.expected_hash.as_deref()
            && current_hash != expected_hash
        {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("write precondition failed: file hash changed".to_owned()),
            );
        }
        Some(current_hash)
    };
    let staged = match stage_replacement(&path, parsed.content.as_bytes(), &cancellation) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let result = (|| {
        if let Some(initial_hash) = initial_hash.as_deref() {
            revalidate_hash(&path, initial_hash, &cancellation)?;
        } else if file_state(&path)? != initial_state {
            return Err(ToolInternalError::ConcurrentModification { path: display.clone() });
        }
        install_replacement(&staged, &initial_state, create_only, &cancellation)
    })();
    if let Err(error) = result {
        staged.cleanup();
        return workspace.result_error(call_id, error);
    }
    let output = WriteOutput {
        path: display,
        bytes: parsed.content.len(),
        hash: hash_bytes(parsed.content.as_bytes()),
        replaced: matches!(initial_state, crate::common::FileState::Present { .. }),
    };
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(output),
        workspace.max_output_bytes(),
    )
}

fn revalidate_hash(
    path: &std::path::Path,
    expected_hash: &str,
    cancellation: &CancellationFlag,
) -> Result<(), ToolInternalError> {
    match hash_file(path, cancellation) {
        Ok(current) if current == expected_hash => Ok(()),
        Ok(_) => {
            Err(ToolInternalError::ConcurrentModification { path: path.display().to_string() })
        }
        Err(ToolInternalError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ToolInternalError::ConcurrentModification { path: path.display().to_string() })
        }
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::common::{ToolInternalError, stage_replacement};

    #[test]
    fn expected_hash_revalidation_rejects_change_after_staging() {
        let root =
            std::env::temp_dir().join(format!("aether-write-revalidation-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("file.txt");
        fs::write(&path, b"original").unwrap();
        let expected = blake3::hash(b"original").to_hex().to_string();
        let cancellation = CancellationFlag::new();
        let staged = stage_replacement(&path, b"replacement", &cancellation).unwrap();
        fs::write(&path, b"external change").unwrap();

        assert!(matches!(
            revalidate_hash(&path, &expected, &cancellation),
            Err(ToolInternalError::ConcurrentModification { .. })
        ));
        staged.cleanup();
        let _ = fs::remove_dir_all(root);
    }
}
