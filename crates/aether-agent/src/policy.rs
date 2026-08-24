//! Deterministic autonomous coding policy.
//!
//! This module is the composition root for bounded repository evidence, context reuse,
//! workflow transitions, focused verification, scheduler/guardrail preflight, and next-action
//! ranking. It deliberately contains no model-facing planning or external state.

use std::path::PathBuf;

use aether_core::{
    CommandEffects, ContextSnapshot, DecisionAction, DecisionEvidenceKind, ObservedFileState,
    ToolCallId, ToolResult, WorkflowPhase, WorkflowVerification, analyze_command,
};
use serde_json::Value;

use crate::context::ContextEngine;
use crate::guardrails::LoopGuardrails;
use crate::planner::{
    RepositoryActionPlan, RepositoryActionPlanner, RepositoryPlanRequest, RepositoryRequestKind,
};
use crate::{RepoMap, RepoSelection, RepoSelectionKind};

/// Maximum repository candidates considered by one policy refresh.
pub const MAX_POLICY_CANDIDATES: usize = 16;
/// Maximum paths extracted from one completed tool result.
pub const MAX_POLICY_OBSERVED_PATHS: usize = 16;
/// Confidence required before a mutation may be admitted by the policy.
pub const MIN_MUTATION_CONFIDENCE: u8 = 40;

/// Bounded action ranking exposed for tests, diagnostics, and model adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RankedAction {
    pub action: DecisionAction,
    pub score: u16,
    pub target: Option<String>,
    pub reason: String,
}

/// The deterministic policy façade used by the live agent loop.
#[derive(Debug, Default)]
pub struct AutonomousCodingPolicy {
    guardrails: LoopGuardrails,
    candidate_key: Option<[u8; 16]>,
    planner: RepositoryActionPlanner,
}

impl AutonomousCodingPolicy {
    /// Construct a fresh per-turn policy. Persisted decision state lives in the context snapshot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Return a pure read-only plan for one model search request, when it is eligible for local
    /// repository observation.  Ambiguous plans are returned to the caller so it can record the
    /// abort and fall back to the normal permissioned scheduler path.
    pub(crate) fn repository_plan_for_call(
        &self,
        snapshot: &ContextSnapshot,
        name: &str,
        input: &Value,
    ) -> Option<RepositoryActionPlan> {
        if name != "search" {
            return None;
        }
        if self.guardrails.has_cached_observation(name, input, snapshot.workflow.workspace_revision)
        {
            return None;
        }
        let patterns = input.get("patterns")?.as_array()?;
        if patterns.is_empty() || patterns.len() > 32 {
            return None;
        }
        let query = patterns.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?.join(" ");
        let request = RepositoryPlanRequest::new(RepositoryRequestKind::Search, &query, snapshot);
        Some(self.planner.plan(request))
    }

    pub(crate) fn planner(&self) -> &RepositoryActionPlanner {
        &self.planner
    }

    /// Run cached-observation and insufficient-evidence checks before tool execution.
    pub fn preflight(
        &self,
        snapshot: &ContextSnapshot,
        call_id: ToolCallId,
        name: &str,
        input: &Value,
    ) -> Option<ToolResult> {
        self.guardrails
            .preflight(call_id.clone(), name, input, snapshot.workflow.workspace_revision)
            .or_else(|| self.mutation_preflight(snapshot, call_id, name, input))
    }

    /// Fold one completed observation into the shared context and decision state.
    pub fn observe(
        &mut self,
        engine: &mut ContextEngine,
        name: &str,
        input: &Value,
        result: &ToolResult,
        executed: bool,
    ) -> Option<String> {
        let before = engine.snapshot().workflow.clone();
        engine.observe_tool(name, input, result);
        self.update_evidence(engine, name, input, result, executed);
        self.refresh_candidates(engine);
        let after = engine.snapshot().workflow.clone();
        let guard_feedback = self.guardrails.observe(name, input, result, &before, &after);
        let advanced = before.progress != after.progress
            || before.phase != after.phase
            || before.workspace_revision != after.workspace_revision
            || before.unresolved_failures != after.unresolved_failures
            || before.decision.candidate_files != after.decision.candidate_files;
        let decision = &mut engine.snapshot_mut().workflow.decision;
        decision.note_observation_value(advanced && executed);
        if decision.low_value_observations >= 3 {
            decision.set_next(
                DecisionAction::Escalate,
                None::<&str>,
                "repeated observations did not add evidence; change scope or arguments",
            );
        } else {
            self.update_next_action(engine);
        }
        let action_feedback = self.compact_feedback(engine.snapshot());
        Some(match guard_feedback {
            Some(feedback) => format!("{feedback}\n{action_feedback}"),
            None => action_feedback,
        })
    }

