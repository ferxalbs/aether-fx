use std::{
    fs::File,
    io::{self, Read},
};

use aether_core::{BoundedText, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{Workspace, bounded_limit};

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
) -> aether_core::ToolResult {
    let parsed: ReadInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    if parsed.files.is_empty() || parsed.files.len() > 64 {
        return workspace.result_error(
            call_id,
            crate::common::ToolInternalError::Input("read requires 1..=64 files".to_owned()),
        );
    }
    let limit = bounded_limit(parsed.max_bytes, workspace.max_output_bytes());
    let mut remaining = limit;
    let mut files = Vec::with_capacity(parsed.files.len());
    for target in parsed.files {
        if target.start_line.zip(target.end_line).is_some_and(|(start, end)| start > end) {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(
                    "start_line must not be greater than end_line".to_owned(),
                ),
            );
        }
        let (relative, path) = match workspace.resolve_existing(&target.path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        let metadata = match path.metadata() {
            Ok(value) => value,
            Err(error) => {
                return workspace
                    .result_error(call_id.clone(), crate::common::ToolInternalError::Io(error));
            }
        };
        if !metadata.is_file() {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(format!("{relative} is not a file")),
            );
        }
        if remaining == 0 {
            files.push(ReadFile {
                path: relative,
                binary: false,
                truncated: true,
                bytes_read: 0,
                lines: Vec::new(),
            });
            continue;
        }
        let read_limit = remaining.min(crate::common::MAX_INPUT_BYTES);
        let mut bytes = Vec::with_capacity(read_limit.min(8192));
        let file = match File::open(&path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error.into()),
        };
        let mut limited = file.take(read_limit.saturating_add(1) as u64);
        let read_count = match limited.read_to_end(&mut bytes) {
            Ok(count) => count,
            Err(error) => return workspace.result_error(call_id, error.into()),
        };
        let truncated = read_count > read_limit;
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
        files.push(ReadFile { path: relative, binary, truncated, bytes_read: bytes.len(), lines });
    }
    let output = serde_json::json!({ "files": files, "truncated": remaining == 0 });
    let mut result = aether_core::ToolResult::success_json(call_id, output, limit);
    if result.output.is_truncated() {
        result.data = None;
        result.output = BoundedText::new(result.output.as_str(), limit);
    }
    result
}

#[allow(dead_code)]
fn _read_error_is_io(_: io::Error) {}

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
