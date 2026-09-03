#![deny(unsafe_code)]

//! A small browser-owned AETHER workspace runtime.
//!
//! The native agent and its filesystem/process tools stay in their native crates. This crate
//! reuses the bounded task and workflow semantics from aether-core and provides a deterministic
//! in-memory repository for the hosted WebMCP vertical slice.

use aether_core::{BoundedText, WorkflowPhase, WorkflowState, WorkspacePath};
use blake3::hash;
use serde_json::{Map, Value, json};
use wasm_bindgen::prelude::wasm_bindgen;

const MAX_REQUEST_BYTES: usize = 8 * 1024;
const MAX_RESULT_BYTES: usize = 48 * 1024;
const MAX_PATH_BYTES: usize = 128;
const MAX_QUERY_BYTES: usize = 256;
const MAX_PATCH_BYTES: usize = 2 * 1024;
const MAX_RATIONALE_BYTES: usize = 512;
const MAX_FILE_BYTES: usize = 16 * 1024;
const MAX_READ_LINES: usize = 120;
const MAX_SEARCH_RESULTS: usize = 12;

const TASK_ID: &str = "auth-token-blank-input";
const TASK_TITLE: &str = "Reject blank API tokens before gateway handoff";
const TASK_OBJECTIVE: &str =
    "Fix token validation so whitespace-only API tokens are rejected before gateway handoff";
const TASK_ISSUE: &str = "The validator checks length before normalization, so a whitespace-only token can pass the boundary when the configured minimum is reduced.";
const PATCH_PATH: &str = "src/auth.rs";
const PATCH_FIND: &str = "if token.len() < 8 {\n        return Err(TokenError::TooShort);\n    }";
const PATCH_REPLACE: &str = "let token = token.trim();\n    if token.is_empty() {\n        return Err(TokenError::Blank);\n    }\n    if token.len() < 8 {\n        return Err(TokenError::TooShort);\n    }";
const PATCH_RATIONALE: &str =
    "Normalize first, then reject blank input while preserving the existing short-token guard.";

#[derive(Clone)]
struct DemoFile {
    path: &'static str,
    role: &'static str,
    content: String,
    original_hash: String,
}

#[derive(Clone)]
struct StagedPatch {
    path: String,
    find: String,
    replace: String,
    rationale: String,
    before: String,
    after: String,
    before_hash: String,
    after_hash: String,
    lines_added: usize,
    lines_removed: usize,
    diff: String,
}

#[derive(Clone, serde::Serialize)]
struct VerificationCheck {
    name: String,
    passed: bool,
    detail: String,
}

#[derive(Clone, serde::Serialize)]
struct VerificationReport {
    status: String,
    scope: String,
    revision: u32,
    summary: String,
    checks: Vec<VerificationCheck>,
}

#[wasm_bindgen]
pub struct BrowserRuntime {
    files: Vec<DemoFile>,
    workflow: WorkflowState,
    staged: Option<StagedPatch>,
    verification: Option<VerificationReport>,
    human_decision: String,
    last_event: String,
}

impl Default for BrowserRuntime {
    fn default() -> Self {
        let files = demo_files();
        let mut workflow = WorkflowState::new();
        workflow.work.initialize_from_prompt(TASK_OBJECTIVE);
        for file in &files {
            workflow.record_relevant_file(file.path);
        }
        workflow.record_inspection();
        workflow.work.record_evidence("issue", PATCH_PATH, TASK_ISSUE, workflow.workspace_revision);

        Self {
            files,
            workflow,
            staged: None,
            verification: None,
            human_decision: "pending".to_owned(),
            last_event: "Demo repository loaded; ready for bounded review.".to_owned(),
        }
    }
}

