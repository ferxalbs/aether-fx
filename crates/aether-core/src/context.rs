use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{BoundedText, OpaqueContinuation, ToolResult};

/// Maximum persisted session JSONL size, including all records.
pub const MAX_SESSION_FILE_BYTES: usize = 2 * 1024 * 1024;
/// Maximum bytes of one JSONL session line.
pub const MAX_SESSION_LINE_BYTES: usize = 256 * 1024;
/// Maximum JSONL records retained in a session file.
pub const MAX_SESSION_LINES: usize = 4_096;
/// Maximum completed turns retained in a session file.
pub const MAX_STORED_TURNS: usize = 64;
/// Maximum compact tool summaries retained in working context.
pub const MAX_STORED_TOOL_SUMMARIES: usize = 32;
/// Maximum inspected-file and related context entries.
pub const MAX_CONTEXT_ITEMS: usize = 48;
/// Maximum code excerpts retained for model reconstruction.
pub const MAX_FILE_EXCERPTS: usize = 16;
/// Maximum bytes stored for one excerpt.
pub const MAX_EXCERPT_BYTES: usize = 4 * 1024;
/// Maximum inspected files tracked for stale detection.
pub const MAX_INSPECTED_FILES: usize = 64;
/// Maximum bytes of one compact tool summary.
pub const MAX_COMPACT_SUMMARY_BYTES: usize = 2 * 1024;
/// Maximum bytes of a stored git snapshot.
pub const MAX_GIT_SNAPSHOT_BYTES: usize = 4 * 1024;
/// Maximum bytes of the current-task string.
pub const MAX_TASK_BYTES: usize = 4 * 1024;
/// Maximum bytes of a persisted user prompt.
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
/// Maximum bytes of the assembled model-visible context packet.
pub const MAX_CONTEXT_RENDER_BYTES: usize = 24 * 1024;
/// Maximum relevant paths listed in a search/find summary.
pub const MAX_SUMMARY_PATHS: usize = 8;

/// Last observed existence of an inspected workspace file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservedFileState {
    /// The path existed as a regular file when last inspected.
    Present,
    /// The path was missing when last observed.
    Missing,
}

/// Inclusive 1-based line range observed during a targeted read.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LineRange {
    /// First observed line, 1-based.
    pub start: usize,
    /// Last observed line, 1-based inclusive.
    pub end: usize,
}

/// Metadata for one inspected workspace file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InspectedFile {
    /// Workspace-relative path.
    pub path: String,
    /// BLAKE3 hex digest of the last observed file bytes, when known.
    pub content_hash: Option<String>,
    /// Targeted ranges already read from this file.
    pub ranges: Vec<LineRange>,
    /// Existence at last observation.
    pub last_state: ObservedFileState,
    /// Whether the on-disk file no longer matches `content_hash`.
    pub stale: bool,
}

/// A bounded excerpt retained for reconstruction, never a full-file copy.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct FileExcerpt {
    /// Workspace-relative path.
    pub path: String,
    /// First excerpt line, 1-based.
    pub start_line: usize,
    /// Last excerpt line, 1-based inclusive.
    pub end_line: usize,
    /// Bounded excerpt text.
    pub text: BoundedText,
    /// File hash observed with this excerpt, when known.
    pub content_hash: Option<String>,
}

/// Deterministic local compaction of a tool result.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CompactToolSummary {
    /// Model-visible tool name.
    pub tool: String,
    /// Whether the tool reported success.
    pub ok: bool,
    /// Bounded structured summary.
    pub summary: BoundedText,
}

/// Compact read-only git working-tree snapshot.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct GitSnapshot {
    /// Current branch name when git is available.
    pub branch: Option<String>,
    /// Bounded status/diff summary.
    pub status: BoundedText,
    /// Whether a git repository was present and readable.
    pub available: bool,
}

/// Bounded, serializable working context for one AETHER session.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContextSnapshot {
    /// Absolute workspace root last associated with the session.
    pub workspace_root: String,
    /// Latest user task/prompt, bounded.
    pub current_task: BoundedText,
    /// Files inspected during the session.
    pub inspected: Vec<InspectedFile>,
    /// Files modified by AETHER write/patch in this session.
    pub modified: Vec<String>,
    /// Targeted excerpts retained for reconstruction.
    pub excerpts: Vec<FileExcerpt>,
    /// Compact recent tool results.
    pub tool_summaries: Vec<CompactToolSummary>,
    /// Last observed git state.
    pub git: Option<GitSnapshot>,
    /// Opaque Rainy continuation, never raw chain-of-thought.
    pub continuation: Option<OpaqueContinuation>,
    /// Model identifier used by the session.
    pub model: Option<String>,
    /// Whether resume detected a workspace or file-state change.
    pub workspace_changed: bool,
}