    /// Refresh target candidates from cached context and bounded RepoMap/symbol evidence.
    pub fn refresh_candidates(&mut self, engine: &mut ContextEngine) {
        let snapshot = engine.snapshot().clone();
        let key = candidate_key(&snapshot);
        if self.candidate_key == Some(key) {
            return;
        }
        self.candidate_key = Some(key);
        let task = snapshot.current_task.as_str().trim();
        if task.is_empty() {
            return;
        }

        let paths = relevant_paths(&snapshot);
        let Some(map) = engine.repository_map() else { return };
        let selections = if paths.is_empty() {
            map.select(task, MAX_POLICY_CANDIDATES).unwrap_or_default()
        } else {
            targeted_selections(map, task, &paths)
        };
        let workflow = &mut engine.snapshot_mut().workflow;
        for selection in selections {
            let path = selection.path.to_string_lossy().into_owned();
            let inspected = snapshot.inspected.iter().any(|file| {
                file.path == path && file.last_state == ObservedFileState::Present && !file.stale
            });
            let modified = snapshot.modified.iter().any(|modified| modified == &path);
            let evidence = usize::from(selection.symbol.is_some())
                + usize::from(matches!(selection.kind, RepoSelectionKind::Test));
            workflow.decision.upsert_candidate(
                &path,
                selection.score,
                evidence,
                inspected,
                modified,
                snapshot.inspected.iter().any(|file| file.path == path && file.stale),
            );
            if let Some(symbol) = selection.symbol {
                workflow.decision.record_evidence(
                    if matches!(selection.kind, RepoSelectionKind::Test) {
                        DecisionEvidenceKind::Relationship
                    } else {
                        DecisionEvidenceKind::Symbol
                    },
                    &path,
                    format!("{} at line {}", symbol.name, symbol.line),
                    selection.score.min(96) as u16,
                    workflow.workspace_revision,
                );
            }
        }
        if workflow.decision.candidate_files.is_empty() {
            workflow
                .decision
                .record_question("target", "identify a relevant file before attempting a mutation");
        } else {
            workflow.decision.clear_question("target");
        }
        self.update_next_action(engine);
    }

    /// Apply final workflow completion and keep the persisted recommendation synchronized.
    pub fn finish_turn(&mut self, engine: &mut ContextEngine) -> bool {
        let complete = engine.finish_turn();
        self.update_next_action(engine);
        complete
    }

