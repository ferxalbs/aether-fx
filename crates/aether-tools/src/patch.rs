use std::{collections::HashSet, fs::File, io::Read, path::PathBuf};

use aether_core::{CancellationFlag, PermissionClass, ToolCallId, ToolExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{
    FileState, MAX_INPUT_BYTES, StagedReplacement, ToolInternalError, Workspace, atomic_write,
    file_state, hash_bytes, hash_file, install_replacement, spawn_blocking_tool, stage_replacement,
};

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

struct PreparedFile {
    display: String,
    path: PathBuf,
    initial_state: FileState,
    old_bytes: Vec<u8>,
    old_hash: String,
    new_bytes: Vec<u8>,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    let parsed: PatchInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    execute_parsed(workspace, call_id, parsed, context).await
}

pub(crate) async fn execute_parsed(
    workspace: &Workspace,
    call_id: ToolCallId,
    parsed: PatchInput,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    if parsed.files.is_empty() || parsed.files.len() > 64 {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("patch requires 1..=64 files".to_owned()),
        );
    }
    if let Err(error) =
        workspace.require_permit(&context, &call_id, "patch", PermissionClass::WorkspaceWrite)
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
    parsed: PatchInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    let mut targets = Vec::with_capacity(parsed.files.len());
    let mut seen = HashSet::with_capacity(parsed.files.len());
    for file in &parsed.files {
        if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
            return workspace.result_error(call_id, error);
        }
        if file.hunks.is_empty() || file.hunks.len() > 256 {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("each patch file requires 1..=256 hunks".to_owned()),
            );
        }
        let (display, path) = match workspace.resolve_for_write(&file.path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        let key = match workspace.mutation_key(&path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        if !seen.insert(key.clone()) {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input(format!("patch contains duplicate file {display}")),
            );
        }
        targets.push((display, path, key, file.expected_hash.clone(), file.hunks.clone()));
    }
    let _mutation_guards =
        match workspace.acquire_mutations(targets.iter().map(|(_, _, key, _, _)| key.clone())) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };

    let mut prepared = Vec::with_capacity(targets.len());
    for (display, path, _, expected_hash, hunks) in targets {
        if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
            return workspace.result_error(call_id, error);
        }
        let initial_state = match file_state(&path) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        let old_bytes = match read_bounded_file(&path, &cancellation) {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        };
        let old_hash = hash_bytes(&old_bytes);
        if let Some(expected_hash) = expected_hash.as_deref()
            && expected_hash != old_hash
        {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input(format!(
                    "patch precondition failed for {display}: file hash changed"
                )),
            );
        }
        let old_text = match String::from_utf8(old_bytes.clone()) {
            Ok(value) => value,
            Err(_) => {
                return workspace.result_error(
                    call_id,
                    ToolInternalError::Input(format!("{display} is not valid UTF-8")),
                );
            }
        };
        let new_bytes = match apply_patch_text(&old_text, &hunks) {
            Ok(value) => value.into_bytes(),
            Err(error) => {
                return workspace.result_error(
                    call_id,
                    ToolInternalError::Input(format!("{display}: {error}")),
                );
            }
        };
        if new_bytes.len() > MAX_INPUT_BYTES {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("patch result exceeds the bounded input limit".to_owned()),
            );
        }
        prepared.push(PreparedFile {
            display,
            path,
            initial_state,
            old_bytes,
            old_hash,
            new_bytes,
        });
    }
    let dry_run = parsed.dry_run.unwrap_or(false);
    let outputs: Vec<PatchFileOutput> = prepared
        .iter()
        .map(|file| PatchFileOutput {
            path: file.display.clone(),
            old_hash: file.old_hash.clone(),
            new_hash: hash_bytes(&file.new_bytes),
            changed: file.new_bytes != file.old_bytes,
        })
        .collect();
    if !dry_run && let Err(error) = commit_prepared(&prepared, &cancellation) {
        return workspace.result_error(call_id, error);
    }
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(PatchOutput { dry_run, files: outputs }),
        workspace.max_output_bytes(),
    )
}

