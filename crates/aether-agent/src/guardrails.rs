use std::{
    collections::VecDeque,
    sync::atomic::{AtomicU16, Ordering},
};

use aether_core::{
    ActionClassification, CommandClass, CommandEffects, DEFAULT_MAX_OUTPUT_BYTES, PreparedAction,
    ToolCallId, ToolResult, WorkflowPhase, WorkflowState, analyze_command,
};
use serde_json::Value;

const MAX_OBSERVATIONS: usize = 64;
const MAX_FAILURE_OBSERVATIONS: usize = 16;
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

#[derive(Clone, Debug)]
struct FailureObservation {
    fingerprint: [u8; 16],
    revision: u32,
    attempts: u8,
    result: ToolResult,
}

/// Small workflow projection used for progress comparisons between tool steps.
///
/// Keeping this projection separate from `WorkflowState` avoids cloning transcripts, evidence,
/// verification plans, and other state that cannot affect the loop-progress decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkflowObservation {
    workspace_revision: u32,
    progress: [u16; 4],
    unresolved_failures: [u8; 16],
    phase: WorkflowPhase,
    candidate_files: [u8; 16],
}

impl From<&WorkflowState> for WorkflowObservation {
    fn from(workflow: &WorkflowState) -> Self {
        Self {
            workspace_revision: workflow.workspace_revision,
            progress: [
                workflow.progress.discoveries,
                workflow.progress.inspections,
                workflow.progress.mutations,
                workflow.progress.verifications,
            ],
            unresolved_failures: digest_failures(&workflow.unresolved_failures),
            phase: workflow.phase,
            candidate_files: digest_candidates(&workflow.decision.candidate_files),
        }
    }
}

trait WorkflowObservationView {
    fn workspace_revision(&self) -> u32;
    fn progress(&self) -> [u16; 4];
    fn unresolved_failures(&self) -> [u8; 16];
    fn phase(&self) -> WorkflowPhase;
}

impl WorkflowObservationView for WorkflowState {
    fn workspace_revision(&self) -> u32 {
        self.workspace_revision
    }

    fn progress(&self) -> [u16; 4] {
        [
            self.progress.discoveries,
            self.progress.inspections,
            self.progress.mutations,
            self.progress.verifications,
        ]
    }

    fn unresolved_failures(&self) -> [u8; 16] {
        digest_failures(&self.unresolved_failures)
    }

    fn phase(&self) -> WorkflowPhase {
        self.phase
    }
}

fn digest_failures(failures: &[aether_core::WorkflowFailure]) -> [u8; 16] {
    if failures.is_empty() {
        return [0; 16];
    }
    let mut hasher = blake3::Hasher::new();
    for failure in failures {
        hasher.update(failure.key.as_str().as_bytes());
        hasher.update(failure.tool.as_str().as_bytes());
        hasher.update(failure.code.as_str().as_bytes());
        hasher.update(failure.message.as_str().as_bytes());
    }
    let hash = hasher.finalize();
    let mut digest = [0; 16];
    digest.copy_from_slice(&hash.as_bytes()[..16]);
    digest
}

fn digest_candidates(candidates: &[aether_core::DecisionCandidate]) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    for candidate in candidates {
        hasher.update(candidate.path.as_str().as_bytes());
        hasher.update(&candidate.score.to_le_bytes());
        hasher.update(&candidate.evidence.to_le_bytes());
        hasher.update(&[
            candidate.inspected as u8,
            candidate.modified as u8,
            candidate.stale as u8,
        ]);
    }
    let hash = hasher.finalize();
    let mut digest = [0; 16];
    digest.copy_from_slice(&hash.as_bytes()[..16]);
    digest
}

impl WorkflowObservationView for WorkflowObservation {
    fn workspace_revision(&self) -> u32 {
        self.workspace_revision
    }

    fn progress(&self) -> [u16; 4] {
        self.progress
    }

    fn unresolved_failures(&self) -> [u8; 16] {
        self.unresolved_failures
    }

    fn phase(&self) -> WorkflowPhase {
        self.phase
    }
}

