use std::path::Path;

use aether_core::ToolCallId;
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{MAX_WALK_ENTRIES, Workspace, bounded_limit};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ListInput {
    pub path: Option<String>,
    pub depth: Option<usize>,
    pub include_hidden: Option<bool>,
    pub respect_ignore: Option<bool>,
    pub max_entries: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListEntry {
    pub path: String,
    pub file_type: String,
    pub size: Option<u64>,
    pub readonly: bool,
    pub symlink: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ListOutput {
    pub entries: Vec<ListEntry>,
    pub truncated: bool,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
) -> aether_core::ToolResult {
    let parsed: ListInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let requested_path = parsed.path.as_deref().unwrap_or(".");
    let (display_base, base) = match workspace.resolve_existing(requested_path) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let base_metadata = match base.metadata() {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error.into()),
    };
    if !base_metadata.is_dir() {
        return workspace.result_error(
            call_id,
            crate::common::ToolInternalError::Input(format!("{display_base} is not a directory")),
        );
    }
    let max_entries = parsed.max_entries.unwrap_or(1000).clamp(1, MAX_WALK_ENTRIES);
    let include_hidden = parsed.include_hidden.unwrap_or(false);
    let respect_ignore = parsed.respect_ignore.unwrap_or(true);
    let mut builder = WalkBuilder::new(&base);
    builder
        .hidden(!include_hidden)
        .git_ignore(respect_ignore)
        .git_exclude(respect_ignore)
        .git_global(false)
        .parents(false)
        .ignore(respect_ignore)
        .follow_links(false);
    if let Some(depth) = parsed.depth {
        builder.max_depth(Some(depth.saturating_add(1)));
    }

    let mut entries = Vec::new();
    let mut truncated = false;
    for result in builder.build() {
        let entry = match result {
            Ok(value) => value,
            Err(error) => {
                return workspace.result_error(
                    call_id,
                    crate::common::ToolInternalError::Input(error.to_string()),
                );
            }
        };
        if entry.path() == Path::new(&base) {
            continue;
        }
        if entries.len() >= max_entries {
            truncated = true;
            break;
        }
        let path = entry.path();
        if let Err(error) = workspace.verify_entry(path, &path.to_string_lossy()) {
            return workspace.result_error(call_id, error);
        }
        let relative = match path.strip_prefix(workspace.root()) {
            Ok(value) => value.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error.into()),
        };
        let file_type = if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "file"
        } else {
            "other"
        };
        entries.push(ListEntry {
            path: relative,
            file_type: file_type.to_owned(),
            size: metadata.is_file().then_some(metadata.len()),
            readonly: metadata.permissions().readonly(),
            symlink: metadata.file_type().is_symlink(),
        });
    }
    let output = serde_json::json!(ListOutput { entries, truncated });
    aether_core::ToolResult::success_json(
        call_id,
        output,
        bounded_limit(None, workspace.max_output_bytes()),
    )
}
