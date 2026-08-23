use std::path::Path;

use aether_core::{
    BoundedText, CONTEXT_GUIDANCE, CompactToolSummary, ContextSnapshot, FileExcerpt, GitSnapshot,
    InspectedFile, LineRange, MAX_CONTEXT_ITEMS, MAX_CONTEXT_RENDER_BYTES, MAX_EXCERPT_BYTES,
    MAX_FILE_EXCERPTS, MAX_INSPECTED_FILES, MAX_STORED_TOOL_SUMMARIES, MAX_TASK_BYTES,
    ObservedFileState, OpaqueContinuation, ToolResult, WorkspacePath, compact_tool_result,
    merge_line_ranges,
};
use serde_json::Value;

use crate::repo_map::RepoMap;

const MAX_REPO_MAP_CONTEXT_BYTES: usize = 8 * 1024;

/// In-memory bounded context engine for one AETHER session.
#[derive(Clone, Debug)]
pub struct ContextEngine {
    snapshot: ContextSnapshot,
    repo_map: Option<RepoMap>,
}

impl ContextEngine {
    /// Create an empty engine for a workspace.
    pub fn new(workspace_root: impl Into<String>, model: Option<String>) -> Self {
        let workspace_root = workspace_root.into();
        let repo_map = (!workspace_root.is_empty()).then(|| RepoMap::new(&workspace_root));
        Self { snapshot: ContextSnapshot::new(workspace_root, model), repo_map }
    }

    /// Restore from persisted snapshot without trusting hashes yet.
    pub fn restore(mut snapshot: ContextSnapshot) -> Self {
        snapshot.inspected.truncate(MAX_INSPECTED_FILES);
        snapshot.excerpts.truncate(MAX_FILE_EXCERPTS);
        snapshot.tool_summaries.truncate(MAX_STORED_TOOL_SUMMARIES);
        snapshot.modified.truncate(MAX_CONTEXT_ITEMS);
        let repo_map =
            (!snapshot.workspace_root.is_empty()).then(|| RepoMap::new(&snapshot.workspace_root));
        Self { snapshot, repo_map }
    }

    /// Return the current bounded snapshot.
    pub fn snapshot(&self) -> &ContextSnapshot {
        &self.snapshot
    }

    /// Consume the engine and return the snapshot.
    pub fn into_snapshot(self) -> ContextSnapshot {
        self.snapshot
    }

    /// Return the lazy repository map associated with this context, if a workspace is known.
    pub fn repository_map(&self) -> Option<&RepoMap> {
        self.repo_map.as_ref()
    }

    /// Record the latest user task.
    pub fn set_task(&mut self, prompt: &str) {
        self.snapshot.current_task = BoundedText::new(prompt, MAX_TASK_BYTES);
    }

    /// Store the latest opaque continuation after sanitizing identity keys.
    pub fn set_continuation(&mut self, continuation: Option<OpaqueContinuation>) {
        self.snapshot.continuation =
            continuation.as_ref().and_then(aether_core::persistable_continuation);
    }

    /// Record a compact git snapshot.
    pub fn set_git(&mut self, git: GitSnapshot) {
        self.snapshot.git = Some(git);
    }

    /// Mark that resume or live observation found workspace drift.
    pub fn set_workspace_changed(&mut self, changed: bool) {
        self.snapshot.workspace_changed = changed;
    }

    /// Update model identity if a later turn selected one.
    pub fn set_model(&mut self, model: Option<String>) {
        if model.is_some() {
            self.snapshot.model = model;
        }
    }

    /// Observe a completed tool call and fold it into bounded working state.
    pub fn observe_tool(&mut self, name: &str, input: &Value, result: &ToolResult) {
        let summary = compact_tool_result(name, result);
        self.push_summary(summary);
        match name {
            "read" => self.observe_read(result),
            "write" => self.observe_write(input, result),
            "patch" => self.observe_patch(result),
            "git" => self.observe_git(result),
            _ => {}
        }
        self.enforce_bounds();
    }