    /// Return deterministic highest-value actions, with stable score and path tie-breaking.
    #[must_use]
    pub fn rank_next_actions(&self, snapshot: &ContextSnapshot) -> Vec<RankedAction> {
        let workflow = &snapshot.workflow;
        let decision = &workflow.decision;
        let mut ranked = Vec::with_capacity(4);
        if workflow.phase == WorkflowPhase::Complete && workflow.unresolved_failures.is_empty() {
            ranked.push(action(
                DecisionAction::Finish,
                100,
                None,
                "all required evidence and checks passed",
            ));
            return ranked;
        }
        if decision.low_value_observations >= 3 {
            ranked.push(action(
                DecisionAction::Escalate,
                98,
                None,
                "repeated low-value observations need a changed approach",
            ));
        }
        if matches!(
            workflow.verification,
            WorkflowVerification::Pending | WorkflowVerification::Failed
        ) {
            if let Some((index, step)) = workflow
                .verification_steps
                .iter()
                .filter(|step| step.revision == workflow.workspace_revision && step.required)
                .enumerate()
                .find(|(_, step)| {
                    matches!(
                        step.status,
                        aether_core::VerificationStatus::Pending
                            | aether_core::VerificationStatus::Failed
                    )
                })
            {
                let action_kind = if index == 0 {
                    DecisionAction::VerifyFocused
                } else {
                    DecisionAction::VerifyBroader
                };
                ranked.push(action(
                    action_kind,
                    if index == 0 { 96 } else { 82 },
                    Some(step.command.as_str()),
                    if index == 0 {
                        "run the required focused check for the smallest affected scope"
                    } else {
                        "focused verification passed; escalate to the next bounded check"
                    },
                ));
            } else {
                ranked.push(action(
                    DecisionAction::VerifyBroader,
                    76,
                    None,
                    "no focused plan established correctness; use the safe fallback check",
                ));
            }
        }
        if let Some(candidate) = decision
            .candidate_files
            .iter()
            .find(|candidate| !candidate.inspected || candidate.stale)
        {
            ranked.push(action(
                DecisionAction::InspectCandidate,
                90,
                Some(candidate.path.as_str()),
                "inspect the highest-ranked current candidate before editing",
            ));
        }
        if !decision.unresolved_questions.is_empty() {
            let question = &decision.unresolved_questions[0];
            ranked.push(action(
                DecisionAction::ResolveQuestion,
                88,
                Some(question.key.as_str()),
                question.question.as_str(),
            ));
        }
        if decision.progress_confidence < MIN_MUTATION_CONFIDENCE {
            ranked.push(action(
                DecisionAction::DiscoverTarget,
                84,
                decision.candidate_files.first().map(|candidate| candidate.path.as_str()),
                "collect targeted repository evidence before mutation",
            ));
        } else if workflow.phase == WorkflowPhase::Inspect && snapshot.modified.is_empty() {
            ranked.push(action(
                DecisionAction::MutateMinimal,
                74,
                decision.candidate_files.first().map(|candidate| candidate.path.as_str()),
                "apply the smallest edit supported by inspected evidence",
            ));
        }
        if ranked.is_empty() {
            ranked.push(action(
                DecisionAction::DiscoverTarget,
                60,
                None,
                "targeted discovery is the next bounded step",
            ));
        }
        ranked.sort_unstable_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.action.cmp(&right.action))
                .then_with(|| left.target.cmp(&right.target))
        });
        ranked.truncate(8);
        ranked
    }

    /// Number of cached observations retained by the active loop guardrail.
    #[must_use]
    pub fn cached_observation_count(&self) -> usize {
        self.guardrails.observation_count()
    }

    /// Number of redundant tool executions prevented by the active policy.
    #[must_use]
    pub fn prevented_count(&self) -> u16 {
        self.guardrails.prevented_count()
    }

    fn mutation_preflight(
        &self,
        snapshot: &ContextSnapshot,
        call_id: ToolCallId,
        name: &str,
        input: &Value,
    ) -> Option<ToolResult> {
        if matches!(name, "write" | "patch") {
            if mutation_has_evidence(snapshot, name, input) {
                return None;
            }
        } else if matches!(name, "shell" | "process") {
            let effects = command_effects(input)?;
            if !effects.uncertain
                && effects.class.is_mutation()
                && command_mutation_has_evidence(snapshot, &effects)
            {
                return None;
            }
        } else {
            return None;
        }
        Some(ToolResult::failure(
            call_id,
            "insufficient_evidence",
            "inspect the exact target and establish a relevant symbol or relationship before modifying it",
            true,
            aether_core::DEFAULT_MAX_OUTPUT_BYTES,
        ))
    }

    fn update_evidence(
        &self,
        engine: &mut ContextEngine,
        name: &str,
        input: &Value,
        result: &ToolResult,
        executed: bool,
    ) {
        if !result.ok {
            return;
        }
        let revision = engine.snapshot().workflow.workspace_revision;
        let paths = result_paths(name, input, result);
        let kind = if is_verification(name, input) {
            DecisionEvidenceKind::Verification
        } else {
            match name {
                "read" => DecisionEvidenceKind::Inspection,
                "write" | "patch" => DecisionEvidenceKind::Mutation,
                _ => DecisionEvidenceKind::Discovery,
            }
        };
        let detail = if executed {
            format!("{name} completed")
        } else {
            format!("{name} reused cached observation")
        };
        let modified_paths = engine.snapshot().modified.clone();
        let decision = &mut engine.snapshot_mut().workflow.decision;
        if is_verification(name, input)
            && let Some(scope) =
                command_effects(input).and_then(|effects| effects.verification_scope)
        {
            decision.verification_scope =
                aether_core::BoundedText::new(scope, aether_core::MAX_DECISION_SCOPE);
        }
        if paths.is_empty() {
            decision.record_evidence(kind, "", detail, 20, revision);
        } else {
            for path in paths {
                decision.record_evidence(
                    kind,
                    &path,
                    &detail,
                    if executed { 42 } else { 24 },
                    revision,
                );
                if matches!(kind, DecisionEvidenceKind::Inspection) {
                    decision.clear_question(&format!("inspect:{path}"));
                }
            }
        }
        if matches!(kind, DecisionEvidenceKind::Verification) {
            decision.clear_question("verification");
        }
        if name == "write" || name == "patch" {
            for path in &modified_paths {
                decision.record_modified_scope(path);
            }
        }
    }

    fn update_next_action(&self, engine: &mut ContextEngine) {
        let snapshot = engine.snapshot().clone();
        let ranked = self.rank_next_actions(&snapshot);
        let Some(best) = ranked.first() else { return };
        let decision = &mut engine.snapshot_mut().workflow.decision;
        decision.set_next(best.action, best.target.as_deref(), &best.reason);
        if matches!(best.action, DecisionAction::VerifyFocused | DecisionAction::VerifyBroader)
            && let Some(target) = &best.target
        {
            decision.verification_scope =
                aether_core::BoundedText::new(target, aether_core::MAX_DECISION_FIELD_BYTES);
        }
    }

    fn compact_feedback(&self, snapshot: &ContextSnapshot) -> String {
        let decision = &snapshot.workflow.decision;
        let target = decision.next_target.as_ref().map_or("-", |target| target.as_str());
        format!(
            "decision: action={} target={} confidence={} candidates={} questions={} scope={}",
            decision.next_action.as_str(),
            target,
            decision.progress_confidence,
            decision.candidate_files.len(),
            decision.unresolved_questions.len(),
            if decision.verification_scope.as_str().is_empty() {
                "-"
            } else {
                decision.verification_scope.as_str()
            },
        )
    }
}

