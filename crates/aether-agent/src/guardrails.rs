use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU16, Ordering},
};

use aether_core::{DEFAULT_MAX_OUTPUT_BYTES, ToolCallId, ToolResult, WorkflowPhase, WorkflowState};
use serde_json::Value;

const MAX_OBSERVATIONS: usize = 64;
const NO_PROGRESS_LIMIT: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallKind {
    Observation,
    Verification,
}

#[derive(Clone, Debug)]
struct Observation {
    fingerprint: [u8; 16],
    revision: u32,
    result: ToolResult,
}

/// Bounded deterministic state for suppressing redundant agent-loop work.
#[derive(Debug, Default)]
pub struct LoopGuardrails {
    observations: VecDeque<Observation>,
    last_progress: Option<[u8; 16]>,
    no_progress: u8,
    prevented: AtomicU16,
}

impl LoopGuardrails {
    /// Create empty per-turn guardrail state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a cached successful observation or compact feedback when a call is redundant.
    pub fn preflight(
        &self,
        call_id: ToolCallId,
        name: &str,
        input: &Value,
        revision: u32,
    ) -> Option<ToolResult> {
        let kind = classify(name, input)?;
        let fingerprint = call_fingerprint(kind, name, input);
        let cached = self.observations
            .iter()
            .rev()
            .find(|item| item.revision == revision && item.fingerprint == fingerprint)
            .map(|item| {
                let mut cached = item.result.clone();
                cached.call_id = call_id;
                let output = format!(
                    "guardrail: redundant {name} skipped; cached observation for workspace revision {revision}:\n{}",
                    item.result.output.as_str()
                );
                cached.output = aether_core::BoundedText::new(output, DEFAULT_MAX_OUTPUT_BYTES);
                cached
            });
        if cached.is_some() {
            let _ = self.prevented.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
        }
        cached
    }

    /// Fold one result into bounded cache/progress state and return actionable loop feedback.
    pub fn observe(
        &mut self,
        name: &str,
        input: &Value,
        result: &ToolResult,
        before: &WorkflowState,
        after: &WorkflowState,
    ) -> Option<String> {
        if result.ok
            && let Some(kind) = classify(name, input)
        {
            let fingerprint = call_fingerprint(kind, name, input);
            let observation = Observation {
                fingerprint,
                revision: after.workspace_revision,
                result: result.clone(),
            };
            if !self.observations.iter().any(|item| {
                item.revision == after.workspace_revision && item.fingerprint == fingerprint
            }) {
                if self.observations.len() == MAX_OBSERVATIONS {
                    self.observations.pop_front();
                }
                self.observations.push_back(observation);
            }
        }

        if !result.ok {
            self.no_progress = 0;
            self.last_progress = None;
            return None;
        }

        let progress = progress_fingerprint(after, result);
        let state_advanced = before.workspace_revision != after.workspace_revision
            || before.progress.discoveries != after.progress.discoveries
            || before.progress.inspections != after.progress.inspections
            || before.progress.mutations != after.progress.mutations
            || before.progress.verifications != after.progress.verifications
            || before.unresolved_failures != after.unresolved_failures
            || before.phase != after.phase;
        if state_advanced || self.last_progress != Some(progress) {
            self.no_progress = 0;
        } else {
            self.no_progress = self.no_progress.saturating_add(1);
        }
        self.last_progress = Some(progress);

        if self.no_progress >= NO_PROGRESS_LIMIT {
            self.no_progress = 0;
            Some("guardrail: no progress across 3 successful tool steps; change arguments or approach, modify the workspace, or finish with the evidence already collected".to_owned())
        } else if after.phase == WorkflowPhase::Complete && after.unresolved_failures.is_empty() {
            Some("completion_ready: required verification passed and no unresolved failures remain; summarize and finish".to_owned())
        } else {
            None
        }
    }

    /// Number of bounded successful observations retained (used by benchmarks/diagnostics).
    pub fn observation_count(&self) -> usize {
        self.observations.len()
    }

    /// Saturating count of redundant executions prevented during this turn.
    pub fn prevented_count(&self) -> u16 {
        self.prevented.load(Ordering::Relaxed)
    }
}

fn classify(name: &str, input: &Value) -> Option<CallKind> {
    match name {
        "read" | "search" | "find" | "list" => Some(CallKind::Observation),
        "shell" | "process" if verification_command(input) => Some(CallKind::Verification),
        _ => None,
    }
}