    /// Re-hash inspected files against the live workspace. Stale hashes never win.
    pub fn refresh_against_workspace(&mut self, workspace_root: &Path) -> Vec<String> {
        let mut stale_paths = Vec::new();
        let live_root = workspace_root.display().to_string();
        if !self.snapshot.workspace_root.is_empty() && self.snapshot.workspace_root != live_root {
            self.snapshot.workspace_changed = true;
        }
        self.snapshot.workspace_root = live_root;
        if self.repo_map.as_ref().is_none_or(|map| map.root() != workspace_root) {
            self.repo_map = Some(RepoMap::new(workspace_root));
        }
        let mut valid = Vec::new();
        for mut file in self.snapshot.inspected.drain(..) {
            if !path_is_workspace_relative(&file.path) {
                self.snapshot.workspace_changed = true;
                continue;
            }
            match live_file_hash(workspace_root, &file.path) {
                Some(hash) => {
                    if file.content_hash.as_ref().is_some_and(|expected| expected != &hash) {
                        file.stale = true;
                        file.content_hash = Some(hash);
                        stale_paths.push(file.path.clone());
                    } else if file.content_hash.is_none() {
                        file.content_hash = Some(hash);
                        file.stale = false;
                    } else {
                        file.stale = false;
                        file.last_state = ObservedFileState::Present;
                    }
                    valid.push(file);
                }
                None => {
                    if file.last_state == ObservedFileState::Present {
                        file.stale = true;
                        file.last_state = ObservedFileState::Missing;
                        stale_paths.push(file.path.clone());
                    }
                    valid.push(file);
                }
            }
        }
        self.snapshot.inspected = valid;
        if !stale_paths.is_empty() {
            self.snapshot.workspace_changed = true;
            let stale = stale_paths.iter().cloned().collect::<std::collections::HashSet<_>>();
            self.snapshot.excerpts.retain(|excerpt| !stale.contains(&excerpt.path));
        }
        self.snapshot.modified.retain(|path| path_is_workspace_relative(path));
        stale_paths
    }

    /// Render a token-conscious packet for model reconstruction. Full files are never included.
    pub fn render(&self, include_guidance: bool) -> String {
        let mut out = String::from("# AETHER workspace context\n");
        if !self.snapshot.workspace_root.is_empty() {
            out.push_str("workspace: ");
            out.push_str(&self.snapshot.workspace_root);
            out.push('\n');
        }
        if !self.snapshot.current_task.as_str().is_empty() {
            out.push_str("task: ");
            out.push_str(self.snapshot.current_task.as_str());
            out.push('\n');
        }
        if self.snapshot.workspace_changed {
            out.push_str(
                "workspace_changed: true; treat persisted hashes and excerpts as stale until re-read\n",
            );
        }
        if let Some(git) = &self.snapshot.git {
            out.push_str("git: ");
            if git.available {
                if let Some(branch) = &git.branch {
                    out.push_str("branch ");
                    out.push_str(branch);
                    out.push_str("; ");
                }
                out.push_str(git.status.as_str());
            } else {
                out.push_str("unavailable");
            }
            out.push('\n');
        }
        if let Some(repo_map) = &self.repo_map
            && let Ok(compact) = repo_map.compact(MAX_REPO_MAP_CONTEXT_BYTES)
            && !compact.is_empty()
        {
            out.push_str("repository map:\n");
            out.push_str(&compact);
            if !compact.ends_with('\n') {
                out.push('\n');
            }
        }
        if !self.snapshot.inspected.is_empty() {
            out.push_str("inspected (do not reread unless stale):\n");
            for file in self.snapshot.inspected.iter().take(MAX_CONTEXT_ITEMS) {
                out.push_str("- ");
                out.push_str(&file.path);
                if let Some(hash) = &file.content_hash {
                    out.push_str(" hash=");
                    out.push_str(hash);
                }
                if !file.ranges.is_empty() {
                    out.push_str(" ranges=");
                    out.push_str(&format_ranges(&file.ranges));
                }
                if file.stale {
                    out.push_str(" [STALE]");
                } else {
                    out.push_str(" [current]");
                }
                out.push('\n');
            }
        }
        if !self.snapshot.modified.is_empty() {
            out.push_str("modified:\n");
            for path in self.snapshot.modified.iter().take(MAX_CONTEXT_ITEMS) {
                out.push_str("- ");
                out.push_str(path);
                out.push('\n');
            }
        }
        if !self.snapshot.tool_summaries.is_empty() {
            out.push_str("recent tools:\n");
            for summary in &self.snapshot.tool_summaries {
                out.push_str("- ");
                out.push_str(&summary.tool);
                out.push_str(": ");
                out.push_str(summary.summary.as_str());
                out.push('\n');
            }
        }
        if !self.snapshot.excerpts.is_empty() {
            out.push_str("excerpts:\n");
            for excerpt in &self.snapshot.excerpts {
                out.push_str("- ");
                out.push_str(&excerpt.path);
                out.push_str(&format!(":{}-{}\n", excerpt.start_line, excerpt.end_line));
                out.push_str(excerpt.text.as_str());
                out.push('\n');
            }
        }
        if include_guidance {
            out.push_str("guidance: ");
            out.push_str(CONTEXT_GUIDANCE);
            out.push('\n');
        }
        if out.len() > MAX_CONTEXT_RENDER_BYTES {
            BoundedText::new(out, MAX_CONTEXT_RENDER_BYTES).into_string()
        } else {
            out
        }
    }

