//! Real-runtime evaluation adapter.
//!
//! The evaluator replays model events through the production Agent, registry, policy, scheduler,
//! context engine, and filesystem tools. This module owns only fixture setup and result shaping.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_agent::{
    Agent, AgentMetrics, AgentRequest, AgentRunResult, AllowAllPermissionBroker, CancellationToken,
    FakeBackend, FakeStep, FakeToolCall,
};
use aether_core::{AgentEvent, ContextSnapshot, SessionId, TurnId, WorkflowVerification};
use aether_tools::ToolRegistry;

use super::{
    Action, CapabilityTaskResult, ControlLoopCaseResult, ControlLoopEvaluation, ExecutionMetrics,
    Task, TaskResult, TrajectoryMetrics, trajectory,
};

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn run_task(
    task: &Task,
    script: &[Action],
    optimize: bool,
) -> Result<TaskResult, String> {
    run_task_internal(task, script, optimize, false)
}

/// Run a task in a temporary Git-backed fixture. Repository-aware verification planning is
/// intentionally exercised only by callers that request it; the historical regression fixtures
/// remain byte-for-byte comparable to their non-Git baseline.
pub(crate) fn run_task_with_git(
    task: &Task,
    script: &[Action],
    optimize: bool,
) -> Result<TaskResult, String> {
    run_task_internal(task, script, optimize, true)
}

fn run_task_internal(
    task: &Task,
    script: &[Action],
    optimize: bool,
    initialize_git: bool,
) -> Result<TaskResult, String> {
    let copy_started = Instant::now();
    let root = copy_fixture(task.fixture)?;
    if initialize_git && let Err(error) = initialize_git_repository(&root) {
        let _ = fs::remove_dir_all(&root);
        return Err(error);
    }
    let harness_overhead_ms = copy_started.elapsed().as_millis();
    let retained = trajectory::retained_indices(script, optimize);
    let existing_write_targets = script
        .iter()
        .filter_map(|action| match action {
            Action::Write { path, .. } if root.join(path).is_file() => Some((*path).to_owned()),
            _ => None,
        })
        .collect();
    let trajectory = TrajectoryMetrics::analyze(script, &retained, &existing_write_targets);
    let steps = build_steps(task, &root, script, &retained, optimize)?;
    let backend = FakeBackend::new("deterministic trace replay").with_steps(steps);
    let registry = ToolRegistry::new(&root)?;
    let agent = Agent::new(backend, registry);
    let session = SessionId::new(format!("eval-{}", task.id)).map_err(|error| error.to_string())?;
    let turn = TurnId::new(format!("eval-{}-{}", task.id, u64::from(optimize)))
        .map_err(|error| error.to_string())?;
    let mut request = AgentRequest::new(session, turn, task.prompt);
    request.max_steps = task.max_steps as u16;
    request.workspace_root = Some(root.display().to_string());
    request.metrics = true;
    request.optimize = optimize;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build eval runtime: {error}"))?;
    let run_started = Instant::now();
    let (result, event_summary) = runtime.block_on(async {
        let result = agent
            .run_with_broker(request, sender, CancellationToken::new(), &AllowAllPermissionBroker)
            .await;
        let mut summary = Vec::new();
        while let Some(event) = receiver.recv().await {
            match event {
                AgentEvent::ToolFinished { ok, .. } => summary.push(format!("tool:{ok}")),
                // Do not copy tool output into an eval failure record: outputs may contain
                // repository secrets even though production context/session boundaries omit them.
                AgentEvent::ToolOutput { .. } => summary.push("tool_output".to_owned()),
                AgentEvent::Warning { .. } => summary.push("warning".to_owned()),
                AgentEvent::Error { message } => {
                    summary.push(format!("error:{}", message.as_str()))
                }
                _ => {}
            }
        }
        (result, summary)
    });
    let run_wall_time_ms = run_started.elapsed().as_millis();
    let (agent_result, metrics, agent_error) = match result {
        Ok(result) => {
            let metrics = result.metrics.clone().unwrap_or_default();
            (Some(result), metrics, None)
        }
        Err(error) => {
            // AgentRunResult intentionally exists only for completed turns, but the bounded
            // workflow state remains available after a typed failure. Preserve its deterministic
            // review/recovery counters so an eval can prove why the turn stopped without
            // retaining transient tool output or raw model text.
            let mut metrics = AgentMetrics::default();
            record_workflow_metrics(&mut metrics, &agent.context_snapshot());
            (None, metrics, Some(error.to_string()))
        }
    };
    let outcomes_ok = task.expected_outcomes.iter().all(|expected| {
        fs::read_to_string(root.join(expected.path))
            .is_ok_and(|contents| contents.contains(expected.contains))
    });
    let verification = agent_result
        .as_ref()
        .map(|result| result.context.workflow.verification)
        .unwrap_or(WorkflowVerification::NotRequired);
    let final_test_status = match verification {
        WorkflowVerification::Passed => "passed",
        WorkflowVerification::Failed => "failed",
        WorkflowVerification::Pending | WorkflowVerification::NotRequired => "not_run",
    };
    let verification_scope = if metrics.verification_attempts == 0 {
        "none".to_owned()
    } else if optimize && task.focused_verification_command.is_some() {
        "focused".to_owned()
    } else {
        "broad".to_owned()
    };
    let verification_quality = match (final_test_status, verification_scope.as_str()) {
        ("passed", "focused") => "focused_pass",
        ("passed", _) => "broad_pass",
        ("failed", _) => "failed",
        _ => "none",
    }
    .to_owned();
    let success = agent_result.is_some() && outcomes_ok && final_test_status == "passed";
    let failure = if success {
        None
    } else if agent_result.is_none() {
        Some(format!(
            "agent turn failed: {}; events={}",
            agent_error.unwrap_or_else(|| "unknown error".to_owned()),
            event_summary.join(" | ")
        ))
    } else if !outcomes_ok {
        Some("expected file outcome was not present".to_owned())
    } else {
        Some("final verification did not pass".to_owned())
    };
    let result = TaskResult::from_metrics(
        task,
        success,
        &metrics,
        verification_scope,
        verification_quality,
        final_test_status.to_owned(),
        failure,
        run_wall_time_ms,
        harness_overhead_ms,
        trajectory,
    );
    let _ = fs::remove_dir_all(&root);
    Ok(result)
}