fn verification_command(input: &Value) -> bool {
    let command = input.get("command").or_else(|| input.get("cmd")).and_then(Value::as_str);
    command.is_some_and(|command| {
        let command = command.trim().to_ascii_lowercase();
        ["cargo test", "cargo nextest", "cargo check", "cargo clippy", "cargo fmt", "cargo build"]
            .iter()
            .any(|prefix| command.starts_with(prefix))
    })
}

fn call_fingerprint(kind: CallKind, name: &str, input: &Value) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[kind as u8]);
    hasher.update(name.as_bytes());
    hash_value(&mut hasher, input);
    truncated_hash(hasher.finalize())
}

fn hash_value(hasher: &mut blake3::Hasher, value: &Value) {
    match value {
        Value::Null => {
            hasher.update(b"n");
        }
        Value::Bool(value) => {
            hasher.update(if *value { b"t" } else { b"f" });
        }
        Value::Number(value) => {
            hasher.update(value.to_string().as_bytes());
        }
        Value::String(value) => {
            hasher.update(value.as_bytes());
        }
        Value::Array(values) => {
            for value in values {
                hash_value(hasher, value);
            }
        }
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            for key in keys {
                hasher.update(key.as_bytes());
                hash_value(hasher, &values[key]);
            }
        }
    };
}

fn progress_fingerprint(workflow: &WorkflowState, result: &ToolResult) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&workflow.workspace_revision.to_le_bytes());
    hasher.update(&workflow.progress.discoveries.to_le_bytes());
    hasher.update(&workflow.progress.inspections.to_le_bytes());
    hasher.update(&workflow.progress.mutations.to_le_bytes());
    hasher.update(&workflow.progress.verifications.to_le_bytes());
    hasher.update(result.output.as_str().as_bytes());
    truncated_hash(hasher.finalize())
}

fn truncated_hash(hash: blake3::Hash) -> [u8; 16] {
    let mut fingerprint = [0; 16];
    fingerprint.copy_from_slice(&hash.as_bytes()[..16]);
    fingerprint
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn success(id: &str, output: &str) -> ToolResult {
        ToolResult::success_text(ToolCallId::new(id).unwrap(), output, 1024)
    }

    #[test]
    fn duplicate_reads_searches_and_equivalent_arguments_use_cache() {
        let mut guard = LoopGuardrails::new();
        let state = WorkflowState::new();
        let input = json!({"query":"needle", "path":"src"});
        guard.observe("search", &input, &success("one", "hit"), &state, &state);
        let reordered = json!({"path":"src", "query":"needle"});
        let cached = guard.preflight(ToolCallId::new("two").unwrap(), "search", &reordered, 0);
        assert_eq!(cached.unwrap().call_id.as_str(), "two");
        assert_eq!(guard.prevented_count(), 1);
    }

    #[test]
    fn changed_arguments_workspace_changes_and_failures_allow_retries() {
        let mut guard = LoopGuardrails::new();
        let state = WorkflowState::new();
        let input = json!({"files":[{"path":"src/lib.rs"}]});
        guard.observe("read", &input, &success("one", "content"), &state, &state);
        assert!(
            guard
                .preflight(
                    ToolCallId::new("two").unwrap(),
                    "read",
                    &json!({"files":[{"path":"src/main.rs"}]}),
                    0
                )
                .is_none()
        );
        assert!(guard.preflight(ToolCallId::new("three").unwrap(), "read", &input, 1).is_none());
        let failed =
            ToolResult::failure(ToolCallId::new("fail").unwrap(), "io", "failed", true, 1024);
        let mut clean = LoopGuardrails::new();
        clean.observe("read", &input, &failed, &state, &state);
        assert!(clean.preflight(ToolCallId::new("retry").unwrap(), "read", &input, 0).is_none());
    }

    #[test]
    fn no_progress_and_completion_feedback_are_deterministic() {
        let mut guard = LoopGuardrails::new();
        let state = WorkflowState::new();
        let empty = success("empty", "[]");
        assert!(guard.observe("unknown", &json!({}), &empty, &state, &state).is_none());
        assert!(guard.observe("unknown", &json!({}), &empty, &state, &state).is_none());
        assert!(guard.observe("unknown", &json!({}), &empty, &state, &state).is_none());
        assert!(
            guard
                .observe("unknown", &json!({}), &empty, &state, &state)
                .unwrap()
                .contains("no progress")
        );

        let mut complete = state.clone();
        complete.record_relevant_file("src/lib.rs");
        complete.record_inspection();
        assert!(complete.complete_if_ready(&[]));
        assert!(
            guard
                .observe("unknown", &json!({}), &success("ok", "done"), &state, &complete)
                .unwrap()
                .starts_with("completion_ready")
        );
    }
}