fn targeted_selections(map: &RepoMap, task: &str, paths: &[PathBuf]) -> Vec<RepoSelection> {
    let mut selections = paths
        .iter()
        .map(|path| RepoSelection {
            path: path.clone(),
            kind: RepoSelectionKind::File,
            score: 32,
            symbol: None,
        })
        .collect::<Vec<_>>();
    if let Ok(symbols) = map.lookup_symbols(task, paths, MAX_POLICY_CANDIDATES) {
        for symbol in symbols {
            selections.push(RepoSelection {
                path: symbol.path,
                kind: RepoSelectionKind::File,
                score: symbol.score,
                symbol: Some(crate::RepoSymbolSelection {
                    name: symbol.symbol.name,
                    kind: symbol.symbol.kind,
                    line: symbol.symbol.start_line,
                    container: symbol.symbol.container,
                }),
            });
        }
    }
    if let Ok(relationships) = map.lookup_relationships(task, paths, MAX_POLICY_CANDIDATES) {
        for relationship in relationships {
            let path = relationship.path;
            if let Some(existing) =
                selections.iter_mut().find(|selection: &&mut RepoSelection| selection.path == path)
            {
                existing.score = existing.score.max(relationship.score);
                if existing.symbol.is_none() {
                    existing.symbol =
                        relationship.symbol.map(|symbol| crate::RepoSymbolSelection {
                            name: symbol.name,
                            kind: symbol.kind,
                            line: symbol.start_line,
                            container: symbol.container,
                        });
                }
            } else {
                selections.push(RepoSelection {
                    path,
                    kind: if relationship.ambiguous {
                        RepoSelectionKind::File
                    } else {
                        RepoSelectionKind::Test
                    },
                    score: relationship.score,
                    symbol: relationship.symbol.map(|symbol| crate::RepoSymbolSelection {
                        name: symbol.name,
                        kind: symbol.kind,
                        line: symbol.start_line,
                        container: symbol.container,
                    }),
                });
            }
        }
    }
    selections.sort_unstable_by(|left, right| {
        right.score.cmp(&left.score).then_with(|| left.path.cmp(&right.path))
    });
    selections.truncate(MAX_POLICY_CANDIDATES);
    selections
}