impl ContextSnapshot {
    /// Construct an empty snapshot for a workspace.
    pub fn new(workspace_root: impl Into<String>, model: Option<String>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            current_task: BoundedText::new("", MAX_TASK_BYTES),
            inspected: Vec::new(),
            modified: Vec::new(),
            excerpts: Vec::new(),
            tool_summaries: Vec::new(),
            git: None,
            continuation: None,
            model,
            workspace_changed: false,
        }
    }

    /// Minimize this snapshot for durable storage.
    ///
    /// Persistence keeps workspace, model, inspected path/hash/range metadata, modified
    /// paths, and safe tool summaries. It omits raw prompts, assistant text, file excerpt
    /// bodies, git diffs, and shell/process/git stdout. `payload_contains_secrets` is not
    /// the primary boundary; this shape is.
    pub fn persistable(&self) -> Self {
        Self {
            workspace_root: self.workspace_root.clone(),
            current_task: BoundedText::new("", MAX_TASK_BYTES),
            inspected: self.inspected.clone(),
            modified: self.modified.clone(),
            excerpts: Vec::new(),
            tool_summaries: self
                .tool_summaries
                .iter()
                .cloned()
                .map(persistable_tool_summary)
                .collect(),
            git: self.git.as_ref().map(|git| GitSnapshot {
                branch: git.branch.clone(),
                status: BoundedText::new("", MAX_GIT_SNAPSHOT_BYTES),
                available: git.available,
            }),
            continuation: self.continuation.as_ref().and_then(persistable_continuation),
            model: self.model.clone(),
            workspace_changed: self.workspace_changed,
        }
    }

    /// Return true when this snapshot has no working-state yet.
    pub fn is_empty(&self) -> bool {
        self.current_task.as_str().is_empty()
            && self.inspected.is_empty()
            && self.modified.is_empty()
            && self.excerpts.is_empty()
            && self.tool_summaries.is_empty()
            && self.git.is_none()
    }
}

/// Sanitize continuation JSON so only opaque identity keys are retained.
pub fn persistable_continuation(continuation: &OpaqueContinuation) -> Option<OpaqueContinuation> {
    let value = &continuation.0;
    if value.is_null() {
        return None;
    }
    let mut kept = serde_json::Map::new();
    match value {
        Value::Object(map) => {
            for (key, item) in map {
                if !is_safe_continuation_key(key) {
                    continue;
                }
                if let Some(text) = item.as_str()
                    && !text.is_empty()
                    && text.len() <= 256
                {
                    kept.insert(key.clone(), Value::String(text.to_owned()));
                }
            }
        }
        _ => return None,
    }
    if kept.is_empty() { None } else { Some(OpaqueContinuation(Value::Object(kept))) }
}

fn persistable_tool_summary(summary: CompactToolSummary) -> CompactToolSummary {
    match summary.tool.as_str() {
        "shell" | "process" | "git" => CompactToolSummary {
            tool: summary.tool.clone(),
            ok: summary.ok,
            summary: BoundedText::new(
                if summary.ok {
                    format!("{} ok", summary.tool)
                } else {
                    format!("{} failed", summary.tool)
                },
                MAX_COMPACT_SUMMARY_BYTES,
            ),
        },
        _ => summary,
    }
}

/// Defense-in-depth scan for obvious secret keys or `RAINY_API_KEY=` assignments.
///
/// This is not complete secret detection. Persistence relies on minimized records
/// (`ContextSnapshot::persistable`) rather than this heuristic.
pub fn payload_contains_secrets(value: &Value) -> bool {
    match value {
        Value::Object(map) => {
            map.iter().any(|(key, item)| is_secret_key(key) || payload_contains_secrets(item))
        }
        Value::Array(items) => items.iter().any(payload_contains_secrets),
        Value::String(text) => looks_like_secret_assignment(text),
        _ => false,
    }
}

fn is_safe_continuation_key(key: &str) -> bool {
    matches!(key, "previous_response_id" | "fake_step")
}

fn is_secret_key(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase();
    matches!(
        normalized.as_str(),
        "rainy_api_key"
            | "api_key"
            | "apikey"
            | "authorization"
            | "password"
            | "secret"
            | "token"
            | "access_token"
            | "private_key"
    ) || normalized.contains("api_key")
        || normalized.contains("secret")
}