#[wasm_bindgen]
impl BrowserRuntime {
    /// Create a fresh deterministic browser workspace.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Execute one bounded application operation and return a JSON result.
    ///
    /// The JavaScript host uses this same operation boundary for both visible controls and
    /// WebMCP callbacks. No filesystem, process, network, or arbitrary JavaScript access exists
    /// behind this method.
    pub fn invoke(&mut self, request_json: &str) -> String {
        if request_json.len() > MAX_REQUEST_BYTES {
            return self.serialized(self.failure(
                "unknown",
                "request_too_large",
                "request exceeds the browser runtime input bound",
            ));
        }

        let value = match serde_json::from_str::<Value>(request_json) {
            Ok(value) => value,
            Err(_) => {
                return self.serialized(self.failure(
                    "unknown",
                    "invalid_json",
                    "request must be a JSON object",
                ));
            }
        };
        let object = match value.as_object() {
            Some(object) => object,
            None => {
                return self.serialized(self.failure(
                    "unknown",
                    "invalid_input",
                    "request must be a JSON object",
                ));
            }
        };
        let operation = match object.get("op").and_then(Value::as_str) {
            Some(operation) if !operation.is_empty() && operation.len() <= MAX_PATH_BYTES => {
                operation
            }
            _ => {
                return self.serialized(self.failure(
                    "unknown",
                    "invalid_input",
                    "op must be a bounded non-empty string",
                ));
            }
        };

        let result = match operation {
            "inspect_workspace" => self.inspect_workspace(object),
            "read_file" => self.read_file(object),
            "search_workspace" => self.search_workspace(object),
            "propose_patch" => self.propose_patch(object),
            "verify_patch" => self.verify_patch(object),
            "get_task_state" => self.get_task_state(object),
            "accept_patch" => self.accept_patch(object),
            "reject_patch" => self.reject_patch(object),
            _ => self.failure(operation, "unknown_operation", "operation is not registered"),
        };
        self.serialized(result)
    }
}