fn initialize_git_repository(root: &Path) -> Result<(), String> {
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .map_err(|error| format!("initialize control-loop fixture repository: {error}"))?;
    if !init.success() {
        return Err("initialize control-loop fixture repository: git init failed".to_owned());
    }
    let add = Command::new("git")
        .args(["add", "--all"])
        .current_dir(root)
        .status()
        .map_err(|error| format!("index control-loop fixture repository: {error}"))?;
    if !add.success() {
        return Err("index control-loop fixture repository: git add failed".to_owned());
    }
    let commit = Command::new("git")
        .args([
            "-c",
            "user.name=AETHER Eval",
            "-c",
            "user.email=aether-eval@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--quiet",
            "--no-verify",
            "-m",
            "fixture baseline",
        ])
        .current_dir(root)
        .status()
        .map_err(|error| format!("commit control-loop fixture repository: {error}"))?;
    if !commit.success() {
        return Err("commit control-loop fixture repository: git commit failed".to_owned());
    }
    Ok(())
}

/// Evaluate deterministic runtime-owned verification and completion controls. The positive
/// fixture is deliberately not a language package: the planner's bounded `git diff --check`
/// fallback makes that case fast, reproducible, and independent of a dependency cache.
pub(crate) fn run_control_loop_evaluation() -> Result<ControlLoopEvaluation, String> {
    let baseline = run_task_with_git(&super::CONTROL_LOOP_TASK, super::CONTROL_LOOP_SCRIPT, false)?;
    let optimized = run_task_with_git(&super::CONTROL_LOOP_TASK, super::CONTROL_LOOP_SCRIPT, true)?;
    let correctness_preserved = baseline.success && optimized.success;
    let automatic_verification = optimized.automatic_verification_attempts > 0;
    let bounded_fresh_reexecution =
        optimized.executed_tool_calls <= baseline.executed_tool_calls.saturating_add(1);
    let positive_passed =
        correctness_preserved && automatic_verification && bounded_fresh_reexecution;
    let positive_failure = (!positive_passed).then(|| {
        format!(
            "verification control failed: baseline_success={} optimized_success={} baseline_tools={} optimized_tools={} fresh_call_limit={} automatic_verifications={} prevented={} baseline_failure={} optimized_failure={}",
            baseline.success,
            optimized.success,
            baseline.executed_tool_calls,
            optimized.executed_tool_calls,
            baseline.executed_tool_calls.saturating_add(1),
            optimized.automatic_verification_attempts,
            optimized.prevented_redundant_calls,
            baseline.failure.as_deref().unwrap_or("none"),
            optimized.failure.as_deref().unwrap_or("none"),
        )
    });
    let positive_case = ControlLoopCaseResult {
        case_id: super::CONTROL_LOOP_TASK.id.to_owned(),
        expected_completion_rejection: false,
        passed: positive_passed,
        baseline_success: baseline.success,
        optimized_success: optimized.success,
        correctness_preserved,
        automatic_verification_attempts: optimized.automatic_verification_attempts,
        prevented_redundant_calls: optimized.prevented_redundant_calls,
        baseline_model_requests: baseline.model_requests,
        optimized_model_requests: optimized.model_requests,
        baseline_requested_tool_calls: baseline.requested_tool_calls,
        optimized_requested_tool_calls: optimized.requested_tool_calls,
        baseline_executed_tool_calls: baseline.executed_tool_calls,
        optimized_executed_tool_calls: optimized.executed_tool_calls,
        verification_scope: optimized.verification_scope,
        completion_rejections: optimized.completion_rejections,
        failure: positive_failure,
    };
    let premature_baseline =
        run_task(&super::PREMATURE_COMPLETION, super::PREMATURE_COMPLETION_SCRIPT, false)?;
    let premature_optimized =
        run_task(&super::PREMATURE_COMPLETION, super::PREMATURE_COMPLETION_SCRIPT, true)?;
    let completion_rejected = premature_baseline.completion_rejections > 0
        && premature_optimized.completion_rejections > 0;
    let no_verification_success = premature_baseline.verification_attempts == 0
        && premature_optimized.verification_attempts == 0
        && premature_baseline.final_test_status == "not_run"
        && premature_optimized.final_test_status == "not_run";
    let no_mutation_success = !super::PREMATURE_COMPLETION_SCRIPT
        .iter()
        .any(|action| matches!(action, Action::Write { .. }));
    let completion_rejected_without_work = !premature_baseline.success
        && !premature_optimized.success
        && completion_rejected
        && no_verification_success
        && no_mutation_success;
    let negative_failure = (!completion_rejected_without_work).then(|| {
        format!(
            "premature completion control failed: baseline_success={} optimized_success={} baseline_verification={} optimized_verification={} baseline_status={} optimized_status={} baseline_rejections={} optimized_rejections={} mutation_action_present={}",
            premature_baseline.success,
            premature_optimized.success,
            premature_baseline.verification_attempts,
            premature_optimized.verification_attempts,
            premature_baseline.final_test_status,
            premature_optimized.final_test_status,
            premature_baseline.completion_rejections,
            premature_optimized.completion_rejections,
            !no_mutation_success,
        )
    });
    let negative_case = ControlLoopCaseResult {
        case_id: super::PREMATURE_COMPLETION.id.to_owned(),
        expected_completion_rejection: true,
        passed: completion_rejected_without_work,
        baseline_success: premature_baseline.success,
        optimized_success: premature_optimized.success,
        correctness_preserved: !premature_baseline.success && !premature_optimized.success,
        automatic_verification_attempts: premature_optimized.after.automatic_verification_attempts,
        prevented_redundant_calls: premature_optimized.prevented_redundant_calls,
        baseline_model_requests: premature_baseline.model_requests,
        optimized_model_requests: premature_optimized.model_requests,
        baseline_requested_tool_calls: premature_baseline.requested_tool_calls,
        optimized_requested_tool_calls: premature_optimized.requested_tool_calls,
        baseline_executed_tool_calls: premature_baseline.executed_tool_calls,
        optimized_executed_tool_calls: premature_optimized.executed_tool_calls,
        verification_scope: premature_optimized.verification_scope,
        completion_rejections: premature_optimized.completion_rejections,
        failure: negative_failure,
    };
    let cases = vec![positive_case, negative_case];
    let passed = cases.iter().filter(|case| case.passed).count();
    Ok(ControlLoopEvaluation { total: cases.len(), passed, cases })
}

