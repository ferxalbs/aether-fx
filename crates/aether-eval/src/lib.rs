use serde::Serialize;
use std::fs;
use std::path::Path;

#[cfg(test)]
use aether_agent::RepositoryActionPlanner;
#[cfg(test)]
use aether_core::ContextSnapshot;

mod real;
mod trajectory;

pub use trajectory::{TrajectoryMetrics, TrajectoryPattern};

#[derive(Clone, Debug)]
pub enum Action {
    List { path: &'static str },
    Read { path: &'static str },
    Search { query: &'static str, path: &'static str },
    Write { path: &'static str, contents: &'static str },
    Shell { program: &'static str, args: &'static [&'static str] },
    Verify,
    Finish,
}

/// Replaceable model boundary. Real-provider adapters can implement this trait without changing
/// fixture execution, metrics, thresholds, or result formatting.
pub trait EvalBackend {
    fn name(&self) -> &str;
    fn reset(&mut self, task: &Task);
    fn next_action(&mut self, observation: &str) -> Action;

    fn trace(&mut self, task: &Task) -> Vec<Action> {
        self.reset(task);
        let mut trace = Vec::new();
        for _ in 0..task.max_steps.saturating_add(task.max_tool_calls).saturating_add(1) {
            let action = self.next_action("");
            let done = matches!(action, Action::Finish);
            trace.push(action);
            if done {
                break;
            }
        }
        trace
    }
}

#[derive(Clone, Debug)]
pub struct ExpectedOutcome {
    pub path: &'static str,
    pub contains: &'static str,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: &'static str,
    pub prompt: &'static str,
    pub fixture: &'static str,
    pub expected_outcomes: &'static [ExpectedOutcome],
    pub verification_command: &'static [&'static str],
    pub focused_verification_command: Option<&'static [&'static str]>,
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_verification_attempts: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ExecutionMetrics {
    pub success: bool,
    pub model_steps: u32,
    pub model_requests: u32,
    pub requested_tool_calls: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub static_prefix_bytes: u64,
    pub tool_schema_bytes: u64,
    pub context_payload_bytes: u64,
    pub tool_result_bytes: u64,
    pub policy_feedback_bytes: u64,
    pub protocol_envelope_bytes: u64,
    pub verification_attempts: u32,
    pub automatic_verification_attempts: u32,
    pub verification_failures: u32,
    pub recovery_attempts: u32,
    pub recovery_successes: u32,
    pub stale_evidence_violations: u32,
    pub reused_observations: u32,
    pub automatic_evidence_reactions: u32,
    pub diagnostic_hydration_events: u32,
    pub mutation_refresh_events: u32,
    pub stale_evidence_invalidations: u32,
    pub verification_escalations: u32,
    pub completion_rejections: u32,
    pub unresolved_work_at_finish: u32,
    pub process_spawns: u32,
    pub wall_time_ms: u64,
    pub provider_wait_ms: u64,
    pub local_execution_ms: u64,
    pub cpu_time_ms: u64,
    pub peak_rss_bytes: u64,
    pub allocation_count: u64,
    pub verification_scope: String,
    pub verification_quality: String,
    pub planner_attempts: u32,
    pub planner_hits: u32,
    pub planner_aborted_ambiguity: u32,
    pub planner_actions: u32,
    pub planner_bytes_read: u64,
    pub trajectory: TrajectoryMetrics,
}

impl ExecutionMetrics {
    #[must_use]
    pub fn planner_hit_rate(&self) -> f64 {
        if self.planner_attempts == 0 {
            0.0
        } else {
            self.planner_hits as f64 / self.planner_attempts as f64
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TaskResult {
    pub task_id: String,
    pub prompt: String,
    pub expected_outcomes: Vec<String>,
    pub verification_command: Vec<String>,
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_verification_attempts: u32,
    pub baseline_success: bool,
    pub before: ExecutionMetrics,
    pub after: ExecutionMetrics,
    pub success: bool,
    pub wall_time_ms: u128,
    pub harness_overhead_ms: u128,
    pub model_steps: u32,
    pub model_requests: u32,
    pub requested_tool_calls: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub static_prefix_bytes: u64,
    pub tool_schema_bytes: u64,
    pub context_payload_bytes: u64,
    pub tool_result_bytes: u64,
    pub policy_feedback_bytes: u64,
    pub protocol_envelope_bytes: u64,
    pub verification_attempts: u32,
    pub automatic_verification_attempts: u32,
    pub verification_failures: u32,
    pub recovery_attempts: u32,
    pub recovery_successes: u32,
    pub stale_evidence_violations: u32,
    pub reused_observations: u32,
    pub automatic_evidence_reactions: u32,
    pub diagnostic_hydration_events: u32,
    pub mutation_refresh_events: u32,
    pub stale_evidence_invalidations: u32,
    pub verification_escalations: u32,
    pub completion_rejections: u32,
    pub unresolved_work_at_finish: u32,
    pub process_spawns: u32,
    pub agent_wall_time_ms: u64,
    pub provider_wait_ms: u64,
    pub local_execution_ms: u64,
    pub cpu_time_ms: u64,
    pub peak_rss_bytes: u64,
    pub allocation_count: u64,
    pub verification_scope: String,
    pub verification_quality: String,
    pub final_test_status: String,
    pub failure: Option<String>,
    pub trajectory: TrajectoryMetrics,
}

/// Result for one difficult long-horizon task executed through a checkpoint/resume boundary.
#[derive(Debug, Serialize)]
pub struct CapabilityTaskResult {
    pub task_id: String,
    pub category: String,
    pub baseline_success: bool,
    pub resumed_success: bool,
    pub success_at_1: bool,
    pub trials: u32,
    pub previously_unsolved: bool,
    pub multi_file_correctness: bool,
    pub checkpoint_captured: bool,
    pub work_state_survived: bool,
    pub recovery_observed: bool,
    pub user_change_tested: bool,
    pub user_change_corruption: bool,
    pub stale_evidence_violations: u32,
    pub model_requests: u32,
    pub tool_executions: u32,
    pub process_spawns: u32,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub wall_time_ms: u64,
    pub verification_attempts: u32,
    pub automatic_verification_attempts: u32,
    pub verification_failures: u32,
    pub reused_observations: u32,
    pub automatic_evidence_reactions: u32,
    pub diagnostic_hydration_events: u32,
    pub mutation_refresh_events: u32,
    pub stale_evidence_invalidations: u32,
    pub verification_escalations: u32,
    pub completion_rejections: u32,
    pub unresolved_work_at_finish: u32,
    pub failure: Option<String>,
}

/// Aggregate capability metrics kept separate from the deterministic economy/regression gate.
#[derive(Debug, Serialize)]
pub struct CapabilityAggregate {
    pub tasks: usize,
    pub trials: u32,
    pub succeeded: usize,
    pub success_rate: f64,
    pub success_at_1: usize,
    pub baseline_succeeded: usize,
    pub baseline_success_rate: f64,
    pub previously_unsolved: usize,
    pub multi_file_correct: usize,
    pub checkpoint_resume_successes: usize,
    pub recovery_successes: usize,
    pub user_change_tests: usize,
    pub user_change_corruptions: usize,
    pub false_completion_count: usize,
    pub stale_evidence_violations: u32,
    pub model_requests: u32,
    pub tool_executions: u32,
    pub process_spawns: u32,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub wall_time_ms: u64,
    pub verification_attempts: u32,
    pub automatic_verification_attempts: u32,
    pub verification_failures: u32,
    pub reused_observations: u32,
    pub automatic_evidence_reactions: u32,
    pub diagnostic_hydration_events: u32,
    pub mutation_refresh_events: u32,
    pub stale_evidence_invalidations: u32,
    pub verification_escalations: u32,
    pub completion_rejections: u32,
    pub unresolved_work_at_finish: u32,
}

/// One same-trace comparison for runtime-owned verification and zero-waste control.
#[derive(Debug, Serialize)]
pub struct ControlLoopCaseResult {
    pub case_id: String,
    pub baseline_success: bool,
    pub optimized_success: bool,
    pub correctness_preserved: bool,
    pub automatic_verification_attempts: u32,
    pub prevented_redundant_calls: u32,
    pub baseline_model_requests: u32,
    pub optimized_model_requests: u32,
    pub baseline_requested_tool_calls: u32,
    pub optimized_requested_tool_calls: u32,
    pub baseline_executed_tool_calls: u32,
    pub optimized_executed_tool_calls: u32,
    pub verification_scope: String,
    pub completion_rejections: u32,
    pub failure: Option<String>,
}

/// Deterministic end-to-end checks for the runtime/model control seam.
#[derive(Debug, Serialize)]
pub struct ControlLoopEvaluation {
    pub total: usize,
    pub passed: usize,
    pub cases: Vec<ControlLoopCaseResult>,
}

/// Separate hard-task suite. Its threshold requires a real gain over the interrupted baseline;
/// it is never folded into the 9/9 deterministic regression suite.
#[derive(Debug, Serialize)]
pub struct CapabilitySuiteResult {
    pub schema_version: u32,
    pub backend: String,
    pub success: bool,
    pub aggregate: CapabilityAggregate,
    pub tasks: Vec<CapabilityTaskResult>,
    pub control_loop: ControlLoopEvaluation,
}

#[derive(Debug, Serialize)]
pub struct AggregateMetrics {
    pub tasks: usize,
    pub succeeded: usize,
    pub success_rate: f64,
    pub model_steps: u32,
    pub model_requests: u32,
    pub requested_tool_calls: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub static_prefix_bytes: u64,
    pub tool_schema_bytes: u64,
    pub context_payload_bytes: u64,
    pub tool_result_bytes: u64,
    pub policy_feedback_bytes: u64,
    pub protocol_envelope_bytes: u64,
    pub verification_attempts: u32,
    pub automatic_verification_attempts: u32,
    pub verification_failures: u32,
    pub recovery_attempts: u32,
    pub recovery_successes: u32,
    pub stale_evidence_violations: u32,
    pub reused_observations: u32,
    pub automatic_evidence_reactions: u32,
    pub diagnostic_hydration_events: u32,
    pub mutation_refresh_events: u32,
    pub stale_evidence_invalidations: u32,
    pub verification_escalations: u32,
    pub completion_rejections: u32,
    pub unresolved_work_at_finish: u32,
    pub process_spawns: u32,
    pub agent_wall_time_ms: u64,
    pub provider_wait_ms: u64,
    pub local_execution_ms: u64,
    pub cpu_time_ms: u64,
    pub peak_rss_bytes: u64,
    pub allocation_count: u64,
    pub wall_time_ms: u128,
    pub harness_overhead_ms: u128,
    pub baseline_succeeded: usize,
    pub baseline_success_rate: f64,
    pub baseline_model_steps: u32,
    pub baseline_model_requests: u32,
    pub baseline_requested_tool_calls: u32,
    pub baseline_tool_calls: u32,
    pub baseline_executed_tool_calls: u32,
    pub baseline_bytes_read: u64,
    pub baseline_context_bytes: u64,
    pub baseline_bytes_shown_to_model: u64,
    pub verification_scope_bytes: u64,
    pub planner_attempts: u32,
    pub planner_hits: u32,
    pub planner_hit_rate: f64,
    pub planner_aborted_ambiguity: u32,
    pub planner_actions: u32,
    pub planner_bytes_read: u64,
    pub trajectory_patterns: Vec<TrajectoryPattern>,
}

#[derive(Debug, Serialize)]
pub struct RegressionThresholds {
    pub minimum_success_rate: f64,
    pub maximum_model_steps: u32,
    pub maximum_executed_tool_calls: u32,
    pub maximum_context_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct SuiteResult {
    pub schema_version: u32,
    pub backend: String,
    pub success: bool,
    pub thresholds: RegressionThresholds,
    pub aggregate: AggregateMetrics,
    pub tasks: Vec<TaskResult>,
}

pub struct ScriptedBackend {
    name: &'static str,
    script: &'static [Action],
    cursor: usize,
}

impl ScriptedBackend {
    #[must_use]
    pub const fn new(name: &'static str, script: &'static [Action]) -> Self {
        Self { name, script, cursor: 0 }
    }
}

impl EvalBackend for ScriptedBackend {
    fn name(&self) -> &str {
        self.name
    }

    fn reset(&mut self, _task: &Task) {
        self.cursor = 0;
    }

    fn next_action(&mut self, observation: &str) -> Action {
        if observation.starts_with("planner_observation:") {
            while matches!(self.script.get(self.cursor), Some(Action::Read { .. })) {
                self.cursor += 1;
            }
        }
        let action = self.script.get(self.cursor).cloned().unwrap_or(Action::Finish);
        self.cursor += 1;
        action
    }
}

pub fn run_suite(output: Option<&Path>) -> Result<SuiteResult, String> {
    let mut results = Vec::new();
    for scenario in scenarios() {
        let baseline = real::run_task(scenario.task, scenario.script, false)?;
        let baseline_metrics = baseline.after.clone();
        let mut policy = real::run_task(scenario.task, scenario.script, true)?;
        policy.baseline_success = baseline.success;
        policy.before = baseline_metrics;
        results.push(policy);
    }

    let aggregate = aggregate(&results);
    let thresholds = RegressionThresholds {
        minimum_success_rate: 1.0,
        maximum_model_steps: 55,
        maximum_executed_tool_calls: 40,
        maximum_context_bytes: 40_000,
    };
    let success = aggregate.success_rate >= thresholds.minimum_success_rate
        && aggregate.model_steps <= thresholds.maximum_model_steps
        && aggregate.executed_tool_calls <= thresholds.maximum_executed_tool_calls
        && aggregate.context_bytes <= thresholds.maximum_context_bytes;
    let suite = SuiteResult {
        schema_version: 1,
        backend: "deterministic-fake".to_owned(),
        success,
        thresholds,
        aggregate,
        tasks: results,
    };
    if let Some(path) = output {
        let json = serde_json::to_vec_pretty(&suite).map_err(|error| error.to_string())?;
        fs::write(path, json).map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    Ok(suite)
}

/// Run genuinely difficult capability scenarios independently from the 9/9 regression gate.
pub fn run_capability_suite(output: Option<&Path>) -> Result<CapabilitySuiteResult, String> {
    let cases = [
        (
            &MULTI_FILE,
            LONG_HORIZON_PREFIX,
            LONG_HORIZON_REMAINING,
            "cross-module multi-file long horizon",
            None,
            None,
        ),
        (
            &FAILED_FIRST,
            COMPILER_RECOVERY_PREFIX,
            COMPILER_RECOVERY_REMAINING,
            "compiler-error-driven recovery",
            None,
            None,
        ),
        (
            &WORKSPACE_TASK,
            WORKSPACE_PREFIX,
            WORKSPACE_REMAINING,
            "cross-crate workspace migration",
            None,
            None,
        ),
        (
            &STALE_OBSERVATION,
            STALE_PREFIX,
            STALE_REMAINING,
            "stale observation recovery",
            Some(("src/lib.rs", STALE_EXTERNAL)),
            None,
        ),
        (
            &DIRTY_WORKTREE,
            DIRTY_PREFIX,
            DIRTY_REMAINING,
            "dirty-tree user-work preservation",
            Some(("README.md", DIRTY_NOTE)),
            Some("README.md"),
        ),
    ];
    let tasks = cases
        .into_iter()
        .map(|(task, first, remaining, category, external_change, preserved_path)| {
            real::run_resumable_task_with_change(
                task,
                first,
                remaining,
                category,
                external_change,
                preserved_path,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let control_loop = real::run_control_loop_evaluation()?;
    let succeeded = tasks.iter().filter(|task| task.resumed_success).count();
    let baseline_succeeded = tasks.iter().filter(|task| task.baseline_success).count();
    let checkpoint_resume_successes =
        tasks.iter().filter(|task| task.checkpoint_captured && task.resumed_success).count();
    let recovery_successes = tasks.iter().filter(|task| task.recovery_observed).count();
    let false_completion_count = tasks
        .iter()
        .filter(|task| task.resumed_success && task.unresolved_work_at_finish > 0)
        .count();
    let aggregate = CapabilityAggregate {
        tasks: tasks.len(),
        trials: tasks.iter().map(|task| task.trials).sum(),
        succeeded,
        success_rate: ratio(succeeded, tasks.len()),
        success_at_1: tasks.iter().filter(|task| task.success_at_1).count(),
        baseline_succeeded,
        baseline_success_rate: ratio(baseline_succeeded, tasks.len()),
        previously_unsolved: tasks.iter().filter(|task| task.previously_unsolved).count(),
        multi_file_correct: tasks.iter().filter(|task| task.multi_file_correctness).count(),
        checkpoint_resume_successes,
        recovery_successes,
        user_change_tests: tasks.iter().filter(|task| task.user_change_tested).count(),
        user_change_corruptions: tasks.iter().filter(|task| task.user_change_corruption).count(),
        false_completion_count,
        stale_evidence_violations: tasks.iter().map(|task| task.stale_evidence_violations).sum(),
        model_requests: tasks.iter().map(|task| task.model_requests).sum(),
        tool_executions: tasks.iter().map(|task| task.tool_executions).sum(),
        process_spawns: tasks.iter().map(|task| task.process_spawns).sum(),
        context_bytes: tasks.iter().map(|task| task.context_bytes).sum(),
        bytes_shown_to_model: tasks.iter().map(|task| task.bytes_shown_to_model).sum(),
        wall_time_ms: tasks.iter().map(|task| task.wall_time_ms).sum(),
        verification_attempts: tasks.iter().map(|task| task.verification_attempts).sum(),
        automatic_verification_attempts: tasks
            .iter()
            .map(|task| task.automatic_verification_attempts)
            .sum(),
        verification_failures: tasks.iter().map(|task| task.verification_failures).sum(),
        reused_observations: tasks.iter().map(|task| task.reused_observations).sum(),
        automatic_evidence_reactions: tasks
            .iter()
            .map(|task| task.automatic_evidence_reactions)
            .sum(),
        diagnostic_hydration_events: tasks
            .iter()
            .map(|task| task.diagnostic_hydration_events)
            .sum(),
        mutation_refresh_events: tasks.iter().map(|task| task.mutation_refresh_events).sum(),
        stale_evidence_invalidations: tasks
            .iter()
            .map(|task| task.stale_evidence_invalidations)
            .sum(),
        verification_escalations: tasks.iter().map(|task| task.verification_escalations).sum(),
        completion_rejections: tasks.iter().map(|task| task.completion_rejections).sum(),
        unresolved_work_at_finish: tasks.iter().map(|task| task.unresolved_work_at_finish).sum(),
    };
    let success = aggregate.succeeded > aggregate.baseline_succeeded
        && aggregate.checkpoint_resume_successes == aggregate.tasks
        && aggregate.user_change_corruptions == 0
        && aggregate.false_completion_count == 0
        && control_loop.passed == control_loop.total;
    let suite = CapabilitySuiteResult {
        schema_version: 1,
        backend: "deterministic-checkpoint-replay".to_owned(),
        success,
        aggregate,
        tasks,
        control_loop,
    };
    if let Some(path) = output {
        let json = serde_json::to_vec_pretty(&suite).map_err(|error| error.to_string())?;
        fs::write(path, json).map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    Ok(suite)
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 { 0.0 } else { numerator as f64 / denominator as f64 }
}

/// Compact capability output suitable for CI logs without hiding per-task JSON evidence.
pub fn compact_capability_summary(suite: &CapabilitySuiteResult) -> String {
    let mut output = String::new();
    for task in &suite.tasks {
        let mark = if task.resumed_success { "PASS" } else { "FAIL" };
        output.push_str(&format!(
            "{mark:4} {:28} baseline={} checkpoint={} work={} recovery={} user_change={} verify={}/{} auto_verify={} stale={} auto={} hydrate={} refresh={} escalate={} reject={} unresolved={}\n",
            task.task_id,
            task.baseline_success,
            task.checkpoint_captured,
            task.work_state_survived,
            task.recovery_observed,
            task.user_change_tested && !task.user_change_corruption,
            task.verification_attempts,
            task.verification_failures,
            task.automatic_verification_attempts,
            task.stale_evidence_violations,
            task.automatic_evidence_reactions,
            task.diagnostic_hydration_events,
            task.mutation_refresh_events,
            task.verification_escalations,
            task.completion_rejections,
            task.unresolved_work_at_finish,
        ));
    }
    output.push_str(&format!(
        "capability: {}/{} ({:.0}%) success_at_1={} baseline={}/{} ({:.0}%) newly_solved={} checkpoint_resume={} recovery={} user_change={}/{} corruptions={} false_completion={} stale={} auto_verify={} auto={} hydrate={} refresh={} escalate={} reject={} unresolved={} requests={} tools={} context={}B shown={}B wall={}ms\n",
        suite.aggregate.succeeded,
        suite.aggregate.tasks,
        suite.aggregate.success_rate * 100.0,
        suite.aggregate.success_at_1,
        suite.aggregate.baseline_succeeded,
        suite.aggregate.tasks,
        suite.aggregate.baseline_success_rate * 100.0,
        suite.aggregate.previously_unsolved,
        suite.aggregate.checkpoint_resume_successes,
        suite.aggregate.recovery_successes,
        suite.aggregate.user_change_tests - suite.aggregate.user_change_corruptions,
        suite.aggregate.user_change_tests,
        suite.aggregate.user_change_corruptions,
        suite.aggregate.false_completion_count,
        suite.aggregate.stale_evidence_violations,
        suite.aggregate.automatic_verification_attempts,
        suite.aggregate.automatic_evidence_reactions,
        suite.aggregate.diagnostic_hydration_events,
        suite.aggregate.mutation_refresh_events,
        suite.aggregate.verification_escalations,
        suite.aggregate.completion_rejections,
        suite.aggregate.unresolved_work_at_finish,
        suite.aggregate.model_requests,
        suite.aggregate.tool_executions,
        suite.aggregate.context_bytes,
        suite.aggregate.bytes_shown_to_model,
        suite.aggregate.wall_time_ms,
    ));
    let automatic = suite
        .control_loop
        .cases
        .iter()
        .map(|case| case.automatic_verification_attempts)
        .sum::<u32>();
    let reduced = suite
        .control_loop
        .cases
        .iter()
        .filter(|case| case.baseline_executed_tool_calls > case.optimized_executed_tool_calls)
        .count();
    output.push_str(&format!(
        "control_loop: {}/{} same_trace_reduced={} automatic_verifications={}\n",
        suite.control_loop.passed, suite.control_loop.total, reduced, automatic,
    ));
    output
}

/// Built-in task definitions for provider comparison runners.
#[must_use]
pub fn task_catalog() -> [&'static Task; 9] {
    [
        &TARGETED_BUG,
        &MULTI_FILE,
        &FAILED_FIRST,
        &REDUNDANT_READ,
        &FOCUSED_VERIFY,
        &INSUFFICIENT_EVIDENCE,
        &TARGETED_EXPLORATION,
        &SHELL_HEAVY,
        &RELATIONSHIP_HEAVY_EXPLORATION,
    ]
}

pub fn compact_summary(suite: &SuiteResult) -> String {
    let mut output = String::new();
    for result in &suite.tasks {
        let mark = if result.success { "PASS" } else { "FAIL" };
        output.push_str(&format!(
            "{mark:4} {:28} steps={:2} tools={:2}/{:2} before={} verify={} auto_verify={} scope={} quality={} read={}B reuse={} auto={} hydrate={} refresh={} stale={} escalate={} reject={} patterns={}\n",
            result.task_id,
            result.model_steps,
            result.executed_tool_calls,
            result.tool_calls,
            result.before.executed_tool_calls,
            result.verification_attempts,
            result.after.automatic_verification_attempts,
            result.verification_scope,
            result.verification_quality,
            result.bytes_read,
            result.after.trajectory.reused_observations,
            result.after.automatic_evidence_reactions,
            result.after.diagnostic_hydration_events,
            result.after.mutation_refresh_events,
            result.after.stale_evidence_invalidations,
            result.after.verification_escalations,
            result.after.completion_rejections,
            result.before.trajectory.patterns.len()
        ));
    }
    output.push_str(&format!(
        "total: {}/{} ({:.0}%) baseline={}/{} ({:.0}%) requests={}/{} steps={}/{} tools={}/{} baseline_tools={}/{} redundant={} reuse={} verify_auto={} auto={} hydrate={} refresh={} stale={} escalate={} reject={} context={}B/{}B shown={}B/{}B schema={}B result={}B policy={}B protocol={}B planner={}/{} ({:.0}%) ambiguous={} actions={} read={}B spawns={} wall={}ms local={}ms overhead={}ms\n",
        suite.aggregate.succeeded,
        suite.aggregate.tasks,
        suite.aggregate.success_rate * 100.0,
        suite.aggregate.baseline_succeeded,
        suite.aggregate.tasks,
        suite.aggregate.baseline_success_rate * 100.0,
        suite.aggregate.model_requests,
        suite.aggregate.baseline_model_requests,
        suite.aggregate.model_steps,
        suite.aggregate.baseline_model_steps,
        suite.aggregate.executed_tool_calls,
        suite.aggregate.tool_calls,
        suite.aggregate.baseline_tool_calls,
        suite.aggregate.baseline_executed_tool_calls,
        suite.aggregate.prevented_redundant_calls,
        suite.aggregate.reused_observations,
        suite.aggregate.automatic_verification_attempts,
        suite.aggregate.automatic_evidence_reactions,
        suite.aggregate.diagnostic_hydration_events,
        suite.aggregate.mutation_refresh_events,
        suite.aggregate.stale_evidence_invalidations,
        suite.aggregate.verification_escalations,
        suite.aggregate.completion_rejections,
        suite.aggregate.context_bytes,
        suite.aggregate.baseline_context_bytes,
        suite.aggregate.bytes_shown_to_model,
        suite.aggregate.baseline_bytes_shown_to_model,
        suite.aggregate.tool_schema_bytes,
        suite.aggregate.tool_result_bytes,
        suite.aggregate.policy_feedback_bytes,
        suite.aggregate.protocol_envelope_bytes,
        suite.aggregate.planner_hits,
        suite.aggregate.planner_attempts,
        suite.aggregate.planner_hit_rate * 100.0,
        suite.aggregate.planner_aborted_ambiguity,
        suite.aggregate.planner_actions,
        suite.aggregate.planner_bytes_read,
        suite.aggregate.process_spawns,
        suite.aggregate.wall_time_ms,
        suite.aggregate.local_execution_ms,
        suite.aggregate.harness_overhead_ms
    ));
    output
}

struct Scenario {
    task: &'static Task,
    script: &'static [Action],
}

fn scenarios() -> [Scenario; 9] {
    [
        Scenario { task: &TARGETED_BUG, script: TARGETED_SCRIPT },
        Scenario { task: &MULTI_FILE, script: MULTI_FILE_SCRIPT },
        Scenario { task: &FAILED_FIRST, script: FAILED_FIRST_SCRIPT },
        Scenario { task: &REDUNDANT_READ, script: REDUNDANT_SCRIPT },
        Scenario { task: &FOCUSED_VERIFY, script: FOCUSED_SCRIPT },
        Scenario { task: &INSUFFICIENT_EVIDENCE, script: INSUFFICIENT_EVIDENCE_SCRIPT },
        Scenario { task: &TARGETED_EXPLORATION, script: TARGETED_EXPLORATION_SCRIPT },
        Scenario { task: &SHELL_HEAVY, script: SHELL_HEAVY_SCRIPT },
        Scenario {
            task: &RELATIONSHIP_HEAVY_EXPLORATION,
            script: RELATIONSHIP_HEAVY_EXPLORATION_SCRIPT,
        },
    ]
}

pub fn run_task(task: &Task, backend: &mut dyn EvalBackend) -> Result<TaskResult, String> {
    let script = backend.trace(task);
    let mut result = real::run_task(task, &script, true)?;
    result.baseline_success = result.success;
    result.before = result.after.clone();
    Ok(result)
}

fn aggregate(results: &[TaskResult]) -> AggregateMetrics {
    let succeeded = results.iter().filter(|result| result.success).count();
    AggregateMetrics {
        tasks: results.len(),
        succeeded,
        success_rate: succeeded as f64 / results.len() as f64,
        model_steps: results.iter().map(|result| result.model_steps).sum(),
        model_requests: results.iter().map(|result| result.after.model_requests).sum(),
        requested_tool_calls: results.iter().map(|result| result.after.requested_tool_calls).sum(),
        tool_calls: results.iter().map(|result| result.tool_calls).sum(),
        executed_tool_calls: results.iter().map(|result| result.executed_tool_calls).sum(),
        prevented_redundant_calls: results
            .iter()
            .map(|result| result.prevented_redundant_calls)
            .sum(),
        input_tokens: results.iter().map(|result| result.after.input_tokens.unwrap_or(0)).sum(),
        output_tokens: results.iter().map(|result| result.after.output_tokens.unwrap_or(0)).sum(),
        total_tokens: results.iter().map(|result| result.after.total_tokens.unwrap_or(0)).sum(),
        bytes_read: results.iter().map(|result| result.bytes_read).sum(),
        context_bytes: results.iter().map(|result| result.context_bytes).sum(),
        bytes_shown_to_model: results.iter().map(|result| result.after.bytes_shown_to_model).sum(),
        static_prefix_bytes: results.iter().map(|result| result.after.static_prefix_bytes).sum(),
        tool_schema_bytes: results.iter().map(|result| result.after.tool_schema_bytes).sum(),
        context_payload_bytes: results
            .iter()
            .map(|result| result.after.context_payload_bytes)
            .sum(),
        tool_result_bytes: results.iter().map(|result| result.after.tool_result_bytes).sum(),
        policy_feedback_bytes: results
            .iter()
            .map(|result| result.after.policy_feedback_bytes)
            .sum(),
        protocol_envelope_bytes: results
            .iter()
            .map(|result| result.after.protocol_envelope_bytes)
            .sum(),
        verification_attempts: results.iter().map(|result| result.verification_attempts).sum(),
        automatic_verification_attempts: results
            .iter()
            .map(|result| result.after.automatic_verification_attempts)
            .sum(),
        verification_failures: results
            .iter()
            .map(|result| result.after.verification_failures)
            .sum(),
        recovery_attempts: results.iter().map(|result| result.after.recovery_attempts).sum(),
        recovery_successes: results.iter().map(|result| result.after.recovery_successes).sum(),
        stale_evidence_violations: results
            .iter()
            .map(|result| result.after.stale_evidence_violations)
            .sum(),
        reused_observations: results.iter().map(|result| result.after.reused_observations).sum(),
        automatic_evidence_reactions: results
            .iter()
            .map(|result| result.after.automatic_evidence_reactions)
            .sum(),
        diagnostic_hydration_events: results
            .iter()
            .map(|result| result.after.diagnostic_hydration_events)
            .sum(),
        mutation_refresh_events: results
            .iter()
            .map(|result| result.after.mutation_refresh_events)
            .sum(),
        stale_evidence_invalidations: results
            .iter()
            .map(|result| result.after.stale_evidence_invalidations)
            .sum(),
        verification_escalations: results
            .iter()
            .map(|result| result.after.verification_escalations)
            .sum(),
        completion_rejections: results
            .iter()
            .map(|result| result.after.completion_rejections)
            .sum(),
        unresolved_work_at_finish: results
            .iter()
            .map(|result| result.after.unresolved_work_at_finish)
            .sum(),
        process_spawns: results.iter().map(|result| result.after.process_spawns).sum(),
        agent_wall_time_ms: results.iter().map(|result| result.after.wall_time_ms).sum(),
        provider_wait_ms: results.iter().map(|result| result.after.provider_wait_ms).sum(),
        local_execution_ms: results.iter().map(|result| result.after.local_execution_ms).sum(),
        cpu_time_ms: results.iter().map(|result| result.after.cpu_time_ms).sum(),
        peak_rss_bytes: results.iter().map(|result| result.after.peak_rss_bytes).max().unwrap_or(0),
        allocation_count: results.iter().map(|result| result.after.allocation_count).sum(),
        wall_time_ms: results.iter().map(|result| result.wall_time_ms).sum(),
        harness_overhead_ms: results.iter().map(|result| result.harness_overhead_ms).sum(),
        baseline_succeeded: results.iter().filter(|result| result.baseline_success).count(),
        baseline_success_rate: results.iter().filter(|result| result.baseline_success).count()
            as f64
            / results.len() as f64,
        baseline_model_steps: results.iter().map(|result| result.before.model_steps).sum(),
        baseline_model_requests: results.iter().map(|result| result.before.model_requests).sum(),
        baseline_requested_tool_calls: results
            .iter()
            .map(|result| result.before.requested_tool_calls)
            .sum(),
        baseline_tool_calls: results.iter().map(|result| result.before.tool_calls).sum(),
        baseline_executed_tool_calls: results
            .iter()
            .map(|result| result.before.executed_tool_calls)
            .sum(),
        baseline_bytes_read: results.iter().map(|result| result.before.bytes_read).sum(),
        baseline_context_bytes: results.iter().map(|result| result.before.context_bytes).sum(),
        baseline_bytes_shown_to_model: results
            .iter()
            .map(|result| result.before.bytes_shown_to_model)
            .sum(),
        verification_scope_bytes: results
            .iter()
            .map(|result| result.verification_scope.len() as u64)
            .sum(),
        planner_attempts: results.iter().map(|result| result.after.planner_attempts).sum(),
        planner_hits: results.iter().map(|result| result.after.planner_hits).sum(),
        planner_hit_rate: {
            let attempts: u32 = results.iter().map(|result| result.after.planner_attempts).sum();
            let hits: u32 = results.iter().map(|result| result.after.planner_hits).sum();
            if attempts == 0 { 0.0 } else { hits as f64 / attempts as f64 }
        },
        planner_aborted_ambiguity: results
            .iter()
            .map(|result| result.after.planner_aborted_ambiguity)
            .sum(),
        planner_actions: results.iter().map(|result| result.after.planner_actions).sum(),
        planner_bytes_read: results.iter().map(|result| result.after.planner_bytes_read).sum(),
        trajectory_patterns: merge_trajectory_patterns(
            results.iter().map(|result| &result.before.trajectory),
        ),
    }
}

fn merge_trajectory_patterns<'a>(
    analyses: impl IntoIterator<Item = &'a TrajectoryMetrics>,
) -> Vec<TrajectoryPattern> {
    let mut merged = Vec::new();
    for analysis in analyses {
        for pattern in &analysis.patterns {
            if let Some(existing) =
                merged.iter_mut().find(|item: &&mut TrajectoryPattern| item.kind == pattern.kind)
            {
                existing.count = existing.count.saturating_add(pattern.count);
                for example in &pattern.examples {
                    if existing.examples.len() < 3 && !existing.examples.contains(example) {
                        existing.examples.push(example.clone());
                    }
                }
            } else {
                merged.push(pattern.clone());
            }
        }
    }
    merged
}

const CARGO_TEST: &[&str] = &["cargo", "test", "--quiet", "--offline"];
const FOCUSED_TEST: &[&str] = &["cargo", "test", "--quiet", "--offline", "formats_slug"];

const TARGETED_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/lib.rs", contains: "values.len() - 1" }];
const TARGETED_BUG: Task = Task {
    id: "targeted-bug-repair",
    prompt: "Fix last_index so it returns the final valid index instead of one past the slice.",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 5,
    max_tool_calls: 4,
    max_verification_attempts: 1,
};
const TARGETED_FIXED: &str = "pub fn last_index(values: &[u8]) -> Option<usize> {\n    (!values.is_empty()).then(|| values.len() - 1)\n}\n\n#[cfg(test)]\nmod tests {\n    use super::last_index;\n\n    #[test]\n    fn returns_last_valid_index() {\n        assert_eq!(last_index(&[4, 8, 15]), Some(2));\n        assert_eq!(last_index(&[]), None);\n    }\n}\n";
const TARGETED_SCRIPT: &[Action] = &[
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

const MULTI_OUTCOMES: &[ExpectedOutcome] = &[
    ExpectedOutcome { path: "src/lib.rs", contains: "pub mod slug" },
    ExpectedOutcome { path: "src/slug.rs", contains: "to_ascii_lowercase" },
];
const MULTI_FILE: Task = Task {
    id: "multi-file-feature",
    prompt: "Navigate the crate and add a public slug module, then use it from the display_name API.",
    fixture: "multi-file",
    expected_outcomes: MULTI_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 7,
    max_tool_calls: 6,
    max_verification_attempts: 1,
};
const MULTI_LIB: &str = "pub mod slug;\n\npub fn display_name(input: &str) -> String {\n    slug::format(input)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn display_name_uses_slug_format() {\n        assert_eq!(super::display_name(\"Hello World\"), \"hello-world\");\n    }\n}\n";
const MULTI_SLUG: &str = "pub fn format(input: &str) -> String {\n    input.trim().to_ascii_lowercase().replace(' ', \"-\")\n}\n";
const MULTI_FILE_SCRIPT: &[Action] = &[
    Action::List { path: "src" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/slug.rs", contents: MULTI_SLUG },
    Action::Write { path: "src/lib.rs", contents: MULTI_LIB },
    Action::Verify,
    Action::Finish,
];

const LONG_HORIZON_PREFIX: &[Action] = &[
    Action::List { path: "src" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/slug.rs", contents: MULTI_SLUG },
    Action::Write { path: "src/lib.rs", contents: MULTI_LIB },
];
const LONG_HORIZON_REMAINING: &[Action] = &[Action::Verify, Action::Finish];

const STALE_OBSERVATION: Task = Task {
    id: "stale-observation-recovery",
    prompt: "Repair the ambiguous indexing bug after the observed source changes externally, then verify the fresh implementation.",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 8,
    max_tool_calls: 7,
    max_verification_attempts: 1,
};
const STALE_EXTERNAL: &str =
    "pub fn last_index(values: &[u8]) -> Option<usize> {\n    values.first().map(|_| 0)\n}\n";
const STALE_PREFIX: &[Action] = &[Action::Read { path: "src/lib.rs" }];
const STALE_REMAINING: &[Action] = &[
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

const DIRTY_OUTCOMES: &[ExpectedOutcome] = &[
    ExpectedOutcome { path: "src/lib.rs", contains: "values.len() - 1" },
    ExpectedOutcome { path: "README.md", contains: "user-owned update" },
];
const DIRTY_WORKTREE: Task = Task {
    id: "dirty-worktree-preservation",
    prompt: "Fix the indexing bug while preserving unrelated user notes in this dirty worktree, then verify the code.",
    fixture: "dirty-worktree",
    expected_outcomes: DIRTY_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 6,
    max_tool_calls: 5,
    max_verification_attempts: 1,
};
const DIRTY_NOTE: &str = "# Local notes\n\nuser-owned update that must survive the coding task.\n";
const DIRTY_PREFIX: &[Action] = &[Action::List { path: "." }, Action::Read { path: "src/lib.rs" }];
const DIRTY_REMAINING: &[Action] = &[
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

const RECOVERY_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/lib.rs", contains: "parse::<u16>()" }];
const FAILED_FIRST: Task = Task {
    id: "failed-first-recovery",
    prompt: "Repair parse_port and recover if the first verification exposes an incorrect edit.",
    fixture: "failed-first",
    expected_outcomes: RECOVERY_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 7,
    max_tool_calls: 6,
    max_verification_attempts: 2,
};
const RECOVERY_WRONG: &str = "pub fn parse_port(value: &str) -> Option<u16> {\n    value.parse::<u8>().ok().map(u16::from)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn accepts_full_port_range() {\n        assert_eq!(super::parse_port(\"8080\"), Some(8080));\n    }\n}\n";
const RECOVERY_FIXED: &str = "pub fn parse_port(value: &str) -> Option<u16> {\n    value.parse::<u16>().ok()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn accepts_full_port_range() {\n        assert_eq!(super::parse_port(\"8080\"), Some(8080));\n    }\n}\n";
const FAILED_FIRST_SCRIPT: &[Action] = &[
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: RECOVERY_WRONG },
    Action::Verify,
    Action::Write { path: "src/lib.rs", contents: RECOVERY_FIXED },
    Action::Verify,
    Action::Finish,
];

const COMPILER_RECOVERY_PREFIX: &[Action] = &[
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: RECOVERY_WRONG },
    Action::Verify,
];
const COMPILER_RECOVERY_REMAINING: &[Action] = &[
    Action::Write { path: "src/lib.rs", contents: RECOVERY_FIXED },
    Action::Verify,
    Action::Finish,
];

const WORKSPACE_TEST: &[&str] = &["cargo", "test", "--workspace", "--quiet", "--offline"];
const WORKSPACE_OUTCOMES: &[ExpectedOutcome] = &[
    ExpectedOutcome { path: "crates/app/src/lib.rs", contains: "codec::normalize" },
    ExpectedOutcome { path: "crates/codec/src/lib.rs", contains: "replace(' ', \"-\")" },
];
const WORKSPACE_TASK: Task = Task {
    id: "workspace-aware-migration",
    prompt: "Migrate the workspace API across the app and codec crates, then verify the callers and tests.",
    fixture: "workspace-task",
    expected_outcomes: WORKSPACE_OUTCOMES,
    verification_command: WORKSPACE_TEST,
    focused_verification_command: None,
    max_steps: 8,
    max_tool_calls: 8,
    max_verification_attempts: 1,
};
const WORKSPACE_PREFIX: &[Action] = &[
    Action::List { path: "." },
    Action::Search { query: "normalize", path: "." },
    Action::Read { path: "crates/app/src/lib.rs" },
    Action::Read { path: "crates/codec/src/lib.rs" },
    Action::Write {
        path: "crates/codec/src/lib.rs",
        contents: "pub fn normalize(input: &str) -> String {\n    input.trim().to_ascii_lowercase().replace(' ', \"-\")\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn normalize_uses_hyphens() {\n        assert_eq!(super::normalize(\"Hello World\"), \"hello-world\");\n    }\n}\n",
    },
    Action::Write {
        path: "crates/app/src/lib.rs",
        contents: "pub fn display_name(input: &str) -> String {\n    codec::normalize(input)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn display_name_uses_codec_contract() {\n        assert_eq!(super::display_name(\"Hello World\"), \"hello-world\");\n    }\n}\n",
    },
];
const WORKSPACE_REMAINING: &[Action] = &[Action::Verify, Action::Finish];

const REDUNDANT_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/lib.rs", contains: "value.trim().is_empty()" }];
const REDUNDANT_READ: Task = Task {
    id: "redundant-read-avoidance",
    prompt: "Fix is_blank while avoiding repeated reads of unchanged files.",
    fixture: "redundant-read",
    expected_outcomes: REDUNDANT_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 6,
    max_tool_calls: 5,
    max_verification_attempts: 1,
};
const REDUNDANT_FIXED: &str = "pub fn is_blank(value: &str) -> bool {\n    value.trim().is_empty()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn whitespace_is_blank() {\n        assert!(super::is_blank(\"  \\n\"));\n    }\n}\n";
const REDUNDANT_SCRIPT: &[Action] = &[
    Action::Read { path: "src/lib.rs" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: REDUNDANT_FIXED },
    Action::Verify,
    Action::Finish,
];

const FOCUSED_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/slug.rs", contains: "trim_matches('-')" }];
const FOCUSED_VERIFY: Task = Task {
    id: "focused-verification",
    prompt: "Fix slug formatting and verify only the directly affected formats_slug test.",
    fixture: "focused-verification",
    expected_outcomes: FOCUSED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: Some(FOCUSED_TEST),
    max_steps: 5,
    max_tool_calls: 4,
    max_verification_attempts: 1,
};
const FOCUSED_FIXED: &str = "pub fn format_slug(value: &str) -> String {\n    value.trim().to_ascii_lowercase().replace(' ', \"-\").trim_matches('-').to_owned()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn formats_slug() {\n        assert_eq!(super::format_slug(\" Hello World \"), \"hello-world\");\n    }\n\n    #[test]\n    fn unrelated_behavior() {\n        assert_eq!(2 + 2, 4);\n    }\n}\n";
const FOCUSED_SCRIPT: &[Action] = &[
    Action::Read { path: "src/slug.rs" },
    Action::Write { path: "src/slug.rs", contents: FOCUSED_FIXED },
    Action::Verify,
    Action::Finish,
];

const INSUFFICIENT_EVIDENCE: Task = Task {
    id: "insufficient-evidence-recovery",
    prompt: "Repair last_index, even if the first edit arrives before the target was inspected.",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 6,
    max_tool_calls: 5,
    max_verification_attempts: 1,
};
const INSUFFICIENT_EVIDENCE_SCRIPT: &[Action] = &[
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

const CONTROL_LOOP_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "notes.txt", contains: "runtime-owned verification" }];
const CONTROL_LOOP_TASK: Task = Task {
    id: "runtime-verification-zero-waste",
    prompt: "Update the repository note and verify the exact workspace diff before finishing.",
    fixture: "control-loop",
    expected_outcomes: CONTROL_LOOP_OUTCOMES,
    verification_command: &["git", "diff", "--check"],
    focused_verification_command: None,
    max_steps: 8,
    max_tool_calls: 7,
    max_verification_attempts: 2,
};
const CONTROL_LOOP_FIXED: &str = "runtime-owned verification\n";
const CONTROL_LOOP_SCRIPT: &[Action] = &[
    Action::Read { path: "notes.txt" },
    Action::Write { path: "notes.txt", contents: CONTROL_LOOP_FIXED },
    Action::Verify,
    Action::Verify,
    Action::Finish,
];

const TARGETED_EXPLORATION: Task = Task {
    id: "targeted-exploration-reuse",
    prompt: "Fix is_blank after a model repeatedly lists the same source directory.",
    fixture: "redundant-read",
    expected_outcomes: REDUNDANT_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 7,
    max_tool_calls: 6,
    max_verification_attempts: 1,
};
const TARGETED_EXPLORATION_SCRIPT: &[Action] = &[
    Action::List { path: "src" },
    Action::List { path: "src" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: REDUNDANT_FIXED },
    Action::Verify,
    Action::Finish,
];

const SHELL_CARGO_METADATA_ARGS: &[&str] =
    &["metadata", "--format-version", "1", "--no-deps", "--offline"];
const SHELL_CARGO_LOCATE_PROJECT_ARGS: &[&str] = &["locate-project", "--message-format", "plain"];
const SHELL_CARGO_TREE_ARGS: &[&str] = &["tree", "--offline"];
const SHELL_HEAVY: Task = Task {
    id: "shell-heavy-command-reuse",
    prompt: "Repair last_index after an inefficient shell-heavy discovery loop.",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 10,
    max_tool_calls: 9,
    max_verification_attempts: 1,
};
const SHELL_HEAVY_SCRIPT: &[Action] = &[
    Action::Shell { program: "cargo", args: SHELL_CARGO_METADATA_ARGS },
    Action::Shell { program: "cargo", args: SHELL_CARGO_METADATA_ARGS },
    Action::Shell { program: "cargo", args: SHELL_CARGO_LOCATE_PROJECT_ARGS },
    Action::Shell { program: "cargo", args: SHELL_CARGO_LOCATE_PROJECT_ARGS },
    Action::Shell { program: "cargo", args: SHELL_CARGO_TREE_ARGS },
    Action::Shell { program: "cargo", args: SHELL_CARGO_TREE_ARGS },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

const RELATIONSHIP_HEAVY_OUTCOMES: &[ExpectedOutcome] = &[
    ExpectedOutcome { path: "src/lib.rs", contains: "pub mod slug" },
    ExpectedOutcome { path: "src/slug.rs", contains: "to_ascii_lowercase" },
];
const RELATIONSHIP_HEAVY_EXPLORATION: Task = Task {
    id: "relationship-heavy-exploration",
    prompt: "Navigate the crate through exact definitions and related imports before adding the slug module.",
    fixture: "multi-file",
    expected_outcomes: RELATIONSHIP_HEAVY_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 10,
    max_tool_calls: 9,
    max_verification_attempts: 1,
};
const RELATIONSHIP_HEAVY_EXPLORATION_SCRIPT: &[Action] = &[
    Action::List { path: "src" },
    Action::Search { query: "display_name", path: "src" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/slug.rs", contents: MULTI_SLUG },
    Action::Search { query: "slug", path: "src" },
    Action::Read { path: "src/slug.rs" },
    Action::Write { path: "src/lib.rs", contents: MULTI_LIB },
    Action::Verify,
    Action::Finish,
];

#[cfg(test)]
const PREMATURE_COMPLETION: Task = Task {
    id: "premature-completion-blocked",
    prompt: "repair the target and prove the change before finishing",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 3,
    max_tool_calls: 1,
    max_verification_attempts: 1,
};
#[cfg(test)]
const PREMATURE_COMPLETION_SCRIPT: &[Action] = &[Action::Finish];

#[cfg(test)]
mod tests {
    use aether_agent::{AutonomousCodingPolicy, LoopGuardrails, RepositoryReadTarget};
    use aether_core::{
        InspectedFile, LineRange, ObservedFileState, ToolCallId, ToolResult, WorkflowState,
    };

    use super::*;

    #[test]
    fn completion_recovery_scope_and_ownership_guards_are_local() {
        let premature_result =
            real::run_task(&PREMATURE_COMPLETION, PREMATURE_COMPLETION_SCRIPT, true)
                .expect("premature completion eval runs");
        assert!(!premature_result.success);
        assert_eq!(premature_result.final_test_status, "not_run");
        assert!(premature_result.completion_rejections > 0);

        let mut stale = ContextSnapshot::new("/workspace", None);
        stale.workspace_changed = true;
        let stale_plan = RepositoryActionPlanner::default().plan_read_targets(
            &stale,
            &[RepositoryReadTarget {
                path: "src/lib.rs".to_owned(),
                ranges: vec![LineRange { start: 1, end: 4 }],
            }],
        );
        assert!(stale_plan.aborted);

        let mut dirty = ContextSnapshot::new("/workspace", None);
        dirty.workflow.record_relevant_file("src/lib.rs");
        dirty.inspected.push(InspectedFile {
            path: "src/lib.rs".to_owned(),
            content_hash: Some("current".to_owned()),
            ranges: vec![LineRange { start: 1, end: 4 }],
            last_state: ObservedFileState::Present,
            stale: false,
        });
        dirty.user_modified.push("src/lib.rs".to_owned());
        let dirty_result = AutonomousCodingPolicy::new().preflight(
            &dirty,
            ToolCallId::new("dirty").unwrap(),
            "write",
            &serde_json::json!({"path": "src/lib.rs"}),
        );
        assert_eq!(dirty_result.unwrap().error.unwrap().code.as_str(), "insufficient_evidence");

        let mut guard = LoopGuardrails::new();
        let workflow = WorkflowState::new();
        let input = serde_json::json!({"files": [{"path": "src/lib.rs"}]});
        for index in 0..3 {
            let failure = ToolResult::failure(
                ToolCallId::new(format!("eval-failure-{index}")).unwrap(),
                "io",
                "same failure",
                true,
                1024,
            );
            guard.observe("read", &input, &failure, &workflow, &workflow);
        }
        let repeated = guard
            .preflight(ToolCallId::new("eval-circuit").unwrap(), "read", &input, 0)
            .expect("repeated no-progress action is bounded");
        assert_eq!(repeated.error.unwrap().code.as_str(), "repeated_failure");
    }

    #[test]
    fn capability_cases_exercise_runtime_recovery_and_bounded_reuse() {
        let suite = run_capability_suite(None).expect("capability suite runs offline");
        assert!(suite.success);
        assert_eq!(suite.aggregate.succeeded, suite.aggregate.tasks);
        assert_eq!(suite.aggregate.checkpoint_resume_successes, suite.aggregate.tasks);
        assert!(suite.aggregate.recovery_successes >= 2);
        assert!(suite.aggregate.user_change_tests > 0);
        assert_eq!(suite.aggregate.user_change_corruptions, 0);
        assert!(suite.aggregate.automatic_evidence_reactions > 0);
        assert!(suite.aggregate.mutation_refresh_events > 0);
        assert_eq!(suite.aggregate.false_completion_count, 0);
        assert_eq!(suite.control_loop.passed, suite.control_loop.total);
        assert!(suite.control_loop.cases[0].automatic_verification_attempts > 0);
        assert!(
            suite.control_loop.cases[0].baseline_executed_tool_calls
                > suite.control_loop.cases[0].optimized_executed_tool_calls
        );
    }
}