/// Bounded deterministic state for suppressing redundant agent-loop work.
#[derive(Debug, Default)]
pub struct LoopGuardrails {
    observations: VecDeque<Observation>,
    failures: VecDeque<FailureObservation>,
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
        if !self.failures.is_empty()
            && let Some(failure) = self.failures.iter().rev().find(|item| {
                item.revision == revision
                    && item.fingerprint == fingerprint
                    && item.attempts >= NO_PROGRESS_LIMIT
            })
        {
            let mut result = ToolResult::failure(
                call_id,
                "repeated_failure",
                "guardrail: repeated failure for the same action; change arguments or approach",
                false,
                DEFAULT_MAX_OUTPUT_BYTES,
            );
            if let Some(error) = failure.result.error.as_ref() {
                result.output = aether_core::BoundedText::new(
                    format!("{}; last failure={}", result.output.as_str(), error.code.as_str()),
                    DEFAULT_MAX_OUTPUT_BYTES,
                );
            }
            let _ = self.prevented.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_add(1))
            });
            return Some(result);
        }
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

    /// Fast preflight using the action fingerprint and classification prepared by the registry.
    pub(crate) fn preflight_prepared(
        &self,
        action: &PreparedAction,
        revision: u32,
    ) -> Option<ToolResult> {
        let kind = prepared_kind(action)?;
        let fingerprint = action.fingerprint;
        if let Some(failure) = self.failures.iter().rev().find(|item| {
            item.revision == revision
                && item.fingerprint == fingerprint
                && item.attempts >= NO_PROGRESS_LIMIT
        }) {
            let mut result = ToolResult::failure(
                action.call_id.clone(),
                "repeated_failure",
                "guardrail: repeated failure for the same action; change arguments or approach",
                false,
                DEFAULT_MAX_OUTPUT_BYTES,
            );
            if let Some(error) = failure.result.error.as_ref() {
                result.output = aether_core::BoundedText::new(
                    format!("{}; last failure={}", result.output.as_str(), error.code.as_str()),
                    DEFAULT_MAX_OUTPUT_BYTES,
                );
            }
            self.prevented.fetch_add(1, Ordering::Relaxed);
            return Some(result);
        }
        let cached = self
            .observations
            .iter()
            .rev()
            .find(|item| item.revision == revision && item.fingerprint == fingerprint)
            .map(|item| {
                let mut cached = item.result.clone();
                cached.call_id = action.call_id.clone();
                cached.output = aether_core::BoundedText::new(
                    format!(
                        "guardrail: redundant {} skipped; cached observation for workspace revision {}:\n{}",
                        action.tool,
                        revision,
                        item.result.output.as_str()
                    ),
                    DEFAULT_MAX_OUTPUT_BYTES,
                );
                cached
            });
        if cached.is_some() {
            self.prevented.fetch_add(1, Ordering::Relaxed);
        }
        let _ = kind;
        cached
    }

    /// Return whether a prepared read/verification is already cached at this revision.
    pub(crate) fn has_cached_prepared(&self, action: &PreparedAction, revision: u32) -> bool {
        prepared_kind(action).is_some_and(|_| {
            self.observations
                .iter()
                .any(|item| item.revision == revision && item.fingerprint == action.fingerprint)
        })
    }

    /// Return whether a call is already represented by the current revision without changing
    /// the prevented counter.  The agent uses this read-only probe before considering a local
    /// planner shortcut; the normal scheduler remains responsible for producing the cached
    /// result and accounting for the prevented call.
    pub fn has_cached_observation(&self, name: &str, input: &Value, revision: u32) -> bool {
        let Some(kind) = classify(name, input) else {
            return false;
        };
        let fingerprint = call_fingerprint(kind, name, input);
        self.observations
            .iter()
            .any(|item| item.revision == revision && item.fingerprint == fingerprint)
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
        self.observe_impl(name, input, result, before, after)
    }

    /// Fold one result using only workflow fields that affect loop progress.
    pub(crate) fn observe_snapshot(
        &mut self,
        name: &str,
        input: &Value,
        result: &ToolResult,
        before: &WorkflowObservation,
        after: &WorkflowObservation,
    ) -> Option<String> {
        self.observe_impl(name, input, result, before, after)
    }

    /// Fold a prepared action while reusing its binary fingerprint and classification.
    pub(crate) fn observe_snapshot_prepared(
        &mut self,
        action: &PreparedAction,
        result: &ToolResult,
        before: &WorkflowObservation,
        after: &WorkflowObservation,
    ) -> Option<String> {
        self.observe_impl_with_fingerprint(
            &action.tool,
            &action.normalized_input,
            result,
            before,
            after,
            prepared_kind(action),
            Some(action.fingerprint),
        )
    }

    fn observe_impl<W: WorkflowObservationView>(
        &mut self,
        name: &str,
        input: &Value,
        result: &ToolResult,
        before: &W,
        after: &W,
    ) -> Option<String> {
        let kind = classify(name, input);
        let fingerprint = kind.map(|kind| call_fingerprint(kind, name, input));
        self.observe_impl_with_fingerprint(name, input, result, before, after, kind, fingerprint)
    }

    #[allow(clippy::too_many_arguments)]
    fn observe_impl_with_fingerprint<W: WorkflowObservationView>(
        &mut self,
        name: &str,
        input: &Value,
        result: &ToolResult,
        before: &W,
        after: &W,
        kind: Option<CallKind>,
        action_fingerprint: Option<[u8; 16]>,
    ) -> Option<String> {
        let fingerprint = action_fingerprint
            .unwrap_or_else(|| call_fingerprint(CallKind::Observation, name, input));
        if !result.ok && kind.is_some() {
            if let Some(existing) = self.failures.iter_mut().find(|item| {
                item.revision == after.workspace_revision() && item.fingerprint == fingerprint
            }) {
                existing.attempts = existing.attempts.saturating_add(1);
                existing.result = result.clone();
            } else {
                if self.failures.len() == MAX_FAILURE_OBSERVATIONS {
                    self.failures.pop_front();
                }
                self.failures.push_back(FailureObservation {
                    fingerprint,
                    revision: after.workspace_revision(),
                    attempts: 1,
                    result: result.clone(),
                });
            }
        }
        if result.ok && kind.is_some() {
            let observation = Observation {
                fingerprint,
                revision: after.workspace_revision(),
                result: result.clone(),
            };
            if !self.observations.iter().any(|item| {
                item.revision == after.workspace_revision() && item.fingerprint == fingerprint
            }) {
                if self.observations.len() == MAX_OBSERVATIONS {
                    self.observations.pop_front();
                }
                self.observations.push_back(observation);
            }
        }

        if !result.ok {
            let repeated = kind.is_some_and(|_| {
                self.failures.iter().any(|item| {
                    item.revision == after.workspace_revision()
                        && item.fingerprint == fingerprint
                        && item.attempts >= NO_PROGRESS_LIMIT
                })
            });
            if repeated {
                return Some(
                    "guardrail: repeated failure circuit breaker; change arguments or approach"
                        .to_owned(),
                );
            }
            self.no_progress = self.no_progress.saturating_add(1);
            return None;
        }

        let progress = progress_fingerprint(after, result);
        let state_advanced = before.workspace_revision() != after.workspace_revision()
            || before.progress() != after.progress()
            || before.unresolved_failures() != after.unresolved_failures()
            || before.phase() != after.phase();
        if state_advanced || self.last_progress != Some(progress) {
            self.no_progress = 0;
        } else {
            self.no_progress = self.no_progress.saturating_add(1);
        }
        self.last_progress = Some(progress);

        if self.no_progress >= NO_PROGRESS_LIMIT {
            self.no_progress = 0;
            Some("guardrail: no progress across 3 successful tool steps; change arguments or approach, modify the workspace, or finish with the evidence already collected".to_owned())
        } else if after.phase() == WorkflowPhase::Complete && after.unresolved_failures() == [0; 16]
        {
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

fn prepared_kind(action: &PreparedAction) -> Option<CallKind> {
    match action.classification {
        ActionClassification::Read => Some(CallKind::Observation),
        ActionClassification::Verification => Some(CallKind::Verification),
        ActionClassification::Mutation | ActionClassification::Other => None,
    }
}

fn verification_command(input: &Value) -> bool {
    command_effects(input)
        .is_some_and(|effects| matches!(effects.class, CommandClass::Verification))
}

fn command_effects(input: &Value) -> Option<CommandEffects> {
    let program = input.get("program")?.as_str()?;
    let args = match input.get("args") {
        None => Vec::new(),
        Some(value) => value
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .map(str::to_owned)
            .collect(),
    };
    Some(analyze_command(program, &args, input.get("cwd").and_then(Value::as_str).unwrap_or("")))
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

fn progress_fingerprint<W: WorkflowObservationView>(workflow: &W, result: &ToolResult) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&workflow.workspace_revision().to_le_bytes());
    for counter in workflow.progress() {
        hasher.update(&counter.to_le_bytes());
    }
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
    fn repeated_failures_trip_a_bounded_circuit_breaker() {
        let mut guard = LoopGuardrails::new();
        let state = WorkflowState::new();
        let input = json!({"files":[{"path":"src/lib.rs"}]});
        for index in 0..3 {
            let failed = ToolResult::failure(
                ToolCallId::new(format!("failure-{index}")).unwrap(),
                "io",
                "same failure",
                true,
                1024,
            );
            guard.observe("read", &input, &failed, &state, &state);
        }
        let blocked = guard
            .preflight(ToolCallId::new("circuit").unwrap(), "read", &input, 0)
            .expect("same failed action must be circuit-broken");
        let error = blocked.error.unwrap();
        assert_eq!(error.code.as_str(), "repeated_failure");
        assert!(!error.retryable);
    }

    #[test]
    fn direct_verification_commands_are_cached_by_argv() {
        let mut guard = LoopGuardrails::new();
        let state = WorkflowState::new();
        let input = json!({"program":"cargo", "args":["test", "-p", "api"]});
        guard.observe("shell", &input, &success("one", "pass"), &state, &state);
        let cached = guard.preflight(ToolCallId::new("two").unwrap(), "shell", &input, 0);
        assert_eq!(cached.unwrap().call_id.as_str(), "two");
        assert_eq!(guard.prevented_count(), 1);
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
        complete.work.initialize_from_prompt("inspect the source");
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