fn looks_like_secret_assignment(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    upper.contains("RAINY_API_KEY=") || upper.contains("RAINY_API_KEY\":")
}

/// Compact one tool result into a bounded local summary. This does not call a model.
pub fn compact_tool_result(name: &str, result: &ToolResult) -> CompactToolSummary {
    let summary = result
        .data
        .as_ref()
        .and_then(|data| compact_from_data(name, data))
        .unwrap_or_else(|| compact_from_text(name, result.ok, result.output.as_str()));
    CompactToolSummary {
        tool: name.to_owned(),
        ok: result.ok,
        summary: BoundedText::new(summary, MAX_COMPACT_SUMMARY_BYTES),
    }
}

fn compact_from_data(name: &str, data: &Value) -> Option<String> {
    match name {
        "search" => compact_search(data),
        "find" => compact_find(data),
        "list" => compact_list(data),
        "read" => compact_read(data),
        "write" => compact_write(data),
        "patch" => compact_patch(data),
        "shell" | "process" | "git" => compact_command(data),
        _ => None,
    }
}

fn compact_search(data: &Value) -> Option<String> {
    let matches = data.get("matches")?.as_array()?;
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for item in matches {
        if let Some(path) = item.get("path").and_then(Value::as_str)
            && seen.insert(path.to_owned())
        {
            files.push(path.to_owned());
        }
        if files.len() >= MAX_SUMMARY_PATHS {
            break;
        }
    }
    let truncated = data.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    let mut text = format!("searched {} matches; relevant:", matches.len());
    for path in &files {
        text.push_str("\n- ");
        text.push_str(path);
    }
    if truncated {
        text.push_str("\ntruncated: true");
    }
    Some(text)
}

fn compact_find(data: &Value) -> Option<String> {
    let paths = data.get("paths")?.as_array()?;
    let truncated = data.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    let mut text = format!("found {} paths", paths.len());
    for path in paths.iter().take(MAX_SUMMARY_PATHS) {
        if let Some(path) = path.as_str() {
            text.push_str("\n- ");
            text.push_str(path);
        }
    }
    if truncated {
        text.push_str("\ntruncated: true");
    }
    Some(text)
}

fn compact_list(data: &Value) -> Option<String> {
    let entries = data.get("entries")?.as_array()?;
    let truncated = data.get("truncated").and_then(Value::as_bool).unwrap_or(false);
    Some(format!("listed {} entries; truncated: {truncated}", entries.len()))
}

fn compact_read(data: &Value) -> Option<String> {
    let files = data.get("files")?.as_array()?;
    let mut text = format!("read {} files", files.len());
    for file in files.iter().take(MAX_SUMMARY_PATHS) {
        let path = file.get("path").and_then(Value::as_str).unwrap_or("unknown");
        let hash = file.get("content_hash").and_then(Value::as_str);
        let truncated = file.get("truncated").and_then(Value::as_bool).unwrap_or(false);
        text.push_str("\n- ");
        text.push_str(path);
        if let Some(hash) = hash {
            text.push_str(" hash=");
            text.push_str(hash);
        }
        if truncated {
            text.push_str(" truncated");
        }
    }
    Some(text)
}

fn compact_write(data: &Value) -> Option<String> {
    let path = data.get("path").and_then(Value::as_str)?;
    let bytes = data.get("bytes").and_then(Value::as_u64).unwrap_or(0);
    let hash = data.get("hash").and_then(Value::as_str).unwrap_or("unknown");
    let replaced = data.get("replaced").and_then(Value::as_bool).unwrap_or(false);
    Some(format!("wrote {path} ({bytes} bytes, hash={hash}, replaced={replaced})"))
}

fn compact_patch(data: &Value) -> Option<String> {
    let files = data.get("files")?.as_array()?;
    let dry_run = data.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let mut text = format!("patched {} files; dry_run={dry_run}", files.len());
    for file in files.iter().take(MAX_SUMMARY_PATHS) {
        let path = file.get("path").and_then(Value::as_str).unwrap_or("unknown");
        let new_hash = file.get("new_hash").and_then(Value::as_str).unwrap_or("unknown");
        text.push_str("\n- ");
        text.push_str(path);
        text.push_str(" hash=");
        text.push_str(new_hash);
    }
    Some(text)
}