    /// Whether the packet should be attached to the next user turn.
    pub fn should_attach_to_prompt(&self, has_continuation: bool) -> bool {
        if !has_continuation {
            return !self.snapshot.is_empty() || !self.snapshot.workspace_root.is_empty();
        }
        self.snapshot.workspace_changed || self.snapshot.inspected.iter().any(|file| file.stale)
    }

    fn observe_read(&mut self, result: &ToolResult) {
        let Some(files) = result.data.as_ref().and_then(|data| data.get("files")?.as_array())
        else {
            return;
        };
        for file in files {
            let Some(path) = file.get("path").and_then(Value::as_str) else {
                continue;
            };
            if !path_is_workspace_relative(path) {
                continue;
            }
            let hash = file.get("content_hash").and_then(Value::as_str).map(str::to_owned);
            let lines = file.get("lines").and_then(Value::as_array);
            let range = lines.and_then(|lines| line_range_from_read(lines));
            self.upsert_inspected(path, hash.clone(), range, ObservedFileState::Present);
            if let (Some(range), Some(lines)) = (range, lines)
                && let Some(text) = excerpt_from_lines(lines)
            {
                self.push_excerpt(FileExcerpt {
                    path: path.to_owned(),
                    start_line: range.start,
                    end_line: range.end,
                    text: BoundedText::new(text, MAX_EXCERPT_BYTES),
                    content_hash: hash,
                });
            }
        }
    }

    fn observe_write(&mut self, input: &Value, result: &ToolResult) {
        let path = result
            .data
            .as_ref()
            .and_then(|data| data.get("path").and_then(Value::as_str))
            .or_else(|| input.get("path").and_then(Value::as_str));
        let Some(path) = path else {
            return;
        };
        if !path_is_workspace_relative(path) {
            return;
        }
        let hash = result.data.as_ref().and_then(|data| data.get("hash").and_then(Value::as_str));
        if result.ok {
            self.upsert_inspected(path, hash.map(str::to_owned), None, ObservedFileState::Present);
            self.record_modified(path);
            self.snapshot.excerpts.retain(|excerpt| excerpt.path != path);
        }
    }

    fn observe_patch(&mut self, result: &ToolResult) {
        let Some(files) = result.data.as_ref().and_then(|data| data.get("files")?.as_array())
        else {
            return;
        };
        if result.data.as_ref().and_then(|data| data.get("dry_run")?.as_bool()).unwrap_or(false) {
            return;
        }
        for file in files {
            let Some(path) = file.get("path").and_then(Value::as_str) else {
                continue;
            };
            if !path_is_workspace_relative(path) {
                continue;
            }
            let hash = file.get("new_hash").and_then(Value::as_str).map(str::to_owned);
            if result.ok {
                self.upsert_inspected(path, hash, None, ObservedFileState::Present);
                self.record_modified(path);
                self.snapshot.excerpts.retain(|excerpt| excerpt.path != path);
            }
        }
    }

    fn observe_git(&mut self, result: &ToolResult) {
        let Some(data) = result.data.as_ref() else {
            return;
        };
        let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("");
        let branch = stdout.lines().find_map(|line| {
            line.strip_prefix("## ")
                .map(|rest| rest.split([' ', '.']).next().unwrap_or(rest).to_owned())
        });
        self.snapshot.git = Some(GitSnapshot {
            branch,
            status: BoundedText::new(stdout, aether_core::MAX_GIT_SNAPSHOT_BYTES),
            available: result.ok,
        });
    }

