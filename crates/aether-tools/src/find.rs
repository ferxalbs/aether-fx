use globset::{Glob, GlobSetBuilder};
use ignore::WalkBuilder;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use aether_core::ToolCallId;

use crate::common::{MAX_WALK_ENTRIES, Workspace};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FindInput {
    pub patterns: Vec<String>,
    pub path: Option<String>,
    pub include_hidden: Option<bool>,
    pub respect_ignore: Option<bool>,
    pub max_results: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FindOutput {
    pub paths: Vec<String>,
    pub truncated: bool,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
) -> aether_core::ToolResult {
    let parsed: FindInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    if parsed.patterns.is_empty() || parsed.patterns.len() > 32 {
        return workspace.result_error(
            call_id,
            crate::common::ToolInternalError::Input("find requires 1..=32 patterns".to_owned()),
        );
    }
    let mut glob_builder = GlobSetBuilder::new();
    for pattern in &parsed.patterns {
        let glob = match Glob::new(pattern) {
            Ok(value) => value,
            Err(error) => {
                return workspace.result_error(
                    call_id,
                    crate::common::ToolInternalError::Pattern(error.to_string()),
                );
            }
        };
        glob_builder.add(glob);
    }
    let glob_set = match glob_builder.build() {
        Ok(value) => value,
        Err(error) => {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Pattern(error.to_string()),
            );
        }
    };
    let requested_path = parsed.path.as_deref().unwrap_or(".");
    let (_, base) = match workspace.resolve_existing(requested_path) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let max_results = parsed.max_results.unwrap_or(1000).clamp(1, MAX_WALK_ENTRIES);
    let include_hidden = parsed.include_hidden.unwrap_or(false);
    let respect_ignore = parsed.respect_ignore.unwrap_or(true);
    let mut builder = WalkBuilder::new(base);
    builder
        .hidden(!include_hidden)
        .git_ignore(respect_ignore)
        .git_exclude(respect_ignore)
        .git_global(false)
        .parents(false)
        .ignore(respect_ignore)
        .follow_links(false);
    let mut paths = Vec::new();
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
        let path = entry.path();
        if path == workspace.root() {
            continue;
        }
        if let Err(error) = workspace.verify_entry(path, &path.to_string_lossy()) {
            return workspace.result_error(call_id, error);
        }
        let relative = match path.strip_prefix(workspace.root()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if glob_set.is_match(relative) {
            paths.push(relative.to_string_lossy().into_owned());
            if paths.len() >= max_results {
                truncated = true;
                break;
            }
        }
    }
    paths.sort();
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(FindOutput { paths, truncated }),
        workspace.max_output_bytes(),
    )
}