fn compact_command(data: &Value) -> Option<String> {
    let program = data.get("program").and_then(Value::as_str).unwrap_or("command");
    let args = data
        .get("args")
        .and_then(Value::as_array)
        .map(|args| args.iter().filter_map(Value::as_str).take(8).collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let success = data.get("success").and_then(Value::as_bool).unwrap_or(false);
    let exit_code = data.get("exit_code").and_then(Value::as_i64);
    let stdout = data.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = data.get("stderr").and_then(Value::as_str).unwrap_or("");
    let mut text = program.to_owned();
    if !args.is_empty() {
        text.push(' ');
        text.push_str(&args);
    }
    if let Some(code) = exit_code {
        text.push_str(&format!(" exit={code}"));
    }
    text.push_str(if success { " ok" } else { " failed" });
    if let Some(tests) = compact_test_output(stdout, stderr) {
        text.push('\n');
        text.push_str(&tests);
    } else {
        let preview = first_nonempty_lines(if stdout.is_empty() { stderr } else { stdout }, 4);
        if !preview.is_empty() {
            text.push('\n');
            text.push_str(&preview);
        }
    }
    Some(text)
}

fn compact_from_text(name: &str, ok: bool, output: &str) -> String {
    if let Some(tests) = compact_test_output(output, "") {
        return format!("{name} {}: {tests}", if ok { "ok" } else { "failed" });
    }
    let preview = first_nonempty_lines(output, 4);
    if preview.is_empty() {
        format!("{name} {}", if ok { "ok" } else { "failed" })
    } else {
        format!("{name} {}:\n{preview}", if ok { "ok" } else { "failed" })
    }
}

fn compact_test_output(stdout: &str, stderr: &str) -> Option<String> {
    let combined = [stdout, stderr].join("\n");
    let mut passed = None;
    let mut failed = None;
    for line in combined.lines() {
        if let Some(count) = count_before(line, "passed") {
            passed = Some(count);
        }
        if let Some(count) = count_before(line, "failed") {
            failed = Some(count);
        }
    }
    let failed_names = failed_test_names(&combined);
    match (passed, failed) {
        (Some(passed), Some(failed)) => {
            let mut text = format!("tests:\n{passed} passed\n{failed} failed");
            if !failed_names.is_empty() {
                text.push(':');
                for name in failed_names {
                    text.push_str("\n- ");
                    text.push_str(&name);
                }
            }
            Some(text)
        }
        (Some(passed), None) => Some(format!("tests:\n{passed} passed")),
        (None, Some(failed)) if failed > 0 => {
            let mut text = format!("tests:\n{failed} failed");
            for name in failed_names {
                text.push_str("\n- ");
                text.push_str(&name);
            }
            Some(text)
        }
        _ => None,
    }
}

fn failed_test_names(output: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    for line in output.lines() {
        let trimmed = line.trim();
        let name = if let Some(rest) = trimmed.strip_prefix("test ") {
            rest.split_whitespace().next().filter(|_| trimmed.contains("FAILED"))
        } else if trimmed.contains("FAILED") {
            trimmed.split_whitespace().next()
        } else {
            None
        };
        if let Some(name) = name
            && name != "FAILED"
            && seen.insert(name.to_owned())
        {
            names.push(name.to_owned());
        }
        if names.len() >= MAX_SUMMARY_PATHS {
            break;
        }
    }
    names
}

fn count_before(line: &str, word: &str) -> Option<u64> {
    let lower = line.to_ascii_lowercase();
    let index = lower.rfind(word)?;
    lower[..index]
        .split(|ch: char| !ch.is_ascii_digit())
        .rfind(|token| !token.is_empty())?
        .parse()
        .ok()
}

fn first_nonempty_lines(text: &str, max_lines: usize) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .take(max_lines)
        .collect::<Vec<_>>()
        .join("\n")
}

