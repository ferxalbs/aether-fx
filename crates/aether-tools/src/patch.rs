use std::fs;

use aether_core::{PermissionClass, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{MAX_INPUT_BYTES, Workspace, atomic_write, hash_bytes, hash_file};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PatchInput {
    pub dry_run: Option<bool>,
    pub files: Vec<PatchFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PatchFile {
    pub path: String,
    pub expected_hash: Option<String>,
    pub hunks: Vec<PatchHunk>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PatchHunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PatchOutput {
    pub dry_run: bool,
    pub files: Vec<PatchFileOutput>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PatchFileOutput {
    pub path: String,
    pub old_hash: String,
    pub new_hash: String,
    pub changed: bool,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
) -> aether_core::ToolResult {
    let parsed: PatchInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    if parsed.files.is_empty() || parsed.files.len() > 64 {
        return workspace.result_error(
            call_id,
            crate::common::ToolInternalError::Input("patch requires 1..=64 files".to_owned()),
        );
    }
    let mut prepared = Vec::with_capacity(parsed.files.len());
    for file in &parsed.files {
        if file.hunks.is_empty() || file.hunks.len() > 256 {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(
                    "each patch file requires 1..=256 hunks".to_owned(),
                ),
            );
        }
        let (display, path) = match workspace.resolve_for_write(&file.path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        let old_bytes = match fs::read(&path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error.into()),
        };
        if old_bytes.len() > MAX_INPUT_BYTES {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(
                    "patch source exceeds the bounded input limit".to_owned(),
                ),
            );
        }
        let old_hash = hash_bytes(&old_bytes);
        if let Some(expected_hash) = file.expected_hash.as_deref()
            && expected_hash != old_hash
        {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(format!(
                    "patch precondition failed for {display}: file hash changed"
                )),
            );
        }
        let old_text = match String::from_utf8(old_bytes.clone()) {
            Ok(value) => value,
            Err(_) => {
                return workspace.result_error(
                    call_id,
                    crate::common::ToolInternalError::Input(format!(
                        "{display} is not valid UTF-8"
                    )),
                );
            }
        };
        let new_bytes = match apply_patch_text(&old_text, &file.hunks) {
            Ok(value) => value.into_bytes(),
            Err(error) => {
                return workspace.result_error(
                    call_id,
                    crate::common::ToolInternalError::Input(format!("{display}: {error}")),
                );
            }
        };
        if new_bytes.len() > MAX_INPUT_BYTES {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(
                    "patch result exceeds the bounded input limit".to_owned(),
                ),
            );
        }
        prepared.push((display, path, old_bytes, old_hash, new_bytes));
    }
    if let Err(error) = workspace.authorize(
        PermissionClass::WorkspaceWrite,
        if parsed.dry_run.unwrap_or(false) { "validate patch" } else { "apply patch" },
        None,
        serde_json::json!({
            "files": prepared.iter().map(|entry| entry.0.as_str()).collect::<Vec<_>>(),
            "dry_run": parsed.dry_run.unwrap_or(false)
        }),
    ) {
        return workspace.result_error(call_id, error);
    }
    let dry_run = parsed.dry_run.unwrap_or(false);
    let outputs: Vec<PatchFileOutput> = prepared
        .iter()
        .map(|(display, _, old_bytes, old_hash, new_bytes)| PatchFileOutput {
            path: display.clone(),
            old_hash: old_hash.clone(),
            new_hash: hash_bytes(new_bytes),
            changed: new_bytes != old_bytes,
        })
        .collect();
    if !dry_run {
        let mut applied: Vec<(&std::path::Path, &[u8])> = Vec::new();
        for (_, path, old_bytes, _, new_bytes) in &prepared {
            if old_bytes == new_bytes {
                continue;
            }
            if let Err(error) = atomic_write(path, new_bytes) {
                for (rollback_path, rollback_bytes) in applied.iter().rev() {
                    let _ = atomic_write(rollback_path, rollback_bytes);
                }
                return workspace.result_error(call_id, error);
            }
            applied.push((path, old_bytes));
        }
    }
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(PatchOutput { dry_run, files: outputs }),
        workspace.max_output_bytes(),
    )
}

pub fn apply_patch_text(source: &str, hunks: &[PatchHunk]) -> Result<String, String> {
    let newline = if source.contains("\r\n") { "\r\n" } else { "\n" };
    let had_final_newline = source.ends_with('\n');
    let mut source_lines: Vec<String> = if source.is_empty() {
        Vec::new()
    } else {
        source.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned()).collect()
    };
    if had_final_newline {
        source_lines.pop();
    }
    let mut output = Vec::new();
    let mut source_cursor = 0usize;
    for hunk in hunks {
        if hunk.old_start == 0 || hunk.new_start == 0 {
            return Err("hunk line numbers must be at least 1".to_owned());
        }
        let start = hunk.old_start - 1;
        if start < source_cursor || start > source_lines.len() {
            return Err("hunks overlap, are out of order, or point outside the file".to_owned());
        }
        if hunk.old_count > source_lines.len().saturating_sub(start) {
            return Err("hunk consumes more source lines than available".to_owned());
        }
        output.extend(source_lines[source_cursor..start].iter().cloned());
        if hunk.new_start != output.len() + 1 {
            return Err("new hunk location does not match the exact output position".to_owned());
        }
        let mut old_seen = 0usize;
        let mut new_seen = 0usize;
        let mut source_index = start;
        for line in &hunk.lines {
            let (kind, content) = line.split_at(1);
            match kind {
                " " => {
                    if source_index >= source_lines.len() || source_lines[source_index] != content {
                        return Err("context line does not match exactly".to_owned());
                    }
                    output.push(content.to_owned());
                    source_index += 1;
                    old_seen += 1;
                    new_seen += 1;
                }
                "-" => {
                    if source_index >= source_lines.len() || source_lines[source_index] != content {
                        return Err("deletion line does not match exactly".to_owned());
                    }
                    source_index += 1;
                    old_seen += 1;
                }
                "+" => {
                    output.push(content.to_owned());
                    new_seen += 1;
                }
                _ => return Err("hunk lines must begin with space, plus, or minus".to_owned()),
            }
        }
        if old_seen != hunk.old_count || new_seen != hunk.new_count {
            return Err("hunk line counts do not match declared counts".to_owned());
        }
        source_cursor = start + hunk.old_count;
    }
    output.extend(source_lines[source_cursor..].iter().cloned());
    let mut result = output.join(newline);
    if had_final_newline {
        result.push_str(newline);
    }
    Ok(result)
}

#[allow(dead_code)]
fn _hash_file_is_available(path: &std::path::Path) -> bool {
    hash_file(path).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_rejects_fuzzy_context() {
        let hunk = PatchHunk {
            old_start: 1,
            old_count: 1,
            new_start: 1,
            new_count: 1,
            lines: vec![" wrong".to_owned()],
        };
        assert!(apply_patch_text("right\n", &[hunk]).is_err());
    }

    #[test]
    fn patch_applies_exact_hunk() {
        let hunk = PatchHunk {
            old_start: 2,
            old_count: 1,
            new_start: 2,
            new_count: 1,
            lines: vec!["-old".to_owned(), "+new".to_owned()],
        };
        assert_eq!(apply_patch_text("first\nold\n", &[hunk]).unwrap(), "first\nnew\n");
    }
}