impl BrowserRuntime {
    fn inspect_workspace(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op"]) {
            return self.failure("inspect_workspace", "invalid_input", message);
        }
        self.workflow.record_inspection();
        self.workflow.work.record_evidence(
            "workspace",
            "",
            "bounded repository inventory inspected",
            self.workflow.workspace_revision,
        );
        self.last_event =
            "Workspace inventory inspected; auth.rs is the active issue surface.".to_owned();
        let mut payload = Map::new();
        payload.insert("summary".to_owned(), json!(TASK_ISSUE));
        payload.insert("files".to_owned(), self.file_inventory());
        payload.insert(
            "suggestedPatch".to_owned(),
            json!({
                "path": PATCH_PATH,
                "find": PATCH_FIND,
                "replace": PATCH_REPLACE,
                "rationale": PATCH_RATIONALE,
            }),
        );
        payload.insert(
            "next".to_owned(),
            json!(["read_file(src/auth.rs)", "search_workspace(blank)", "propose_patch"]),
        );
        self.success("inspect_workspace", payload)
    }

    fn read_file(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op", "path", "startLine", "endLine"]) {
            return self.failure("read_file", "invalid_input", message);
        }
        let path = match required_path(object, "path") {
            Ok(path) => path,
            Err(message) => return self.failure("read_file", "invalid_input", message),
        };
        let start_line = match optional_line(object, "startLine", 1) {
            Ok(line) => line,
            Err(message) => return self.failure("read_file", "invalid_input", message),
        };
        let default_end = start_line.saturating_add((MAX_READ_LINES - 1) as u32);
        let end_line = match optional_line(object, "endLine", default_end as usize) {
            Ok(line) => line,
            Err(message) => return self.failure("read_file", "invalid_input", message),
        };
        if end_line < start_line {
            return self.failure(
                "read_file",
                "invalid_input",
                "endLine must be greater than or equal to startLine",
            );
        }

        let Some(content) = self.file_content(&path) else {
            return self.failure("read_file", "not_found", "path is not in the demo workspace");
        };
        let lines = content.lines().collect::<Vec<_>>();
        let reported_start =
            if lines.is_empty() { 1 } else { start_line.min(lines.len() as u32).max(1) };
        let reported_end =
            if lines.is_empty() { 1 } else { end_line.min(lines.len() as u32).max(reported_start) };
        let start = reported_start.saturating_sub(1) as usize;
        let end = (reported_end as usize).min(start.saturating_add(MAX_READ_LINES));
        let rendered = lines
            .iter()
            .enumerate()
            .skip(start)
            .take(end.saturating_sub(start))
            .map(|(index, line)| format!("{:>4} | {}", index + 1, line))
            .collect::<Vec<_>>()
            .join("\n");
        self.workflow.record_inspection();
        self.workflow.work.record_evidence(
            "read",
            &path,
            "bounded source excerpt inspected",
            self.workflow.workspace_revision,
        );
        self.last_event = format!("Read {path} with bounded line numbers.");
        let mut payload = Map::new();
        payload.insert("path".to_owned(), json!(path));
        payload.insert("startLine".to_owned(), json!(reported_start));
        payload.insert("endLine".to_owned(), json!(reported_end));
        payload.insert("lineCount".to_owned(), json!(lines.len()));
        payload.insert("content".to_owned(), json!(bounded(&rendered, MAX_FILE_BYTES)));
        self.success("read_file", payload)
    }

    fn search_workspace(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op", "query", "path", "maxResults"]) {
            return self.failure("search_workspace", "invalid_input", message);
        }
        let query = match required_text(object, "query", MAX_QUERY_BYTES) {
            Ok(query) => query,
            Err(message) => return self.failure("search_workspace", "invalid_input", message),
        };
        let path_filter = match optional_path(object, "path") {
            Ok(path) => path,
            Err(message) => return self.failure("search_workspace", "invalid_input", message),
        };
        let max_results = match optional_count(object, "maxResults", MAX_SEARCH_RESULTS) {
            Ok(count) => count,
            Err(message) => return self.failure("search_workspace", "invalid_input", message),
        };
        let needle = query.to_ascii_lowercase();
        let mut matches = Vec::new();
        let mut truncated = false;
        for file in &self.files {
            if path_filter.as_deref().is_some_and(|path| path != file.path) {
                continue;
            }
            for (index, line) in file.content.lines().enumerate() {
                if line.to_ascii_lowercase().contains(&needle) {
                    matches.push(json!({
                        "path": file.path,
                        "line": index + 1,
                        "preview": bounded(line.trim(), 240),
                    }));
                    if matches.len() >= max_results {
                        truncated = true;
                        break;
                    }
                }
            }
            if matches.len() >= max_results {
                break;
            }
        }
        self.workflow.work.record_evidence(
            "search",
            path_filter.as_deref().unwrap_or(""),
            &format!("bounded literal search for {query}"),
            self.workflow.workspace_revision,
        );
        self.last_event = format!("Searched the workspace for {query}.");
        let mut payload = Map::new();
        payload.insert("query".to_owned(), json!(query));
        payload.insert("matches".to_owned(), Value::Array(matches));
        payload.insert("truncated".to_owned(), json!(truncated));
        self.success("search_workspace", payload)
    }

    fn propose_patch(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op", "path", "find", "replace", "rationale"]) {
            return self.failure("propose_patch", "invalid_input", message);
        }
        let path = match required_path(object, "path") {
            Ok(path) => path,
            Err(message) => return self.failure("propose_patch", "invalid_input", message),
        };
        let find = match required_text(object, "find", MAX_PATCH_BYTES) {
            Ok(value) => value,
            Err(message) => return self.failure("propose_patch", "invalid_input", message),
        };
        let replace = match required_text(object, "replace", MAX_PATCH_BYTES) {
            Ok(value) => value,
            Err(message) => return self.failure("propose_patch", "invalid_input", message),
        };
        let rationale = match required_text(object, "rationale", MAX_RATIONALE_BYTES) {
            Ok(value) => value,
            Err(message) => return self.failure("propose_patch", "invalid_input", message),
        };
        if contains_secret_marker(&find)
            || contains_secret_marker(&replace)
            || contains_secret_marker(&rationale)
        {
            return self.failure(
                "propose_patch",
                "sensitive_input",
                "secret-like content is not accepted by the browser workspace",
            );
        }
        let Some(before) = self.file_content(&path) else {
            return self.failure(
                "propose_patch",
                "not_found",
                "path is not in the deterministic demo workspace",
            );
        };
        let occurrence_count = before.matches(&find).count();
        if occurrence_count != 1 {
            return self.failure(
                "propose_patch",
                "precondition_failed",
                format!("find text must match exactly once; observed {occurrence_count}"),
            );
        }
        let after = before.replacen(&find, &replace, 1);
        if after.len() > MAX_FILE_BYTES {
            return self.failure(
                "propose_patch",
                "patch_too_large",
                "resulting file exceeds the browser workspace bound",
            );
        }
        let patch = StagedPatch {
            path: path.clone(),
            find,
            replace,
            rationale,
            before_hash: digest(&before),
            after_hash: digest(&after),
            lines_added: added_lines(&before, &after),
            lines_removed: removed_lines(&before, &after),
            diff: unified_diff(&path, &before, &after),
            before,
            after,
        };
        self.staged = Some(patch.clone());
        self.verification = None;
        self.human_decision = "awaiting_review".to_owned();
        self.workflow.begin_modification();
        self.last_event = format!("Staged a bounded proposal for {path}; human review required.");
        let mut payload = Map::new();
        payload.insert("staged".to_owned(), json!(true));
        payload.insert("patch".to_owned(), staged_json(&patch));
        payload.insert(
            "humanActionRequired".to_owned(),
            json!("Accept or Reject in the workspace before the change is applied."),
        );
        self.success("propose_patch", payload)
    }

    fn verify_patch(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op", "scope"]) {
            return self.failure("verify_patch", "invalid_input", message);
        }
        let scope = match object.get("scope").and_then(Value::as_str) {
            Some("staged") => "staged",
            Some("accepted") => "accepted",
            _ => {
                return self.failure(
                    "verify_patch",
                    "invalid_input",
                    "scope must be either staged or accepted",
                );
            }
        };
        match scope {
            "staged" => {
                let Some(staged) = self.staged.clone() else {
                    return self.failure(
                        "verify_patch",
                        "nothing_staged",
                        "stage a proposal before verifying the staged view",
                    );
                };
                let mut files = self.files.clone();
                let Some(file) = files.iter_mut().find(|file| file.path == staged.path) else {
                    return self.failure(
                        "verify_patch",
                        "not_found",
                        "staged path no longer exists",
                    );
                };
                file.content = staged.after;
                let report = verification_report(&files, &staged.path, "staged", self.revision());
                self.verification = Some(report.clone());
                self.workflow.work.record_evidence(
                    "verification_preview",
                    &staged.path,
                    "browser-safe checks ran against the staged view",
                    self.workflow.workspace_revision,
                );
                self.last_event = format!("Verified the staged proposal for {}.", staged.path);
                let mut payload = Map::new();
                payload.insert("verification".to_owned(), json!(report));
                self.success("verify_patch", payload)
            }
            "accepted" => {
                if self.human_decision != "accepted" {
                    return self.failure(
                        "verify_patch",
                        "human_acceptance_required",
                        "the accepted scope is available only after the human accepts the patch",
                    );
                }
                let Some(path) = self.staged_path_or_last_changed() else {
                    return self.failure(
                        "verify_patch",
                        "nothing_accepted",
                        "no accepted patch exists",
                    );
                };
                let report = verification_report(&self.files, &path, "accepted", self.revision());
                self.record_verification(&path, &report);
                self.verification = Some(report.clone());
                self.last_event = format!("Verified the accepted change for {path}.");
                let mut payload = Map::new();
                payload.insert("verification".to_owned(), json!(report));
                self.success("verify_patch", payload)
            }
            _ => self.failure("verify_patch", "invalid_input", "unsupported verification scope"),
        }
    }

    fn get_task_state(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op"]) {
            return self.failure("get_task_state", "invalid_input", message);
        }
        let mut payload = Map::new();
        payload.insert("state".to_owned(), self.state_json());
        self.success_without_nested_state("get_task_state", payload)
    }

    fn accept_patch(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op"]) {
            return self.failure("accept_patch", "invalid_input", message);
        }
        let Some(staged) = self.staged.clone() else {
            return self.failure(
                "accept_patch",
                "nothing_staged",
                "there is no staged patch to accept",
            );
        };
        let Some(file) = self.files.iter_mut().find(|file| file.path == staged.path) else {
            return self.failure("accept_patch", "not_found", "staged path no longer exists");
        };
        if digest(&file.content) != staged.before_hash {
            return self.failure(
                "accept_patch",
                "stale_patch",
                "the file changed after the proposal was staged; review a fresh proposal",
            );
        }
        file.content = staged.after.clone();
        let paths = vec![staged.path.clone()];
        self.workflow.record_mutation_scope("human_accept", &paths);
        let report = verification_report(&self.files, &staged.path, "accepted", self.revision());
        self.record_verification(&staged.path, &report);
        self.workflow.work.record_evidence(
            "human_acceptance",
            &staged.path,
            "human accepted the staged patch; the exact accepted view was verified",
            self.workflow.workspace_revision,
        );
        self.staged = None;
        self.verification = Some(report.clone());
        self.human_decision = "accepted".to_owned();
        self.last_event =
            format!("Human accepted {}; bounded verification completed.", staged.path);
        let mut payload = Map::new();
        payload.insert("decision".to_owned(), json!("accepted"));
        payload.insert("applied".to_owned(), json!(true));
        payload.insert("verification".to_owned(), json!(report));
        self.success("accept_patch", payload)
    }

    fn reject_patch(&mut self, object: &Map<String, Value>) -> Value {
        if let Err(message) = ensure_keys(object, &["op"]) {
            return self.failure("reject_patch", "invalid_input", message);
        }
        let Some(staged) = self.staged.take() else {
            return self.failure(
                "reject_patch",
                "nothing_staged",
                "there is no staged patch to reject",
            );
        };
        self.workflow.phase = WorkflowPhase::Inspect;
        self.workflow.sync_director(&[], false);
        self.workflow.work.record_evidence(
            "human_rejection",
            &staged.path,
            "human rejected the staged patch; repository content is unchanged",
            self.workflow.workspace_revision,
        );
        self.verification = Some(VerificationReport {
            status: "not_run".to_owned(),
            scope: "rejected".to_owned(),
            revision: self.revision(),
            summary: "The proposal was rejected before application.".to_owned(),
            checks: Vec::new(),
        });
        self.human_decision = "rejected".to_owned();
        self.last_event =
            format!("Human rejected the proposal for {}; no file changed.", staged.path);
        let mut payload = Map::new();
        payload.insert("decision".to_owned(), json!("rejected"));
        payload.insert("applied".to_owned(), json!(false));
        self.success("reject_patch", payload)
    }

    fn record_verification(&mut self, path: &str, report: &VerificationReport) {
        let affected_paths = vec![path.to_owned()];
        self.workflow.record_verification_scope(
            report.status == "passed",
            "aether-web contract checks",
            &affected_paths,
            Vec::new(),
        );
        if report.status == "passed" {
            self.workflow.complete_if_ready(&affected_paths);
        }
    }

    fn staged_path_or_last_changed(&self) -> Option<String> {
        self.files
            .iter()
            .find(|file| digest(&file.content) != file.original_hash)
            .map(|file| file.path.to_owned())
    }

    fn file_content(&self, path: &str) -> Option<String> {
        self.files.iter().find(|file| file.path == path).map(|file| file.content.clone())
    }

    fn file_inventory(&self) -> Value {
        Value::Array(
            self.files
                .iter()
                .map(|file| {
                    json!({
                        "path": file.path,
                        "role": file.role,
                        "bytes": file.content.len(),
                        "changed": digest(&file.content) != file.original_hash,
                    })
                })
                .collect(),
        )
    }

    fn revision(&self) -> u32 {
        self.workflow.workspace_revision
    }

    fn state_json(&self) -> Value {
        let verification =
            self.verification.as_ref().map(|report| json!(report)).unwrap_or_else(|| {
                json!({
                    "status": "not_run",
                    "scope": "none",
                    "revision": self.revision(),
                    "summary": "No verification has been run for the current view.",
                    "checks": [],
                })
            });
        let task_status = if self.staged.is_some() {
            "awaiting_human"
        } else if self.human_decision == "accepted"
            && self.verification.as_ref().is_some_and(|report| report.status == "passed")
        {
            "verified"
        } else if self.human_decision == "rejected" {
            "unchanged"
        } else {
            "ready"
        };
        json!({
            "schemaVersion": 1,
            "runtime": "aether-web",
            "revision": self.revision(),
            "task": {
                "id": TASK_ID,
                "title": TASK_TITLE,
                "objective": TASK_OBJECTIVE,
                "issue": TASK_ISSUE,
                "status": task_status,
                "acceptanceCriteria": [
                    {"key": "blank-input", "description": "Whitespace-only tokens are rejected", "satisfied": self.acceptance_satisfied()},
                    {"key": "short-input", "description": "The existing short-token guard remains enforced", "satisfied": self.short_guard_satisfied()},
                ],
            },
            "files": self.file_inventory(),
            "stagedPatch": self.staged.as_ref().map(staged_json),
            "verification": verification,
            "humanDecision": self.human_decision,
            "lastEvent": bounded(&self.last_event, 512),
            "evidence": self.evidence_json(),
            "workflow": {
                "phase": self.workflow.phase.as_str(),
                "directorPhase": self.workflow.director.phase.as_str(),
                "workspaceRevision": self.workflow.workspace_revision,
                "inspections": self.workflow.progress.inspections,
                "mutations": self.workflow.progress.mutations,
                "verifications": self.workflow.progress.verifications,
            },
        })
    }

    fn evidence_json(&self) -> Value {
        Value::Array(
            self.workflow
                .work
                .evidence
                .iter()
                .rev()
                .take(12)
                .map(|evidence| {
                    json!({
                        "kind": evidence.kind.as_str(),
                        "path": evidence.path.as_str(),
                        "detail": evidence.detail.as_str(),
                        "revision": evidence.revision,
                        "valid": evidence.valid,
                    })
                })
                .collect(),
        )
    }

    fn acceptance_satisfied(&self) -> bool {
        self.files.iter().find(|file| file.path == PATCH_PATH).is_some_and(|file| {
            file.content.contains("let token = token.trim();")
                && file.content.contains("token.is_empty()")
                && file.content.contains("TokenError::Blank")
        })
    }

    fn short_guard_satisfied(&self) -> bool {
        self.files
            .iter()
            .find(|file| file.path == PATCH_PATH)
            .is_some_and(|file| file.content.contains("token.len() < 8"))
    }

    fn success(&self, tool: &str, mut payload: Map<String, Value>) -> Value {
        payload.insert("ok".to_owned(), json!(true));
        payload.insert("tool".to_owned(), json!(tool));
        payload.insert("state".to_owned(), self.state_json());
        Value::Object(payload)
    }

    fn success_without_nested_state(&self, tool: &str, mut payload: Map<String, Value>) -> Value {
        payload.insert("ok".to_owned(), json!(true));
        payload.insert("tool".to_owned(), json!(tool));
        Value::Object(payload)
    }

    fn failure(&self, tool: &str, code: &str, message: impl AsRef<str>) -> Value {
        json!({
            "ok": false,
            "tool": bounded(tool, MAX_PATH_BYTES),
            "error": {
                "code": bounded(code, MAX_PATH_BYTES),
                "message": bounded(message.as_ref(), MAX_RATIONALE_BYTES),
            },
            "state": self.state_json(),
        })
    }

    fn serialized(&self, value: Value) -> String {
        let serialized = match serde_json::to_string(&value) {
            Ok(serialized) => serialized,
            Err(_) => "{\"ok\":false,\"error\":{\"code\":\"serialization_failed\"}}".to_owned(),
        };
        if serialized.len() <= MAX_RESULT_BYTES {
            serialized
        } else {
            "{\"ok\":false,\"error\":{\"code\":\"result_too_large\",\"message\":\"result exceeded browser bound\"}}".to_owned()
        }
    }
}