fn relevant_paths(snapshot: &ContextSnapshot) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for path in snapshot
        .workflow
        .relevant_files
        .iter()
        .chain(snapshot.modified.iter())
        .chain(snapshot.inspected.iter().map(|file| &file.path))
    {
        let path = PathBuf::from(path);
        if !paths.contains(&path) {
            paths.push(path);
        }
        if paths.len() == MAX_POLICY_CANDIDATES {
            break;
        }
    }
    paths
}

fn candidate_key(snapshot: &ContextSnapshot) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(snapshot.current_task.as_str().as_bytes());
    hasher.update(&snapshot.workflow.workspace_revision.to_le_bytes());
    for path in relevant_paths(snapshot) {
        hasher.update(path.to_string_lossy().as_bytes());
    }
    let hash = hasher.finalize();
    let mut key = [0; 16];
    key.copy_from_slice(&hash.as_bytes()[..16]);
    key
}

fn result_paths(name: &str, input: &Value, result: &ToolResult) -> Vec<String> {
    let mut paths = Vec::new();
    let mut add = |path: &str| {
        if path.is_empty()
            || paths.iter().any(|existing| existing == path)
            || paths.len() == MAX_POLICY_OBSERVED_PATHS
        {
            return;
        }
        paths.push(path.to_owned());
    };
    if name == "write" {
        input.get("path").and_then(Value::as_str).into_iter().for_each(&mut add);
    }
    if let Some(data) = result.data.as_ref() {
        for key in ["path", "files", "matches", "paths"] {
            match data.get(key) {
                Some(Value::String(path)) => add(path),
                Some(Value::Array(values)) => {
                    for value in values {
                        value
                            .get("path")
                            .and_then(Value::as_str)
                            .or_else(|| value.as_str())
                            .into_iter()
                            .for_each(&mut add);
                    }
                }
                _ => {}
            }
        }
    }
    paths
}