/// Run a checkpointed task while applying one bounded external change between segments.
/// `preserved_path` is used for dirty-tree cases where the changed path must survive untouched.
pub(crate) fn run_resumable_task_with_change(
    task: &Task,
    first_script: &[Action],
    remaining_script: &[Action],
    category: &str,
    external_change: Option<(&str, &str)>,
    preserved_path: Option<&str>,
) -> Result<CapabilityTaskResult, String> {
    let baseline = run_task(task, first_script, true)?;
    let root = copy_fixture(task.fixture)?;
    let first = run_segment(task, &root, first_script, first_script.len() as u16, None)?;
    let checkpoint_captured = first.result.is_none();
    let persisted = first.context.persistable();
    let encoded = serde_json::to_vec(&persisted).map_err(|error| error.to_string())?;
    let seed: ContextSnapshot =
        serde_json::from_slice(&encoded).map_err(|error| error.to_string())?;
    let work_state_survived = !seed.workflow.work.objective.as_str().is_empty()
        && (seed.workflow.work.is_structured() || !seed.workflow.work.mutations.is_empty());
    if let Some((path, contents)) = external_change {
        fs::write(root.join(path), contents)
            .map_err(|error| format!("apply external capability change {path}: {error}"))?;
    }
    let second =
        run_segment(task, &root, remaining_script, remaining_script.len() as u16, Some(seed))?;
    let outcomes_ok = task.expected_outcomes.iter().all(|expected| {
        fs::read_to_string(root.join(expected.path))
            .is_ok_and(|contents| contents.contains(expected.contains))
    });
    let resumed_success = second.result.is_some()
        && outcomes_ok
        && second.result.as_ref().is_some_and(|result| {
            result.context.workflow.verification == WorkflowVerification::Passed
        });
    let metrics = second.metrics.unwrap_or_default();
    let user_change_tested = preserved_path.is_some();
    let user_change_corruption = preserved_path.is_some_and(|path| {
        fs::read_to_string(root.join(path)).map_or(true, |contents| {
            external_change.is_some_and(|(changed_path, expected)| {
                changed_path == path && contents != expected
            })
        })
    });
    let recovery_observed = metrics.recovery_successes > 0
        || second.context.workflow.work.blockers.is_empty()
            && !first.context.workflow.work.blockers.is_empty();
    let failure = if resumed_success {
        None
    } else {
        Some(format!(
            "checkpoint task failed: first={} second={} outcomes_ok={outcomes_ok}",
            first.error.as_deref().unwrap_or("completed"),
            second.error.as_deref().unwrap_or("completed"),
        ))
    };
    let result = CapabilityTaskResult {
        task_id: task.id.to_owned(),
        category: category.to_owned(),
        baseline_success: baseline.success,
        resumed_success,
        success_at_1: resumed_success,
        trials: 1,
        previously_unsolved: !baseline.success && resumed_success,
        multi_file_correctness: task.expected_outcomes.len() > 1 && outcomes_ok,
        checkpoint_captured,
        work_state_survived,
        recovery_observed,
        user_change_tested,
        user_change_corruption,
        stale_evidence_violations: metrics.stale_evidence_violations as u32,
        model_requests: metrics.model_requests as u32,
        tool_executions: metrics.executed_tool_calls as u32,
        process_spawns: metrics.process_spawns as u32,
        context_bytes: metrics.context_bytes,
        bytes_shown_to_model: metrics.bytes_shown_to_model,
        wall_time_ms: metrics.wall_time_ms,
        verification_attempts: metrics.verification_attempts as u32,
        automatic_verification_attempts: metrics.automatic_verification_attempts as u32,
        verification_failures: metrics.verification_failures as u32,
        reused_observations: metrics.reused_observations as u32,
        automatic_evidence_reactions: metrics.automatic_evidence_reactions as u32,
        diagnostic_hydration_events: metrics.diagnostic_hydration_events as u32,
        mutation_refresh_events: metrics.mutation_refresh_events as u32,
        stale_evidence_invalidations: metrics.stale_evidence_invalidations as u32,
        verification_escalations: metrics.verification_escalations as u32,
        completion_rejections: metrics.completion_rejections as u32,
        unresolved_work_at_finish: metrics.unresolved_work_at_finish as u32,
        failure,
    };
    let _ = fs::remove_dir_all(&root);
    Ok(result)
}

