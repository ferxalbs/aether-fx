use std::{fs::File, io::Read};

use aether_core::{
    BoundedText, CancellationFlag, PermissionClass, ToolCallId, ToolExecutionContext,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{
    MAX_INPUT_BYTES, ToolInternalError, Workspace, bounded_limit, hash_file, spawn_blocking_tool,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadInput {
    pub files: Vec<ReadTarget>,
    pub max_bytes: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadTarget {
    pub path: String,
    pub start_line: Option<usize>,
    pub end_line: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadFile {
    pub path: String,
    pub binary: bool,
    pub truncated: bool,
    pub bytes_read: usize,
    pub content_hash: Option<String>,
    pub lines: Vec<ReadLine>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadLine {
    pub number: usize,
    pub text: String,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    let parsed: ReadInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    execute_parsed(workspace, call_id, parsed, context).await
}

pub(crate) async fn execute_parsed(
    workspace: &Workspace,
    call_id: ToolCallId,
    parsed: ReadInput,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    if parsed.files.is_empty() || parsed.files.len() > 64 {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("read requires 1..=64 files".to_owned()),
        );
    }
    if let Err(error) =
        workspace.require_permit(&context, &call_id, "read", PermissionClass::ReadOnly)
    {
        return workspace.result_error(call_id, error);
    }
    let limit = bounded_limit(parsed.max_bytes, workspace.max_output_bytes());
    spawn_blocking_tool(workspace, call_id, &context, move |workspace, call_id, cancellation| {
        execute_blocking(workspace, call_id, parsed, limit, cancellation)
    })
    .await
}

fn execute_blocking(
    workspace: Workspace,
    call_id: ToolCallId,
    parsed: ReadInput,
    limit: usize,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    let mut remaining = limit;
    let mut files = Vec::with_capacity(parsed.files.len());
    for target in parsed.files {
        if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
            return workspace.result_error(call_id, error);
        }
        if target.start_line.zip(target.end_line).is_some_and(|(start, end)| start > end) {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("start_line must not be greater than end_line".to_owned()),
            );
        }
        let (relative, path) = match workspace.resolve_existing(&target.path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        let metadata = match path.metadata() {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id.clone(), error.into()),
        };
        if !metadata.is_file() {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input(format!("{relative} is not a file")),
            );
        }
        if remaining == 0 {
            files.push(ReadFile {
                path: relative,
                binary: false,
                truncated: true,
                bytes_read: 0,
                content_hash: None,
                lines: Vec::new(),
            });
            continue;
        }
        let read_limit = remaining.min(MAX_INPUT_BYTES);
        let file = match File::open(&path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error.into()),
        };
        let (mut bytes, truncated) = match read_limited(file, read_limit, &cancellation) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        if truncated {
            bytes.truncate(read_limit);
        }
        remaining = remaining.saturating_sub(bytes.len());
        let binary = bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
        let lines = if binary {
            Vec::new()
        } else {
            let text = String::from_utf8_lossy(&bytes);
            let start = target.start_line.unwrap_or(1);
            let end = target.end_line.unwrap_or(usize::MAX);
            text.lines()
                .enumerate()
                .filter_map(|(index, line)| {
                    let number = index + 1;
                    (number >= start && number <= end)
                        .then(|| ReadLine { number, text: line.to_owned() })
                })
                .collect()
        };
        let content_hash = hash_file(&path, &cancellation).ok();
        files.push(ReadFile {
            path: relative,
            binary,
            truncated,
            bytes_read: bytes.len(),
            content_hash,
            lines,
        });
    }
    let output = serde_json::json!({ "files": files, "truncated": remaining == 0 });
    let mut result = aether_core::ToolResult::success_json(call_id, output, limit);
    if result.output.is_truncated() {
        result.data = None;
        result.output = BoundedText::new(result.output.as_str(), limit);
    }
    result
}

fn read_limited(
    mut file: File,
    limit: usize,
    cancellation: &CancellationFlag,
) -> Result<(Vec<u8>, bool), ToolInternalError> {
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 32 * 1024];
    let mut truncated = false;
    loop {
        cancellation.check().map_err(ToolInternalError::Core)?;
        let count = file.read(&mut buffer).map_err(ToolInternalError::Io)?;
        if count == 0 {
            break;
        }
        let remaining = limit.saturating_add(1).saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&buffer[..retained]);
        if retained < count {
            truncated = true;
            break;
        }
    }
    Ok((bytes, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ranges_are_typed() {
        let input: ReadInput = serde_json::from_value(serde_json::json!({
            "files": [{"path": "src/main.rs", "start_line": 2, "end_line": 4}]
        }))
        .unwrap();
        assert_eq!(input.files[0].start_line, Some(2));
    }
}
