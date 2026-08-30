//! Deterministic autonomous coding policy.
//!
//! This module is the composition root for bounded repository evidence, context reuse,
//! workflow transitions, focused verification, scheduler/guardrail preflight, and next-action
//! ranking. It deliberately contains no model-facing planning or external state.

use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};

use aether_core::{
    ActionClassification, CommandEffects, ContextSnapshot, DecisionAction, DecisionEvidenceKind,
    EvidenceProvenance, LineRange, ObservedFileState, PreparedAction, ToolCallId, ToolResult,
    WorkflowPhase, WorkflowVerification, WorkspacePath, analyze_command,
};
use serde_json::Value;

use crate::context::ContextEngine;
use crate::guardrails::{LoopGuardrails, WorkflowObservation};
use crate::planner::{
    RepositoryActionPlan, RepositoryActionPlanner, RepositoryPlanRequest, RepositoryReadTarget,
    RepositoryRequestKind,
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

struct ToolEvidenceUpdate {
    executed: bool,
    revision: u32,
    paths: Vec<String>,
    kind: DecisionEvidenceKind,
    detail: String,
    verification_scope: Option<String>,
    mutation: bool,
}

/// The deterministic policy façade used by the live agent loop.
#[derive(Debug, Default)]
pub struct AutonomousCodingPolicy {
    guardrails: LoopGuardrails,
    candidate_key: Option<[u8; 16]>,
    planner: RepositoryActionPlanner,
    optimizations_enabled: bool,
}

impl AutonomousCodingPolicy {
    /// Construct a fresh per-turn policy. Persisted decision state lives in the context snapshot.
    pub fn new() -> Self {
        Self { optimizations_enabled: true, ..Self::default() }
    }

    pub(crate) fn set_optimizations_enabled(&mut self, enabled: bool) {
        self.optimizations_enabled = enabled;
    }

    /// Build a plan from already-normalized actions without reparsing permission or footprint data.
    pub(crate) fn repository_plan_for_actions(
        &self,
        snapshot: &ContextSnapshot,
        actions: &[PreparedAction],
    ) -> Option<RepositoryActionPlan> {
        if !self.optimizations_enabled {
            return None;
        }
        if actions.is_empty() || actions.len() > 8 {
            return None;
        }
        let mut plans = Vec::with_capacity(actions.len());
        let mut read_targets = Vec::new();
        let mut saw_read = false;
        for action in actions {
            if self.guardrails.has_cached_prepared(action, snapshot.workflow.workspace_revision) {
                return None;
            }
            match action.tool.as_str() {
                "search" => {
                    plans.push(self.repository_search_plan(snapshot, &action.normalized_input)?)
                }
                "read" => {
                    saw_read = true;
                    if action.normalized_input.get("max_bytes").is_some() {
                        return None;
                    }
                    let files = action.normalized_input.get("files")?.as_array()?;
                    if files.is_empty() || files.len() > 64 {
                        return None;
                    }
                    for file in files.iter().take(64) {
                        let path = file.get("path")?.as_str()?;
                        let ranges = match (file.get("start_line"), file.get("end_line")) {
                            (Some(start), Some(end)) => vec![LineRange {
                                start: usize::try_from(start.as_u64()?).ok()?,
                                end: usize::try_from(end.as_u64()?).ok()?,
                            }],
                            _ => Vec::new(),
                        };
                        read_targets.push(RepositoryReadTarget { path: path.to_owned(), ranges });
                    }
                }
                _ => return None,
            }
        }
        if saw_read {
            plans.push(self.planner.plan_read_targets(snapshot, &read_targets));
        }
        self.planner.coalesce(&plans)
    }

    #[cfg(test)]
    fn repository_plan_for_calls(
        &self,
        snapshot: &ContextSnapshot,
        calls: &[(ToolCallId, String, Value)],
    ) -> Option<RepositoryActionPlan> {
        let actions = calls
            .iter()
            .map(|(call_id, name, input)| {
                PreparedAction::fallback(
                    aether_core::ToolInvocation {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        input: input.clone(),
                    },
                    aether_core::PermissionClass::ReadOnly,
                )
            })
            .collect::<Vec<_>>();
        self.repository_plan_for_actions(snapshot, &actions)
    }

    fn repository_search_plan(
        &self,
        snapshot: &ContextSnapshot,
        input: &Value,
    ) -> Option<RepositoryActionPlan> {
        if input.as_object()?.keys().any(|key| key != "patterns") {
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
        // A read cache is only safe after its source hash has been checked against the live
        // canonical file. The generic guardrail cache has no workspace handle and therefore must
        // not be allowed to satisfy reads first after an external edit.
        if name == "read" {
            return (self.optimizations_enabled)
                .then(|| cached_read_result(snapshot, call_id.clone(), input))
                .flatten()
                .or_else(|| {
                    self.guardrails.preflight_failure(
                        call_id.clone(),
                        name,
                        input,
                        snapshot.workflow.workspace_revision,
                    )
                })
                .or_else(|| self.mutation_preflight(snapshot, call_id, name, input));
        }
        // Discovery and verification results may depend on files that are not represented by the
        // current inspected set. The generic successful-observation cache has no workspace handle
        // with which to prove those dependencies are still current, so only retain its bounded
        // repeated-failure circuit here and execute successful non-read calls again.
        self.guardrails
            .preflight_failure(call_id.clone(), name, input, snapshot.workflow.workspace_revision)
            .or_else(|| self.mutation_preflight(snapshot, call_id, name, input))
    }

    /// Run policy checks against the normalized action shared by scheduler and executor.
    pub(crate) fn preflight_prepared(
        &self,
        snapshot: &ContextSnapshot,
        action: &PreparedAction,
    ) -> Option<ToolResult> {
        if action.tool == "read" && action.classification == ActionClassification::Read {
            return (self.optimizations_enabled)
                .then(|| {
                    cached_read_result(snapshot, action.call_id.clone(), &action.normalized_input)
                })
                .flatten()
                .or_else(|| {
                    self.optimizations_enabled
                        .then(|| {
                            self.guardrails.preflight_failure_prepared(
                                action,
                                snapshot.workflow.workspace_revision,
                            )
                        })
                        .flatten()
                })
                .or_else(|| self.mutation_preflight_prepared(snapshot, action));
        }
        // See the direct-call path above: successful non-read observations/verifications are not
        // reusable without a sound dependency-freshness check. Keep the prepared fast path only
        // when no successful observation exists, so its repeated-failure handling remains active
        // without allowing the cached success through.
        self.optimizations_enabled
            .then(|| {
                let revision = snapshot.workflow.workspace_revision;
                self.guardrails.preflight_failure_prepared(action, revision).or_else(|| {
                    (!self.guardrails.has_cached_prepared(action, revision))
                        .then(|| self.guardrails.preflight_prepared(action, revision))
                        .flatten()
                })
            })
            .flatten()
            .or_else(|| self.mutation_preflight_prepared(snapshot, action))
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
        let before = WorkflowObservation::from(&engine.snapshot().workflow);
        engine.observe_tool(name, input, result);
        self.update_evidence(engine, name, input, result, executed);
        self.refresh_candidates(engine);
        let after = WorkflowObservation::from(&engine.snapshot().workflow);
        let guard_feedback = self.guardrails.observe_snapshot(name, input, result, &before, &after);
        let advanced = before != after;
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

    /// Fold one prepared action without reconstructing its invocation or command classification.
    pub(crate) fn observe_prepared(
        &mut self,
        engine: &mut ContextEngine,
        action: &PreparedAction,
        result: &ToolResult,
        executed: bool,
    ) -> Option<String> {
        let before = WorkflowObservation::from(&engine.snapshot().workflow);
        engine.observe_prepared(action, result);
        self.update_evidence_prepared(engine, action, result, executed);
        self.refresh_candidates(engine);
        let after = WorkflowObservation::from(&engine.snapshot().workflow);
        let guard_feedback =
            self.guardrails.observe_snapshot_prepared(action, result, &before, &after);
        let advanced = before != after;
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
        let key = candidate_key(engine.snapshot());
        if self.candidate_key == Some(key) {
            return;
        }
        self.candidate_key = Some(key);
        let task = engine.snapshot().current_task.as_str().trim().to_owned();
        if task.is_empty() {
            return;
        }

        let paths = relevant_paths(engine.snapshot());
        let Some(map) = engine.repository_map() else { return };
        let selections = if paths.is_empty() {
            map.select(&task, MAX_POLICY_CANDIDATES).unwrap_or_default()
        } else {
            targeted_selections(map, &task, &paths)
        };
        for selection in selections {
            let path = selection.path.to_string_lossy().into_owned();
            let (inspected, modified, stale, revision) = {
                let snapshot = engine.snapshot();
                (
                    snapshot.inspected.iter().any(|file| {
                        file.path == path
                            && file.last_state == ObservedFileState::Present
                            && !file.stale
                    }),
                    snapshot.modified.iter().any(|modified| modified == &path),
                    snapshot.inspected.iter().any(|file| file.path == path && file.stale),
                    snapshot.workflow.workspace_revision,
                )
            };
            let evidence = usize::from(selection.symbol.is_some())
                + usize::from(matches!(selection.kind, RepoSelectionKind::Test));
            let selection_kind = selection.kind;
            let selection_score = selection.score;
            let symbol = selection.symbol;
            let workflow = &mut engine.snapshot_mut().workflow;
            workflow.decision.upsert_candidate(
                &path,
                selection_score,
                evidence,
                inspected,
                modified,
                stale,
            );
            if let Some(symbol) = symbol {
                workflow.decision.record_evidence(
                    if matches!(selection_kind, RepoSelectionKind::Test) {
                        DecisionEvidenceKind::Relationship
                    } else {
                        DecisionEvidenceKind::Symbol
                    },
                    &path,
                    format!("{} at line {}", symbol.name, symbol.line),
                    selection_score.min(96) as u16,
                    revision,
                );
            }
        }
        let workflow = &mut engine.snapshot_mut().workflow;
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

    /// Explain the deterministic evidence that still blocks a model's silent finish.
    pub(crate) fn completion_feedback(&self, snapshot: &ContextSnapshot, attempts: u8) -> String {
        let action = self.rank_next_actions(snapshot).into_iter().next();
        let next = action
            .as_ref()
            .map_or("continue with bounded evidence", |action| action.reason.as_str());
        format!(
            "completion_blocked: attempt={attempts}; phase={}; failures={}; verification={}; next={next}",
            snapshot.workflow.phase.as_str(),
            snapshot.workflow.unresolved_failures.len(),
            snapshot.workflow.verification.as_str(),
        )
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
        if workflow.director.phase == aether_core::TurnPhase::Recover {
            let target = workflow.recovery.affected_paths.first().map(|path| path.as_str());
            ranked.push(RankedAction {
                action: DecisionAction::Escalate,
                score: 102,
                target: target.map(str::to_owned),
                reason: format!(
                    "recover from {} with {} before attempting another mutation",
                    workflow.recovery.category.as_str(),
                    workflow.recovery.action.as_str()
                ),
            });
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
        Some(insufficient_evidence(call_id))
    }

    fn mutation_preflight_prepared(
        &self,
        snapshot: &ContextSnapshot,
        action: &PreparedAction,
    ) -> Option<ToolResult> {
        if !action.requirements.current_workspace_evidence {
            return None;
        }
        let has_evidence = match action.tool.as_str() {
            "write" => prepared_write_has_evidence(snapshot, action),
            "patch" => prepared_patch_has_evidence(snapshot, action),
            "shell" | "process" => action.command_effects.as_ref().is_some_and(|effects| {
                !effects.uncertain
                    && effects.class.is_mutation()
                    && command_mutation_has_evidence(snapshot, effects)
            }),
            _ => false,
        };
        if has_evidence { None } else { Some(insufficient_evidence(action.call_id.clone())) }
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
        let verification_scope = if is_verification(name, input) {
            command_effects(input).and_then(|effects| effects.verification_scope)
        } else {
            None
        };
        self.record_tool_evidence(
            engine,
            ToolEvidenceUpdate {
                executed,
                revision,
                paths,
                kind,
                detail,
                verification_scope,
                mutation: name == "write" || name == "patch",
            },
        );
    }

    fn record_tool_evidence(&self, engine: &mut ContextEngine, update: ToolEvidenceUpdate) {
        let modified_paths = engine.snapshot().modified.clone();
        let decision = &mut engine.snapshot_mut().workflow.decision;
        if let Some(scope) = update.verification_scope {
            decision.verification_scope =
                aether_core::BoundedText::new(scope, aether_core::MAX_DECISION_SCOPE);
        }
        if update.paths.is_empty() {
            decision.record_evidence_with_provenance(
                EvidenceProvenance::ToolOutput,
                update.kind,
                "",
                update.detail,
                20,
                update.revision,
            );
        } else {
            for path in update.paths {
                decision.record_evidence_with_provenance(
                    EvidenceProvenance::ToolOutput,
                    update.kind,
                    &path,
                    &update.detail,
                    if update.executed { 42 } else { 24 },
                    update.revision,
                );
                if matches!(update.kind, DecisionEvidenceKind::Inspection) {
                    decision.clear_question(&format!("inspect:{path}"));
                }
            }
        }
        if matches!(update.kind, DecisionEvidenceKind::Verification) {
            decision.clear_question("verification");
        }
        if update.mutation {
            for path in &modified_paths {
                decision.record_modified_scope(path);
            }
        }
    }

    fn update_evidence_prepared(
        &self,
        engine: &mut ContextEngine,
        action: &PreparedAction,
        result: &ToolResult,
        executed: bool,
    ) {
        if !result.ok {
            return;
        }
        let revision = engine.snapshot().workflow.workspace_revision;
        let mut paths = result_paths(&action.tool, &action.normalized_input, result);
        if paths.is_empty() {
            paths.extend(action.paths.iter().cloned());
        }
        let kind = if action.classification == ActionClassification::Verification {
            DecisionEvidenceKind::Verification
        } else {
            match action.tool.as_str() {
                "read" => DecisionEvidenceKind::Inspection,
                "write" | "patch" => DecisionEvidenceKind::Mutation,
                _ => DecisionEvidenceKind::Discovery,
            }
        };
        let detail = if executed {
            format!("{} completed", action.tool)
        } else {
            format!("{} reused cached observation", action.tool)
        };
        let verification_scope =
            action.command_effects.as_ref().and_then(|effects| effects.verification_scope.clone());
        self.record_tool_evidence(
            engine,
            ToolEvidenceUpdate {
                executed,
                revision,
                paths,
                kind,
                detail,
                verification_scope,
                mutation: matches!(action.tool.as_str(), "write" | "patch"),
            },
        );
    }

    fn update_next_action(&self, engine: &mut ContextEngine) {
        let ranked = {
            let snapshot = engine.snapshot();
            self.rank_next_actions(snapshot)
        };
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
        let work = &snapshot.workflow.work;
        let work_detail = if work.is_structured() {
            let active = work.active_subgoal.as_ref().map_or("-", |active| active.as_str());
            format!(
                " work={} active={} pending={}",
                work.mode.as_str(),
                active,
                work.unresolved_count()
            )
        } else {
            String::new()
        };
        let failure = snapshot.workflow.unresolved_failures.last();
        let failure_text = failure.map_or("-".to_owned(), |failure| {
            format!(
                "{}:{} attempts={} category={}",
                failure.tool.as_str(),
                failure.code.as_str(),
                failure.attempts,
                failure.category.as_str()
            )
        });
        format!(
            "dir={} rec={} decision: action={} target={} confidence={} candidates={} questions={} scope={} blocker={}{}",
            snapshot.workflow.director.phase.as_str(),
            snapshot.workflow.recovery.action.as_str(),
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
            failure_text,
            work_detail,
        )
    }
}

fn targeted_selections(map: &RepoMap, task: &str, paths: &[&str]) -> Vec<RepoSelection> {
    let mut selections = paths
        .iter()
        .map(|path| RepoSelection {
            path: PathBuf::from(path),
            kind: RepoSelectionKind::File,
            score: 32,
            symbol: None,
        })
        .collect::<Vec<_>>();
    if let Ok(symbols) = map.lookup_symbols_in_paths(task, paths, MAX_POLICY_CANDIDATES) {
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
    if let Ok(relationships) = map.lookup_relationships_in_paths(task, paths, MAX_POLICY_CANDIDATES)
    {
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

fn relevant_paths(snapshot: &ContextSnapshot) -> Vec<&str> {
    let mut paths = Vec::new();
    for path in snapshot
        .workflow
        .relevant_files
        .iter()
        .chain(snapshot.modified.iter())
        .chain(snapshot.inspected.iter().map(|file| &file.path))
    {
        if !paths.contains(&path.as_str()) {
            paths.push(path);
        }
        if paths.len() == MAX_POLICY_CANDIDATES {
            break;
        }
    }
    paths
}

/// Build a read-shaped result only from a current, complete-enough retained excerpt. The real
/// read tool remains the fallback for every ambiguous, stale, partial, or byte-limited request.
fn cached_read_result(
    snapshot: &ContextSnapshot,
    call_id: ToolCallId,
    input: &Value,
) -> Option<ToolResult> {
    if snapshot.workspace_changed || input.get("max_bytes").is_some() {
        return None;
    }
    let targets = input.get("files")?.as_array()?;
    if targets.is_empty() || targets.len() > 64 {
        return None;
    }
    let mut files = Vec::with_capacity(targets.len());
    for target in targets {
        let path = target.get("path")?.as_str()?;
        if WorkspacePath::new(path).is_err() {
            return None;
        }
        let requested = match (target.get("start_line"), target.get("end_line")) {
            (None, None) => None,
            (Some(start), Some(end)) => Some(LineRange {
                start: usize::try_from(start.as_u64()?).ok()?,
                end: usize::try_from(end.as_u64()?).ok()?,
            }),
            _ => return None,
        };
        if requested.is_some_and(|range| range.start == 0 || range.start > range.end) {
            return None;
        }
        let inspected = snapshot.inspected.iter().find(|file| {
            file.path == path
                && file.last_state == ObservedFileState::Present
                && !file.stale
                && file.content_hash.is_some()
        })?;
        let hash = inspected.content_hash.as_deref()?;
        if !live_hash_matches(snapshot, path, hash) {
            return None;
        }
        let excerpt = snapshot.excerpts.iter().rev().find(|excerpt| {
            excerpt.path == path
                && excerpt.content_hash.as_deref() == Some(hash)
                && match requested {
                    Some(range) => {
                        range.start >= excerpt.start_line && range.end <= excerpt.end_line
                    }
                    None => excerpt.start_line == 1 && !excerpt.truncated,
                }
        })?;
        let start = requested.map_or(excerpt.start_line, |range| range.start);
        let end = requested.map_or(excerpt.end_line, |range| range.end);
        let lines = excerpt
            .text
            .as_str()
            .lines()
            .enumerate()
            .filter_map(|(index, text)| {
                let number = excerpt.start_line + index;
                (number >= start && number <= end).then(|| {
                    serde_json::json!({
                        "number": number,
                        "text": text,
                    })
                })
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return None;
        }
        files.push(serde_json::json!({
            "path": path,
            "binary": false,
            "truncated": false,
            "bytes_read": excerpt.text.as_str().len(),
            "content_hash": hash,
            "lines": lines,
        }));
    }
    let mut result = ToolResult::success_json(
        call_id,
        serde_json::json!({"files": files, "truncated": false}),
        aether_core::DEFAULT_MAX_OUTPUT_BYTES,
    );
    result.output = aether_core::BoundedText::new(
        format!("policy: reused current source evidence\n{}", result.output.as_str()),
        aether_core::DEFAULT_MAX_OUTPUT_BYTES,
    );
    Some(result)
}

/// Revalidate a cached observation against the live canonical file before suppressing a read.
/// This is a bounded freshness check, never an authority grant; the actual mutation tool still
/// performs its own permit, containment, and expected-hash checks.
fn live_hash_matches(snapshot: &ContextSnapshot, relative: &str, expected: &str) -> bool {
    let relative = match WorkspacePath::new(relative) {
        Ok(path) => path,
        Err(_) => return false,
    };
    let root = Path::new(&snapshot.workspace_root);
    let canonical_root = match root.canonicalize() {
        Ok(root) => root,
        Err(_) => return false,
    };
    let candidate = root.join(relative.as_path());
    let metadata = match fs::symlink_metadata(&candidate) {
        Ok(metadata) => metadata,
        Err(_) => return false,
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return false;
    }
    let canonical = match candidate.canonicalize() {
        Ok(path) => path,
        Err(_) => return false,
    };
    if !canonical.starts_with(canonical_root) {
        return false;
    }
    let file = match fs::File::open(canonical) {
        Ok(file) => file,
        Err(_) => return false,
    };
    let mut bytes = Vec::with_capacity(8192);
    if file.take((4 * 1024 * 1024 + 1) as u64).read_to_end(&mut bytes).is_err()
        || bytes.len() > 4 * 1024 * 1024
    {
        return false;
    }
    blake3::hash(&bytes).to_hex().as_ref() == expected
}

fn candidate_key(snapshot: &ContextSnapshot) -> [u8; 16] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(snapshot.current_task.as_str().as_bytes());
    hasher.update(&snapshot.workflow.workspace_revision.to_le_bytes());
    for path in relevant_paths(snapshot) {
        hasher.update(path.as_bytes());
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
    let has_paths = !effects.paths.is_empty() || include_manifests && !effects.manifests.is_empty();
    has_paths
        && effects.paths.iter().all(|path| command_target_has_evidence(snapshot, path))
        && (!include_manifests
            || effects.manifests.iter().all(|path| command_target_has_evidence(snapshot, path)))
}

fn command_target_has_evidence(snapshot: &ContextSnapshot, path: &str) -> bool {
    if snapshot.user_modified.iter().any(|user_path| user_path == path) {
        return false;
    }
    snapshot.workflow.relevant_files.iter().any(|relevant| relevant == path)
        && snapshot.inspected.iter().any(|file| {
            file.path == path
                && file.last_state == ObservedFileState::Present
                && !file.stale
                && file.content_hash.is_some()
        })
}

fn prepared_write_has_evidence(snapshot: &ContextSnapshot, action: &PreparedAction) -> bool {
    let Some(path) = action.paths.first() else { return false };
    let create_only =
        action.normalized_input.get("create_only").and_then(Value::as_bool) == Some(true);
    if create_only
        && WorkspacePath::new(path).is_ok()
        && new_target_has_discovered_parent(snapshot, path)
    {
        return !snapshot.user_modified.iter().any(|existing| existing == path);
    }
    current_target_has_evidence(snapshot, path)
        && expected_hash_matches(
            snapshot,
            path,
            action.normalized_input.get("expected_hash").and_then(Value::as_str),
        )
}

fn prepared_patch_has_evidence(snapshot: &ContextSnapshot, action: &PreparedAction) -> bool {
    let Some(files) = action.normalized_input.get("files").and_then(Value::as_array) else {
        return false;
    };
    !files.is_empty()
        && files.iter().all(|file| {
            let Some(path) = file.get("path").and_then(Value::as_str) else { return false };
            current_target_has_evidence(snapshot, path)
                && expected_hash_matches(
                    snapshot,
                    path,
                    file.get("expected_hash").and_then(Value::as_str),
                )
        })
}

fn insufficient_evidence(call_id: ToolCallId) -> ToolResult {
    ToolResult::failure(
        call_id,
        "insufficient_evidence",
        "inspect the exact target, preserve user-owned changes, and provide the current expected_hash before replacing it",
        true,
        aether_core::DEFAULT_MAX_OUTPUT_BYTES,
    )
}

fn mutation_has_evidence(snapshot: &ContextSnapshot, name: &str, input: &Value) -> bool {
    if name == "write" {
        let Some(path) = input.get("path").and_then(Value::as_str) else { return false };
        let create_only = input.get("create_only").and_then(Value::as_bool) == Some(true);
        if create_only
            && WorkspacePath::new(path).is_ok()
            && new_target_has_discovered_parent(snapshot, path)
        {
            return !snapshot.user_modified.iter().any(|existing| existing == path);
        }
        return current_target_has_evidence(snapshot, path)
            && expected_hash_matches(
                snapshot,
                path,
                input.get("expected_hash").and_then(Value::as_str),
            );
    }

    let Some(files) = input.get("files").and_then(Value::as_array) else { return false };
    !files.is_empty()
        && files.iter().all(|file| {
            let Some(path) = file.get("path").and_then(Value::as_str) else { return false };
            current_target_has_evidence(snapshot, path)
                && expected_hash_matches(
                    snapshot,
                    path,
                    file.get("expected_hash").and_then(Value::as_str),
                )
        })
}

fn current_target_has_evidence(snapshot: &ContextSnapshot, path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
        && snapshot.workflow.relevant_files.iter().any(|relevant| relevant == path)
        && snapshot.inspected.iter().any(|file| {
            file.path == path
                && file.last_state == ObservedFileState::Present
                && !file.stale
                && file.content_hash.is_some()
        })
}

fn expected_hash_matches(snapshot: &ContextSnapshot, path: &str, expected: Option<&str>) -> bool {
    let Some(file) = snapshot.inspected.iter().find(|file| file.path == path) else {
        return false;
    };
    let Some(expected) = expected else { return false };
    file.content_hash.as_deref() == Some(expected)
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
    fn successful_non_read_observations_are_not_reused_without_freshness() {
        let mut engine = ContextEngine::new("", None);
        let mut policy = AutonomousCodingPolicy::new();

        let search_input = json!({"patterns": ["needle"]});
        let search_result = ToolResult::success_text(
            ToolCallId::new("search-first").unwrap(),
            "first result",
            1024,
        );
        assert!(
            policy.observe(&mut engine, "search", &search_input, &search_result, true).is_some()
        );
        assert!(
            policy
                .preflight(
                    engine.snapshot(),
                    ToolCallId::new("search-repeat").unwrap(),
                    "search",
                    &search_input,
                )
                .is_none()
        );

        let verification_input = json!({"program": "cargo", "args": ["test", "-p", "api"]});
        let verification_result = ToolResult::success_text(
            ToolCallId::new("verification-first").unwrap(),
            "passed",
            1024,
        );
        let mut verification = PreparedAction::fallback(
            aether_core::ToolInvocation {
                call_id: ToolCallId::new("verification-first").unwrap(),
                name: "shell".to_owned(),
                input: verification_input.clone(),
            },
            aether_core::PermissionClass::ReadOnly,
        );
        verification.classification = ActionClassification::Verification;
        assert!(
            policy
                .observe_prepared(&mut engine, &verification, &verification_result, true)
                .is_some()
        );
        let mut repeated_verification = verification;
        repeated_verification.call_id = ToolCallId::new("verification-repeat").unwrap();
        assert!(policy.preflight_prepared(engine.snapshot(), &repeated_verification,).is_none());
        assert_eq!(policy.prevented_count(), 0);
    }

    #[test]
    fn repeated_non_read_failures_remain_bounded() {
        let mut engine = ContextEngine::new("", None);
        let mut policy = AutonomousCodingPolicy::new();
        let input = json!({"patterns": ["missing"]});

        for index in 0..3 {
            let result = ToolResult::failure(
                ToolCallId::new(format!("search-failure-{index}")).unwrap(),
                "io",
                "same failure",
                true,
                1024,
            );
            assert!(policy.observe(&mut engine, "search", &input, &result, true).is_some());
        }

        let blocked = policy
            .preflight(
                engine.snapshot(),
                ToolCallId::new("search-circuit").unwrap(),
                "search",
                &input,
            )
            .expect("repeated non-read failures must trip the circuit breaker");
        let error = blocked.error.expect("circuit breaker returns a typed error");
        assert_eq!(error.code.as_str(), "repeated_failure");
        assert!(!error.retryable);
    }

    #[test]
    fn compatible_read_calls_share_one_bounded_repository_plan() {
        let policy = AutonomousCodingPolicy::new();
        let snapshot = inspected_snapshot();
        let calls = vec![
            (
                ToolCallId::new("read-one").unwrap(),
                "read".to_owned(),
                json!({"files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 4}]}),
            ),
            (
                ToolCallId::new("read-two").unwrap(),
                "read".to_owned(),
                json!({"files": [{"path": "src/main.rs", "start_line": 1, "end_line": 4}]}),
            ),
        ];
        let plan = policy
            .repository_plan_for_calls(&snapshot, &calls)
            .expect("read-only calls are compatible");
        assert!(plan.is_usable());
        assert_eq!(plan.actions.len(), 2);
        assert!(plan.observation.collapsed);
    }

    #[test]
    fn planner_falls_back_for_search_semantics_it_does_not_model() {
        let policy = AutonomousCodingPolicy::new();
        let snapshot = inspected_snapshot();
        let calls = vec![(
            ToolCallId::new("regex-search").unwrap(),
            "search".to_owned(),
            json!({"patterns": ["parse"], "regex": true}),
        )];
        assert!(policy.repository_plan_for_calls(&snapshot, &calls).is_none());
    }

    #[test]
    fn dirty_or_stale_targets_require_explicit_current_evidence() {
        let policy = AutonomousCodingPolicy::new();
        let mut dirty = inspected_snapshot();
        dirty.user_modified.push("src/lib.rs".to_owned());
        let blocked = policy
            .preflight(
                &dirty,
                ToolCallId::new("dirty-write").unwrap(),
                "write",
                &json!({"path": "src/lib.rs"}),
            )
            .expect("dirty target without a hash must be blocked");
        assert_eq!(blocked.error.unwrap().code.as_str(), "insufficient_evidence");
        assert!(
            policy
                .preflight(
                    &dirty,
                    ToolCallId::new("explicit-write").unwrap(),
                    "write",
                    &json!({"path": "src/lib.rs", "expected_hash": "hash"}),
                )
                .is_none()
        );

        dirty.inspected[0].stale = true;
        let blocked = policy
            .preflight(
                &dirty,
                ToolCallId::new("stale-write").unwrap(),
                "write",
                &json!({"path": "src/lib.rs", "expected_hash": "hash"}),
            )
            .expect("stale target must be blocked");
        assert_eq!(blocked.error.unwrap().code.as_str(), "insufficient_evidence");
    }

    #[test]
    fn prepared_read_cache_rejects_external_edit_before_guardrail_reuse() {
        let root = std::env::temp_dir().join(format!(
            "aether-policy-live-read-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("src.rs");
        std::fs::write(&path, "fn before() {}\n").unwrap();
        let hash = blake3::hash(b"fn before() {}\n").to_hex().to_string();
        let input = json!({
            "files": [{"path": "src.rs", "start_line": 1, "end_line": 1}]
        });
        let result = ToolResult::success_json(
            ToolCallId::new("initial-read").unwrap(),
            json!({
                "files": [{
                    "path": "src.rs",
                    "content_hash": hash,
                    "lines": [{"number": 1, "text": "fn before() {}"}]
                }]
            }),
            aether_core::DEFAULT_MAX_OUTPUT_BYTES,
        );
        let mut engine = ContextEngine::new(root.display().to_string(), None);
        engine.set_task("inspect src.rs");
        let mut policy = AutonomousCodingPolicy::new();
        assert!(policy.observe(&mut engine, "read", &input, &result, true).is_some());
        let action = PreparedAction::fallback(
            aether_core::ToolInvocation {
                call_id: ToolCallId::new("cached-read").unwrap(),
                name: "read".to_owned(),
                input: input.clone(),
            },
            aether_core::PermissionClass::ReadOnly,
        );
        assert!(policy.preflight_prepared(engine.snapshot(), &action).is_some());

        std::fs::write(&path, "fn after() {}\n").unwrap();
        assert!(policy.preflight_prepared(engine.snapshot(), &action).is_none());
        let _ = std::fs::remove_dir_all(root);
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