struct SegmentResult {
    result: Option<AgentRunResult>,
    context: ContextSnapshot,
    metrics: Option<AgentMetrics>,
    error: Option<String>,
}

fn run_segment(
    task: &Task,
    root: &Path,
    script: &[Action],
    max_steps: u16,
    context_seed: Option<ContextSnapshot>,
) -> Result<SegmentResult, String> {
    let retained = (0..script.len()).collect::<Vec<_>>();
    let steps = build_steps(task, root, script, &retained, true)?;
    let backend = FakeBackend::new("capability trace replay").with_steps(steps);
    let registry = ToolRegistry::new(root)?;
    let agent = Agent::new(backend, registry);
    let session =
        SessionId::new(format!("capability-{}", task.id)).map_err(|error| error.to_string())?;
    let turn = TurnId::new(format!("capability-{}-{}", task.id, max_steps))
        .map_err(|error| error.to_string())?;
    let mut request = AgentRequest::new(session, turn, task.prompt);
    request.max_steps = max_steps.max(1);
    request.workspace_root = Some(root.display().to_string());
    request.context_seed = context_seed;
    request.metrics = true;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(256);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("build capability runtime: {error}"))?;
    let result = runtime.block_on(async {
        let result = agent
            .run_with_broker(request, sender, CancellationToken::new(), &AllowAllPermissionBroker)
            .await;
        while receiver.recv().await.is_some() {}
        result
    });
    let failed_context = agent.context_snapshot();
    let (result, metrics, error) = match result {
        Ok(result) => {
            let metrics = result.metrics.clone();
            (Some(result), metrics, None)
        }
        Err(error) => {
            let mut metrics = AgentMetrics::default();
            record_workflow_metrics(&mut metrics, &failed_context);
            (None, Some(metrics), Some(error.to_string()))
        }
    };
    let context = result.as_ref().map(|result| result.context.clone()).unwrap_or(failed_context);
    Ok(SegmentResult { result, context, metrics, error })
}