fn commit_prepared(
    prepared: &[PreparedFile],
    cancellation: &CancellationFlag,
) -> Result<(), ToolInternalError> {
    let mut staged = Vec::<(usize, StagedReplacement)>::new();
    for (index, file) in prepared.iter().enumerate() {
        if file.old_bytes == file.new_bytes {
            continue;
        }
        match stage_replacement(&file.path, &file.new_bytes, cancellation) {
            Ok(replacement) => staged.push((index, replacement)),
            Err(error) => {
                for (_, replacement) in &staged {
                    replacement.cleanup();
                }
                return Err(error);
            }
        }
    }
    let mut applied = Vec::<(PathBuf, Vec<u8>)>::new();
    for (index, replacement) in &staged {
        let file = &prepared[*index];
        let commit_result = revalidate_prepared(file, cancellation).and_then(|()| {
            install_replacement(replacement, &file.initial_state, false, cancellation)
        });
        match commit_result {
            Ok(()) => {
                replacement.cleanup();
                applied.push((file.path.clone(), file.old_bytes.clone()));
            }
            Err(error) => {
                for (_, pending) in &staged {
                    pending.cleanup();
                }
                return finish_commit_failure(error, &applied);
            }
        }
    }
    Ok(())
}

fn revalidate_prepared(
    file: &PreparedFile,
    cancellation: &CancellationFlag,
) -> Result<(), ToolInternalError> {
    if file_state(&file.path)? != file.initial_state {
        return Err(ToolInternalError::ConcurrentModification { path: file.display.clone() });
    }
    match hash_file(&file.path, cancellation) {
        Ok(current) if current == file.old_hash => Ok(()),
        Ok(_) => Err(ToolInternalError::ConcurrentModification { path: file.display.clone() }),
        Err(ToolInternalError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(ToolInternalError::ConcurrentModification { path: file.display.clone() })
        }
        Err(error) => Err(error),
    }
}

fn finish_commit_failure(
    commit_error: ToolInternalError,
    applied: &[(PathBuf, Vec<u8>)],
) -> Result<(), ToolInternalError> {
    let rollback_flag = CancellationFlag::new();
    let mut results = Vec::new();
    for (path, bytes) in applied.iter().rev() {
        match atomic_write(path, bytes, &rollback_flag) {
            Ok(()) => results.push(format!("{}: restored", path.display())),
            Err(error) => results.push(format!("{}: rollback failed: {error}", path.display())),
        }
    }
    if results.iter().all(|result| result.ends_with(": restored")) {
        Err(commit_error)
    } else {
        Err(ToolInternalError::RollbackFailed { commit: commit_error.to_string(), results })
    }
}

fn read_bounded_file(
    path: &std::path::Path,
    cancellation: &CancellationFlag,
) -> Result<Vec<u8>, ToolInternalError> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 32 * 1024];
    loop {
        cancellation.check().map_err(ToolInternalError::Core)?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > MAX_INPUT_BYTES {
            return Err(ToolInternalError::Input(
                "patch source exceeds the bounded input limit".to_owned(),
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
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
            if line.is_empty() {
                return Err("hunk lines must begin with space, plus, or minus".to_owned());
            }
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
    hash_file(path, &CancellationFlag::new()).is_ok()
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

    #[test]
    fn rollback_restores_an_already_committed_file() {
        let root =
            std::env::temp_dir().join(format!("aether-patch-rollback-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("file.txt");
        std::fs::write(&path, "new").unwrap();
        let applied = vec![(path.clone(), b"old".to_vec())];
        let result = finish_commit_failure(
            ToolInternalError::Input("simulated commit failure".to_owned()),
            &applied,
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
        let _ = std::fs::remove_dir_all(root);
    }
}