fn is_verification(name: &str, input: &Value) -> bool {
    if name == "git" {
        return matches!(input.get("operation").and_then(Value::as_str), Some("status" | "diff"));
    }
    if !matches!(name, "shell" | "process") {
        return false;
    }
    matches!(name, "shell" | "process")
        && command_effects(input).is_some_and(|effects| effects.class.is_verification())
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

fn command_mutation_has_evidence(snapshot: &ContextSnapshot, effects: &CommandEffects) -> bool {
    let include_manifests = matches!(
        effects.class,
        aether_core::CommandClass::DependencyManagement | aether_core::CommandClass::GitMutation
    );
    let paths = effects
        .paths
        .iter()
        .chain(include_manifests.then_some(effects.manifests.iter()).into_iter().flatten());
    let paths = paths.collect::<Vec<_>>();
    !paths.is_empty()
        && paths.iter().all(|path| {
            snapshot.workflow.relevant_files.iter().any(|relevant| relevant == *path)
                && snapshot.inspected.iter().any(|file| {
                    file.path == **path
                        && file.last_state == ObservedFileState::Present
                        && !file.stale
                        && file.content_hash.is_some()
                })
        })
}

fn mutation_has_evidence(snapshot: &ContextSnapshot, name: &str, input: &Value) -> bool {
    let paths = if name == "write" {
        input.get("path").and_then(Value::as_str).into_iter().map(str::to_owned).collect::<Vec<_>>()
    } else {
        input
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .filter_map(|file| file.get("path").and_then(Value::as_str).map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    !paths.is_empty()
        && paths.iter().all(|path| {
            let create_only = name == "write"
                && input.get("create_only").and_then(Value::as_bool) == Some(true)
                && new_target_has_discovered_parent(snapshot, path);
            create_only
                || (snapshot.workflow.relevant_files.iter().any(|relevant| relevant == path)
                    && snapshot.inspected.iter().any(|file| {
                        file.path == *path
                            && file.last_state == ObservedFileState::Present
                            && !file.stale
                            && file.content_hash.is_some()
                    }))
        })
}

fn new_target_has_discovered_parent(snapshot: &ContextSnapshot, path: &str) -> bool {
    let parent = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("."));
    snapshot.workflow.relevant_files.iter().any(|relevant| {
        std::path::Path::new(relevant).parent().is_some_and(|candidate| candidate == parent)
    })
}

fn action(action: DecisionAction, score: u16, target: Option<&str>, reason: &str) -> RankedAction {
    RankedAction { action, score, target: target.map(str::to_owned), reason: reason.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::{BoundedText, ContextSnapshot, InspectedFile, LineRange};
    use serde_json::json;

    fn inspected_snapshot() -> ContextSnapshot {
        let mut snapshot = ContextSnapshot::new("/workspace", None);
        snapshot.current_task = BoundedText::new("fix parser", 512);
        snapshot.workflow.record_relevant_file("src/lib.rs");
        snapshot.inspected.push(InspectedFile {
            path: "src/lib.rs".to_owned(),
            content_hash: Some("hash".to_owned()),
            ranges: vec![LineRange { start: 1, end: 5 }],
            last_state: ObservedFileState::Present,
            stale: false,
        });
        snapshot.workflow.decision.progress_confidence = MIN_MUTATION_CONFIDENCE;
        snapshot
    }

    #[test]
    fn ranking_prefers_focused_verification_then_broader_scope() {
        let mut snapshot = inspected_snapshot();
        snapshot.workflow.record_mutation();
        snapshot.workflow.set_verification_plan([
            aether_core::VerificationStep::new("cargo test -p api", "api", 1),
            aether_core::VerificationStep::new("cargo check -p api", "api", 1),
        ]);
        let policy = AutonomousCodingPolicy::new();
        assert_eq!(policy.rank_next_actions(&snapshot)[0].action, DecisionAction::VerifyFocused);
        snapshot.workflow.record_verification_command("cargo test -p api", true);
        assert_eq!(policy.rank_next_actions(&snapshot)[0].action, DecisionAction::VerifyBroader);
    }

    #[test]
    fn mutation_preflight_rejects_uninspected_targets() {
        let policy = AutonomousCodingPolicy::new();
        let snapshot = ContextSnapshot::new("/workspace", None);
        let blocked = policy
            .preflight(
                &snapshot,
                ToolCallId::new("write").unwrap(),
                "write",
                &json!({"path":"src/lib.rs"}),
            )
            .unwrap();
        assert_eq!(blocked.error.unwrap().code.as_str(), "insufficient_evidence");
    }

    #[test]
    fn direct_command_mutation_preflight_uses_extracted_paths() {
        let policy = AutonomousCodingPolicy::new();
        let snapshot = inspected_snapshot();
        let allowed = policy.preflight(
            &snapshot,
            ToolCallId::new("shell-allowed").unwrap(),
            "shell",
            &json!({
                "program": "cargo",
                "args": ["fmt", "--", "src/lib.rs"]
            }),
        );
        assert!(allowed.is_none());
        let blocked = policy
            .preflight(
                &snapshot,
                ToolCallId::new("shell-blocked").unwrap(),
                "shell",
                &json!({"program": "cargo", "args": ["fmt"]}),
            )
            .unwrap();
        assert_eq!(blocked.error.unwrap().code.as_str(), "insufficient_evidence");
    }

    #[test]
    fn decision_bounds_are_stable_and_candidates_are_ranked() {
        let mut state = aether_core::DecisionState::new();
        for index in 0..(aether_core::MAX_DECISION_CANDIDATES + 3) {
            state.upsert_candidate(format!("src/{index}.rs"), index, 1, false, false, false);
        }
        assert_eq!(state.candidate_files.len(), aether_core::MAX_DECISION_CANDIDATES);
        assert!(state.candidate_files.windows(2).all(|items| items[0].score >= items[1].score));
    }
}