fn build_steps(
    task: &Task,
    root: &Path,
    script: &[Action],
    retained: &[usize],
    optimize: bool,
) -> Result<Vec<FakeStep>, String> {
    let mut hashes = HashMap::new();
    let mut inspected = std::collections::HashSet::new();
    let mut steps = Vec::with_capacity(script.len());
    for &index in retained {
        let action = script
            .get(index)
            .ok_or_else(|| "trajectory retained an invalid action index".to_owned())?;
        let tool_call = action_to_call(task, root, action, optimize, &hashes)?;
        if let Action::Write { path, contents } = action {
            let existing = root.join(path).exists();
            if !existing || inspected.contains(*path) {
                hashes.insert(
                    (*path).to_owned(),
                    blake3::hash(contents.as_bytes()).to_hex().to_string(),
                );
            }
        }
        if let Action::Read { path } = action {
            inspected.insert((*path).to_owned());
        }
        if let Action::Write { path, .. } = action {
            // A successful write returns the new content hash, so a following read of that same
            // path is redundant even when the model did not explicitly inspect it beforehand.
            inspected.insert((*path).to_owned());
        }
        steps.push(match tool_call {
            Some(tool_call) => FakeStep { text: None, tool_calls: vec![tool_call] },
            None => FakeStep { text: Some("completed".to_owned()), tool_calls: Vec::new() },
        });
    }
    Ok(steps)
}

fn record_workflow_metrics(metrics: &mut AgentMetrics, context: &ContextSnapshot) {
    let progress = &context.workflow.progress;
    metrics.automatic_evidence_reactions = u64::from(progress.automatic_evidence_reactions);
    metrics.diagnostic_hydration_events = u64::from(progress.diagnostic_hydrations);
    metrics.mutation_refresh_events = u64::from(progress.mutation_refreshes);
    metrics.stale_evidence_invalidations = u64::from(progress.stale_invalidations);
    metrics.verification_escalations = u64::from(progress.verification_escalations);
    metrics.completion_rejections = u64::from(progress.completion_rejections);
    metrics.unresolved_work_at_finish = context.workflow.work.unresolved_count() as u64;
    let scope = context.workflow.decision.verification_scope.as_str();
    if !scope.is_empty() {
        metrics.verification_scope = Some(scope.to_owned());
    }
}