fn demo_files() -> Vec<DemoFile> {
    [
        (
            "README.md",
            "repository guide",
            "# AETHER Auth Demo\n\nThis deterministic repository models a token-validation boundary.\n\n## Active task\n\nReject whitespace-only API tokens before gateway handoff.\n",
        ),
        (
            "src/auth.rs",
            "active issue",
            "pub enum TokenError {\n    TooShort,\n    Blank,\n}\n\npub fn validate_token(token: &str) -> Result<(), TokenError> {\n    if token.len() < 8 {\n        return Err(TokenError::TooShort);\n    }\n    Ok(())\n}\n",
        ),
        (
            "tests/auth.rs",
            "regression fixture",
            "#[test]\nfn blank_tokens_are_rejected() {\n    assert_eq!(validate_token(\"        \\t\"), Err(TokenError::Blank));\n}\n\n#[test]\nfn short_tokens_are_still_rejected() {\n    assert_eq!(validate_token(\"abc\"), Err(TokenError::TooShort));\n}\n",
        ),
        (
            "Cargo.toml",
            "project manifest",
            "[package]\nname = \"aether-auth-demo\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        ),
    ]
    .into_iter()
    .map(|(path, role, content)| DemoFile {
        path,
        role,
        content: content.to_owned(),
        original_hash: digest(content),
    })
    .collect()
}

fn verification_report(
    files: &[DemoFile],
    path: &str,
    scope: &str,
    revision: u32,
) -> VerificationReport {
    let auth = files
        .iter()
        .find(|file| file.path == PATCH_PATH)
        .map(|file| file.content.as_str())
        .unwrap_or("");
    let tests = files
        .iter()
        .find(|file| file.path == "tests/auth.rs")
        .map(|file| file.content.as_str())
        .unwrap_or("");
    let blank_behavior = evaluate_token("        \t") == "blank";
    let short_behavior = evaluate_token("abc") == "too_short";
    let valid_behavior = evaluate_token("sk_live_demo") == "accepted";
    let checks = vec![
        VerificationCheck {
            name: "blank token behavior".to_owned(),
            passed: auth.contains("let token = token.trim();")
                && auth.contains("token.is_empty()")
                && auth.contains("TokenError::Blank")
                && blank_behavior,
            detail: "Whitespace-only input reaches the explicit blank-token branch.".to_owned(),
        },
        VerificationCheck {
            name: "short token regression".to_owned(),
            passed: auth.contains("token.len() < 8") && short_behavior,
            detail: "The existing minimum-length behavior remains present.".to_owned(),
        },
        VerificationCheck {
            name: "valid token behavior".to_owned(),
            passed: valid_behavior && tests.contains("blank_tokens_are_rejected"),
            detail: "A normal token is accepted and the regression fixture is present.".to_owned(),
        },
    ];
    let passed = checks.iter().all(|check| check.passed);
    VerificationReport {
        status: if passed { "passed" } else { "failed" }.to_owned(),
        scope: scope.to_owned(),
        revision,
        summary: if passed {
            format!("{scope} view for {path} passed 3 bounded contract checks.")
        } else {
            format!("{scope} view for {path} failed one or more bounded contract checks.")
        },
        checks,
    }
}

fn evaluate_token(token: &str) -> &'static str {
    let token = token.trim();
    if token.is_empty() {
        "blank"
    } else if token.len() < 8 {
        "too_short"
    } else {
        "accepted"
    }
}