/// Merge overlapping inclusive line ranges in place.
pub fn merge_line_ranges(ranges: &mut Vec<LineRange>) {
    if ranges.len() < 2 {
        return;
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<LineRange> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        if let Some(last) = merged.last_mut()
            && range.start <= last.end.saturating_add(1)
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    *ranges = merged;
}

/// Coding-agent discovery guidance included in reconstructed model context.
pub const CONTEXT_GUIDANCE: &str = "Use find/list → search → targeted read → reason → patch/write → test. \
Do not scan the whole repository or reread unchanged current files. \
Before write/patch, inspect git status/diff and do not overwrite unrelated user changes.";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolCallId;

    #[test]
    fn context_deduplication_merges_ranges_and_paths() {
        let mut ranges = vec![
            LineRange { start: 10, end: 20 },
            LineRange { start: 18, end: 30 },
            LineRange { start: 40, end: 42 },
        ];
        merge_line_ranges(&mut ranges);
        assert_eq!(
            ranges,
            vec![LineRange { start: 10, end: 30 }, LineRange { start: 40, end: 42 }]
        );
    }

    #[test]
    fn compact_tool_result_summarizes_search_and_tests_without_raw_output() {
        let search = ToolResult::success_json(
            ToolCallId::new("c1").unwrap(),
            serde_json::json!({
                "matches": [
                    {"path": "src/agent.rs", "line": 1, "kind": "match", "text": "fn run"},
                    {"path": "src/context.rs", "line": 4, "kind": "match", "text": "struct Context"},
                    {"path": "src/agent.rs", "line": 9, "kind": "match", "text": "again"}
                ],
                "truncated": false
            }),
            64 * 1024,
        );
        let summary = compact_tool_result("search", &search);
        assert!(summary.summary.as_str().contains("searched 3 matches"));
        assert!(summary.summary.as_str().contains("src/agent.rs"));
        assert!(summary.summary.as_str().contains("src/context.rs"));
        assert!(!summary.summary.as_str().contains("fn run"));

        let tests = ToolResult::success_json(
            ToolCallId::new("c2").unwrap(),
            serde_json::json!({
                "program": "cargo",
                "args": ["test"],
                "success": false,
                "exit_code": 101,
                "stdout": "test context_restore ... FAILED\ntest stale_hash ... FAILED\ntest result: FAILED. 42 passed; 2 failed",
                "stderr": ""
            }),
            64 * 1024,
        );
        let summary = compact_tool_result("shell", &tests);
        assert!(summary.summary.as_str().contains("42 passed"));
        assert!(summary.summary.as_str().contains("2 failed"));
        assert!(summary.summary.as_str().contains("context_restore"));
        assert!(summary.summary.as_str().contains("stale_hash"));
    }

    #[test]
    fn persistable_context_omits_raw_prompt_excerpts_and_command_output() {
        let mut snapshot = ContextSnapshot::new("/workspace", Some("model-a".to_owned()));
        snapshot.current_task = BoundedText::new("fix the failing tests", MAX_TASK_BYTES);
        snapshot.excerpts.push(FileExcerpt {
            path: "src/lib.rs".to_owned(),
            start_line: 1,
            end_line: 2,
            text: BoundedText::new("fn secret() {}", MAX_EXCERPT_BYTES),
            content_hash: Some("abc".to_owned()),
        });
        snapshot.tool_summaries.push(CompactToolSummary {
            tool: "shell".to_owned(),
            ok: false,
            summary: BoundedText::new("cargo test\nRAINY_API_KEY=must-not-store", 1024),
        });
        snapshot.git = Some(GitSnapshot {
            branch: Some("main".to_owned()),
            status: BoundedText::new("M src/lib.rs\ndiff --git", MAX_GIT_SNAPSHOT_BYTES),
            available: true,
        });
        let persisted = snapshot.persistable();
        assert!(persisted.current_task.as_str().is_empty());
        assert!(persisted.excerpts.is_empty());
        assert_eq!(persisted.tool_summaries[0].summary.as_str(), "shell failed");
        assert!(!persisted.tool_summaries[0].summary.as_str().contains("RAINY_API_KEY"));
        assert_eq!(persisted.git.as_ref().unwrap().branch.as_deref(), Some("main"));
        assert!(persisted.git.as_ref().unwrap().status.as_str().is_empty());
    }

    #[test]
    fn persistable_continuation_keeps_identity_and_drops_secrets() {
        let dirty = OpaqueContinuation(serde_json::json!({
            "previous_response_id": "resp_1",
            "api_key": "secret",
            "reasoning": "hidden"
        }));
        let clean = persistable_continuation(&dirty).unwrap();
        assert_eq!(clean.0, serde_json::json!({"previous_response_id": "resp_1"}));
        assert!(payload_contains_secrets(&serde_json::json!({"RAINY_API_KEY": "x"})));
        assert!(!payload_contains_secrets(&serde_json::json!({"previous_response_id": "resp_1"})));
    }

    #[test]
    fn context_bounds_are_explicit() {
        assert_eq!(MAX_SESSION_FILE_BYTES, 2 * 1024 * 1024);
        assert_eq!(MAX_STORED_TURNS, 64);
        assert_eq!(MAX_STORED_TOOL_SUMMARIES, 32);
        assert_eq!(MAX_CONTEXT_ITEMS, 48);
        assert_eq!(MAX_FILE_EXCERPTS, 16);
        assert_eq!(MAX_EXCERPT_BYTES, 4 * 1024);
        assert_eq!(MAX_INSPECTED_FILES, 64);
    }
}