fn action_to_call(
    task: &Task,
    root: &Path,
    action: &Action,
    optimize: bool,
    hashes: &HashMap<String, String>,
) -> Result<Option<FakeToolCall>, String> {
    let call = match action {
        Action::List { path } => ("list", serde_json::json!({"path": path})),
        Action::Read { path } => ("read", serde_json::json!({"files": [{"path": path}]})),
        Action::Search { query, path } => {
            ("search", serde_json::json!({"patterns": [query], "path": path}))
        }
        Action::Write { path, contents } => {
            let absolute = root.join(path);
            let expected_hash = hashes.get(*path).cloned().or_else(|| {
                fs::read(&absolute).ok().map(|bytes| blake3::hash(&bytes).to_hex().to_string())
            });
            let create_only = expected_hash.is_none();
            (
                "write",
                serde_json::json!({
                    "path": path,
                    "content": contents,
                    "expected_hash": expected_hash,
                    "create_only": create_only
                }),
            )
        }
        Action::Shell { program, args } => (
            "shell",
            serde_json::json!({
                "program": program,
                "args": args,
                "cwd": ".",
                "timeout_ms": 120_000
            }),
        ),
        Action::Verify => {
            let command = if optimize {
                task.focused_verification_command.unwrap_or(task.verification_command)
            } else {
                task.verification_command
            };
            let (program, args) =
                command.split_first().ok_or_else(|| "verification command is empty".to_owned())?;
            (
                "shell",
                serde_json::json!({"program": program, "args": args, "cwd": ".", "timeout_ms": 120_000}),
            )
        }
        Action::Finish => return Ok(None),
    };
    Ok(Some(FakeToolCall { name: call.0.to_owned(), arguments: call.1 }))
}

fn copy_fixture(fixture: &str) -> Result<PathBuf, String> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures").join(fixture);
    let sequence = NEXT_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("aether-eval-{}-{sequence}", std::process::id()));
    copy_tree(&source, &root).map_err(|error| format!("copy fixture {fixture}: {error}"))?;
    Ok(root)
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

impl TaskResult {
    #[allow(clippy::too_many_arguments)]
    fn from_metrics(
        task: &Task,
        success: bool,
        metrics: &AgentMetrics,
        verification_scope: String,
        verification_quality: String,
        final_test_status: String,
        failure: Option<String>,
        wall_time_ms: u128,
        harness_overhead_ms: u128,
        trajectory: TrajectoryMetrics,
    ) -> Self {
        let after = ExecutionMetrics::from_agent(
            metrics,
            success,
            &verification_scope,
            &verification_quality,
            trajectory,
        );
        Self {
            task_id: task.id.to_owned(),
            prompt: task.prompt.to_owned(),
            expected_outcomes: task
                .expected_outcomes
                .iter()
                .map(|item| format!("{} contains {:?}", item.path, item.contains))
                .collect(),
            verification_command: task
                .verification_command
                .iter()
                .map(ToString::to_string)
                .collect(),
            max_steps: task.max_steps,
            max_tool_calls: task.max_tool_calls,
            max_verification_attempts: task.max_verification_attempts,
            baseline_success: false,
            before: ExecutionMetrics::default(),
            after: after.clone(),
            success,
            wall_time_ms,
            harness_overhead_ms,
            model_steps: after.model_steps,
            model_requests: after.model_requests,
            requested_tool_calls: after.requested_tool_calls,
            executed_tool_calls: after.executed_tool_calls,
            prevented_redundant_calls: after.prevented_redundant_calls,
            input_tokens: after.input_tokens,
            output_tokens: after.output_tokens,
            total_tokens: after.total_tokens,
            bytes_read: after.bytes_read,
            context_bytes: after.context_bytes,
            bytes_shown_to_model: after.bytes_shown_to_model,
            static_prefix_bytes: after.static_prefix_bytes,
            tool_schema_bytes: after.tool_schema_bytes,
            context_payload_bytes: after.context_payload_bytes,
            tool_result_bytes: after.tool_result_bytes,
            policy_feedback_bytes: after.policy_feedback_bytes,
            protocol_envelope_bytes: after.protocol_envelope_bytes,
            verification_attempts: after.verification_attempts,
            automatic_verification_attempts: after.automatic_verification_attempts,
            verification_failures: after.verification_failures,
            recovery_attempts: after.recovery_attempts,
            recovery_successes: after.recovery_successes,
            stale_evidence_violations: after.stale_evidence_violations,
            reused_observations: after.reused_observations,
            automatic_evidence_reactions: after.automatic_evidence_reactions,
            diagnostic_hydration_events: after.diagnostic_hydration_events,
            mutation_refresh_events: after.mutation_refresh_events,
            stale_evidence_invalidations: after.stale_evidence_invalidations,
            verification_escalations: after.verification_escalations,
            completion_rejections: after.completion_rejections,
            unresolved_work_at_finish: after.unresolved_work_at_finish,
            process_spawns: after.process_spawns,
            agent_wall_time_ms: after.wall_time_ms,
            provider_wait_ms: after.provider_wait_ms,
            local_execution_ms: after.local_execution_ms,
            cpu_time_ms: after.cpu_time_ms,
            peak_rss_bytes: after.peak_rss_bytes,
            allocation_count: after.allocation_count,
            verification_scope,
            verification_quality,
            final_test_status,
            failure,
            trajectory: after.trajectory.clone(),
        }
    }
}