fn staged_json(patch: &StagedPatch) -> Value {
    json!({
        "path": patch.path,
        "find": bounded(&patch.find, MAX_PATCH_BYTES),
        "replace": bounded(&patch.replace, MAX_PATCH_BYTES),
        "rationale": patch.rationale,
        "beforeHash": patch.before_hash,
        "afterHash": patch.after_hash,
        "linesAdded": patch.lines_added,
        "linesRemoved": patch.lines_removed,
        "diff": patch.diff,
        "before": bounded(&patch.before, MAX_FILE_BYTES),
        "after": bounded(&patch.after, MAX_FILE_BYTES),
    })
}

fn ensure_keys(object: &Map<String, Value>, allowed: &[&str]) -> Result<(), String> {
    object
        .keys()
        .find(|key| !allowed.contains(&key.as_str()))
        .map_or(Ok(()), |key| Err(format!("unknown field: {key}")))
}

fn required_text(
    object: &Map<String, Value>,
    key: &str,
    max_bytes: usize,
) -> Result<String, String> {
    let value =
        object.get(key).and_then(Value::as_str).ok_or_else(|| format!("{key} must be a string"))?;
    if value.is_empty() || value.len() > max_bytes || value.contains('\0') {
        return Err(format!("{key} must be between 1 and {max_bytes} bytes"));
    }
    Ok(value.to_owned())
}