    fn upsert_inspected(
        &mut self,
        path: &str,
        hash: Option<String>,
        range: Option<LineRange>,
        last_state: ObservedFileState,
    ) {
        if self.snapshot.inspected.iter().any(|file| file.path == path) {
            let hash_changed = self.snapshot.inspected.iter().any(|file| {
                file.path == path
                    && hash.as_ref().is_some_and(|new_hash| {
                        file.content_hash.as_ref().is_some_and(|old| old != new_hash)
                    })
            });
            if let Some(existing) =
                self.snapshot.inspected.iter_mut().find(|file| file.path == path)
            {
                if let Some(hash) = hash.clone() {
                    existing.content_hash = Some(hash);
                }
                existing.last_state = last_state;
                existing.stale = false;
                if let Some(range) = range {
                    existing.ranges.push(range);
                    merge_line_ranges(&mut existing.ranges);
                }
            }
            if hash_changed {
                self.snapshot.excerpts.retain(|excerpt| excerpt.path != path);
            }
            return;
        }
        let mut ranges = Vec::new();
        if let Some(range) = range {
            ranges.push(range);
        }
        self.snapshot.inspected.push(InspectedFile {
            path: path.to_owned(),
            content_hash: hash,
            ranges,
            last_state,
            stale: false,
        });
        if self.snapshot.inspected.len() > MAX_INSPECTED_FILES {
            self.snapshot.inspected.remove(0);
        }
    }

    fn record_modified(&mut self, path: &str) {
        if !self.snapshot.modified.iter().any(|existing| existing == path) {
            self.snapshot.modified.push(path.to_owned());
        }
        if self.snapshot.modified.len() > MAX_CONTEXT_ITEMS {
            self.snapshot.modified.remove(0);
        }
    }

    fn push_summary(&mut self, summary: CompactToolSummary) {
        self.snapshot.tool_summaries.push(summary);
        if self.snapshot.tool_summaries.len() > MAX_STORED_TOOL_SUMMARIES {
            self.snapshot.tool_summaries.remove(0);
        }
    }

    fn push_excerpt(&mut self, excerpt: FileExcerpt) {
        if self.snapshot.excerpts.iter().any(|existing| {
            existing.path == excerpt.path
                && existing.start_line == excerpt.start_line
                && existing.end_line == excerpt.end_line
        }) {
            self.snapshot.excerpts.retain(|existing| {
                !(existing.path == excerpt.path
                    && existing.start_line == excerpt.start_line
                    && existing.end_line == excerpt.end_line)
            });
        }
        self.snapshot.excerpts.push(excerpt);
        if self.snapshot.excerpts.len() > MAX_FILE_EXCERPTS {
            self.snapshot.excerpts.remove(0);
        }
    }

    fn enforce_bounds(&mut self) {
        self.snapshot.inspected.truncate(MAX_INSPECTED_FILES);
        self.snapshot.excerpts.truncate(MAX_FILE_EXCERPTS);
        self.snapshot.tool_summaries.truncate(MAX_STORED_TOOL_SUMMARIES);
        self.snapshot.modified.truncate(MAX_CONTEXT_ITEMS);
    }
}

fn path_is_workspace_relative(path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
}

fn format_ranges(ranges: &[LineRange]) -> String {
    ranges
        .iter()
        .map(|range| format!("{}-{}", range.start, range.end))
        .collect::<Vec<_>>()
        .join(",")
}

fn line_range_from_read(lines: &[Value]) -> Option<LineRange> {
    let mut start = None;
    let mut end = None;
    for line in lines {
        if let Some(number) = line.get("number").and_then(Value::as_u64) {
            let number = number as usize;
            start = Some(start.map_or(number, |value: usize| value.min(number)));
            end = Some(end.map_or(number, |value: usize| value.max(number)));
        }
    }
    Some(LineRange { start: start?, end: end? })
}

fn excerpt_from_lines(lines: &[Value]) -> Option<String> {
    let mut text = String::new();
    for line in lines {
        let line_text = line.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(line_text);
        if text.len() >= MAX_EXCERPT_BYTES {
            break;
        }
    }
    if text.is_empty() { None } else { Some(text) }
}