impl ExecutionMetrics {
    fn from_agent(
        metrics: &AgentMetrics,
        success: bool,
        scope: &str,
        quality: &str,
        trajectory: TrajectoryMetrics,
    ) -> Self {
        Self {
            success,
            model_steps: metrics.model_steps as u32,
            model_requests: metrics.model_requests as u32,
            requested_tool_calls: metrics.requested_tool_calls as u32,
            executed_tool_calls: metrics.executed_tool_calls as u32,
            prevented_redundant_calls: metrics.prevented_calls as u32,
            input_tokens: metrics.input_tokens,
            output_tokens: metrics.output_tokens,
            total_tokens: metrics.total_tokens,
            bytes_read: metrics.bytes_read,
            context_bytes: metrics.context_bytes,
            bytes_shown_to_model: metrics.bytes_shown_to_model,
            static_prefix_bytes: metrics.static_prefix_bytes,
            tool_schema_bytes: metrics.tool_schema_bytes,
            context_payload_bytes: metrics.context_payload_bytes,
            tool_result_bytes: metrics.tool_result_bytes,
            policy_feedback_bytes: metrics.policy_feedback_bytes,
            protocol_envelope_bytes: metrics.protocol_envelope_bytes,
            verification_attempts: metrics.verification_attempts as u32,
            automatic_verification_attempts: metrics.automatic_verification_attempts as u32,
            verification_failures: metrics.verification_failures as u32,
            recovery_attempts: metrics.recovery_attempts as u32,
            recovery_successes: metrics.recovery_successes as u32,
            stale_evidence_violations: metrics.stale_evidence_violations as u32,
            reused_observations: metrics.reused_observations as u32,
            automatic_evidence_reactions: metrics.automatic_evidence_reactions as u32,
            diagnostic_hydration_events: metrics.diagnostic_hydration_events as u32,
            mutation_refresh_events: metrics.mutation_refresh_events as u32,
            stale_evidence_invalidations: metrics.stale_evidence_invalidations as u32,
            verification_escalations: metrics.verification_escalations as u32,
            completion_rejections: metrics.completion_rejections as u32,
            unresolved_work_at_finish: metrics.unresolved_work_at_finish as u32,
            process_spawns: metrics.process_spawns as u32,
            wall_time_ms: metrics.wall_time_ms,
            provider_wait_ms: metrics.provider_wait_ms,
            local_execution_ms: metrics.local_execution_ms,
            cpu_time_ms: metrics.cpu_time_ms,
            peak_rss_bytes: metrics.peak_rss_bytes,
            allocation_count: metrics.allocation_count,
            verification_scope: scope.to_owned(),
            verification_quality: quality.to_owned(),
            planner_attempts: metrics.planner_attempts as u32,
            planner_hits: metrics.planner_hits as u32,
            planner_aborted_ambiguity: metrics.planner_aborted_ambiguity as u32,
            planner_actions: metrics.planner_actions as u32,
            planner_bytes_read: metrics.planner_bytes_read,
            trajectory,
        }
    }
}