fn required_path(object: &Map<String, Value>, key: &str) -> Result<String, String> {
    let value = required_text(object, key, MAX_PATH_BYTES)?;
    normalize_path(&value)
}

fn optional_path(object: &Map<String, Value>, key: &str) -> Result<Option<String>, String> {
    object.get(key).map(|_| required_path(object, key)).transpose()
}

fn normalize_path(path: &str) -> Result<String, String> {
    let normalized = WorkspacePath::new(path)
        .map_err(|_| "path must be relative and cannot escape the workspace".to_owned())?
        .display();
    if normalized == "." {
        Err("path must identify a workspace file".to_owned())
    } else {
        Ok(normalized)
    }
}

fn optional_line(object: &Map<String, Value>, key: &str, default: usize) -> Result<u32, String> {
    let Some(value) = object.get(key) else {
        return Ok(default as u32);
    };
    let line = value.as_u64().ok_or_else(|| format!("{key} must be a positive integer"))?;
    if line == 0 || line > u32::MAX as u64 {
        return Err(format!("{key} must be a positive integer"));
    }
    Ok(line as u32)
}

fn optional_count(object: &Map<String, Value>, key: &str, max: usize) -> Result<usize, String> {
    let Some(value) = object.get(key) else {
        return Ok(max);
    };
    let count = value.as_u64().ok_or_else(|| format!("{key} must be an integer"))?;
    if count == 0 || count > max as u64 {
        return Err(format!("{key} must be between 1 and {max}"));
    }
    Ok(count as usize)
}