fn live_file_hash(root: &Path, relative: &str) -> Option<String> {
    let relative = WorkspacePath::new(relative).ok()?;
    let path = root.join(relative.as_path());
    let bytes = std::fs::read(&path).ok()?;
    if bytes.len() > 4 * 1024 * 1024 {
        return None;
    }
    Some(blake3::hash(&bytes).to_hex().to_string())
}

/// Capture a compact git status snapshot without exposing a new model-visible tool.
pub fn capture_git_snapshot(workspace_root: &Path) -> GitSnapshot {
    let status = std::process::Command::new("git")
        .args(["status", "--short", "--branch"])
        .current_dir(workspace_root)
        .output();
    match status {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            let branch = stdout.lines().next().and_then(|line| {
                line.strip_prefix("## ").map(|rest| {
                    rest.split([' ', '.']).next().unwrap_or(rest).trim_start_matches('*').to_owned()
                })
            });
            GitSnapshot {
                branch,
                status: BoundedText::new(stdout, aether_core::MAX_GIT_SNAPSHOT_BYTES),
                available: true,
            }
        }
        _ => GitSnapshot {
            branch: None,
            status: BoundedText::new("git unavailable", aether_core::MAX_GIT_SNAPSHOT_BYTES),
            available: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::ToolCallId;

    fn success(data: Value) -> ToolResult {
        ToolResult::success_json(ToolCallId::new("ctx-1").unwrap(), data, 64 * 1024)
    }

    #[test]
    fn context_deduplicates_inspected_paths_and_ranges() {
        let mut engine = ContextEngine::new("/workspace", None);
        let result = success(serde_json::json!({
            "files": [{
                "path": "src/lib.rs",
                "content_hash": "aaa",
                "truncated": false,
                "lines": [
                    {"number": 1, "text": "mod a;"},
                    {"number": 2, "text": "mod b;"}
                ]
            }]
        }));
        engine.observe_tool("read", &Value::Null, &result);
        engine.observe_tool("read", &Value::Null, &result);
        assert_eq!(engine.snapshot().inspected.len(), 1);
        assert_eq!(engine.snapshot().inspected[0].ranges, vec![LineRange { start: 1, end: 2 }]);
        assert_eq!(engine.snapshot().excerpts.len(), 1);
    }

    #[test]
    fn context_bounds_drop_oldest_inspected_files() {
        let mut engine = ContextEngine::new("/workspace", None);
        for index in 0..(MAX_INSPECTED_FILES + 5) {
            let result = success(serde_json::json!({
                "path": format!("file-{index}.txt"),
                "bytes": 1,
                "hash": format!("{index:064}"),
                "replaced": false
            }));
            engine.observe_tool(
                "write",
                &serde_json::json!({"path": format!("file-{index}.txt")}),
                &result,
            );
        }
        assert_eq!(engine.snapshot().inspected.len(), MAX_INSPECTED_FILES);
        assert_eq!(engine.snapshot().modified.len(), MAX_CONTEXT_ITEMS);
        assert!(!engine.snapshot().inspected.iter().any(|file| file.path == "file-0.txt"));
    }

    #[test]
    fn changed_file_invalidation_marks_stale_and_drops_excerpts() {
        let root = std::env::temp_dir().join(format!(
            "aether-context-stale-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("src.rs");
        std::fs::write(&path, "old").unwrap();
        let hash = blake3::hash(b"old").to_hex().to_string();
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        let result = success(serde_json::json!({
            "files": [{
                "path": "src.rs",
                "content_hash": hash,
                "truncated": false,
                "lines": [{"number": 1, "text": "old"}]
            }]
        }));
        engine.observe_tool("read", &Value::Null, &result);
        assert_eq!(engine.snapshot().excerpts.len(), 1);
        std::fs::write(&path, "new").unwrap();
        let stale = engine.refresh_against_workspace(&root);
        assert_eq!(stale, vec!["src.rs".to_owned()]);
        assert!(engine.snapshot().inspected[0].stale);
        assert!(engine.snapshot().excerpts.is_empty());
        assert!(engine.snapshot().workspace_changed);
        let _ = std::fs::remove_dir_all(root);
    }
}