fn contains_secret_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    lower.contains("rainy_api_key")
        || lower.contains("api_key=")
        || lower.contains("authorization: bearer")
        || lower.contains("-----begin ")
}

fn bounded(value: &str, max_bytes: usize) -> String {
    BoundedText::new(value, max_bytes).into_string()
}

fn digest(value: &str) -> String {
    hash(value.as_bytes()).to_hex().to_string()
}

fn added_lines(before: &str, after: &str) -> usize {
    after.lines().count().saturating_sub(before.lines().count())
}

fn removed_lines(before: &str, after: &str) -> usize {
    before.lines().count().saturating_sub(after.lines().count())
}

fn unified_diff(path: &str, before: &str, after: &str) -> String {
    let mut diff = format!("--- a/{path}\n+++ b/{path}\n@@\n");
    for line in before.lines() {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in after.lines() {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    bounded(&diff, 12 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn invoke(runtime: &mut BrowserRuntime, value: Value) -> Value {
        serde_json::from_str(&runtime.invoke(&value.to_string())).expect("valid runtime JSON")
    }

    #[test]
    fn workspace_inspection_exposes_issue_and_bounded_inventory() {
        let mut runtime = BrowserRuntime::new();
        let result = invoke(&mut runtime, json!({"op": "inspect_workspace"}));
        assert_eq!(result["ok"], true);
        assert_eq!(result["state"]["task"]["id"], TASK_ID);
        assert_eq!(result["state"]["files"].as_array().map(Vec::len), Some(4));
        let read = invoke(&mut runtime, json!({"op": "read_file", "path": PATCH_PATH}));
        assert_eq!(read["startLine"], 1);
        assert_eq!(read["endLine"], 11);
    }

    #[test]
    fn patch_is_staged_until_explicit_human_acceptance() {
        let mut runtime = BrowserRuntime::new();
        let result = invoke(
            &mut runtime,
            json!({
                "op": "propose_patch",
                "path": PATCH_PATH,
                "find": PATCH_FIND,
                "replace": PATCH_REPLACE,
                "rationale": PATCH_RATIONALE,
            }),
        );
        assert_eq!(result["ok"], true);
        assert!(result["state"]["stagedPatch"].is_object());
        assert_eq!(result["state"]["files"][1]["changed"], false);
    }

    #[test]
    fn staged_verification_passes_and_acceptance_updates_shared_state() {
        let mut runtime = BrowserRuntime::new();
        let _ = invoke(
            &mut runtime,
            json!({
                "op": "propose_patch",
                "path": PATCH_PATH,
                "find": PATCH_FIND,
                "replace": PATCH_REPLACE,
                "rationale": PATCH_RATIONALE,
            }),
        );
        let verified = invoke(&mut runtime, json!({"op": "verify_patch", "scope": "staged"}));
        assert_eq!(verified["verification"]["status"], "passed");
        let accepted = invoke(&mut runtime, json!({"op": "accept_patch"}));
        assert_eq!(accepted["decision"], "accepted");
        assert_eq!(accepted["state"]["task"]["status"], "verified");
        assert!(
            accepted["state"]["evidence"]
                .as_array()
                .is_some_and(|items| items.iter().any(|item| item["kind"] == "human_acceptance"))
        );
    }

    #[test]
    fn unknown_fields_and_path_escape_fail_closed() {
        let mut runtime = BrowserRuntime::new();
        let unknown = invoke(&mut runtime, json!({"op": "get_task_state", "extra": "nope"}));
        assert_eq!(unknown["ok"], false);
        let escaped = invoke(&mut runtime, json!({"op": "read_file", "path": "../secret"}));
        assert_eq!(escaped["ok"], false);
        let embedded_nul =
            invoke(&mut runtime, json!({"op": "read_file", "path": "src/\u{0}auth.rs"}));
        assert_eq!(embedded_nul["ok"], false);
    }

    #[test]
    fn rejected_patch_leaves_file_unchanged() {
        let mut runtime = BrowserRuntime::new();
        let _ = invoke(
            &mut runtime,
            json!({
                "op": "propose_patch",
                "path": PATCH_PATH,
                "find": PATCH_FIND,
                "replace": PATCH_REPLACE,
                "rationale": PATCH_RATIONALE,
            }),
        );
        let rejected = invoke(&mut runtime, json!({"op": "reject_patch"}));
        assert_eq!(rejected["applied"], false);
        assert_eq!(rejected["state"]["files"][1]["changed"], false);
        assert_eq!(rejected["state"]["task"]["status"], "unchanged");
    }

    #[test]
    fn invalid_patch_inputs_do_not_expose_sensitive_content() {
        let mut runtime = BrowserRuntime::new();
        let result = invoke(
            &mut runtime,
            json!({
                "op": "propose_patch",
                "path": PATCH_PATH,
                "find": "old",
                "replace": "RAINY_API_KEY=secret",
                "rationale": "test",
            }),
        );
        assert_eq!(result["ok"], false);
        assert_eq!(result["error"]["code"], "sensitive_input");
        assert!(
            runtime.invoke(&json!({"op": "get_task_state"}).to_string()).len() < MAX_RESULT_BYTES
        );
    }

    #[test]
    fn core_verification_status_remains_typed() {
        assert_eq!(aether_core::VerificationStatus::Passed.as_str(), "passed");
        assert_eq!(aether_core::WorkflowVerification::NotRequired.as_str(), "not_required");
    }
}
