//! Real-runtime evaluation adapter.
//!
//! The evaluator replays model events through the production Agent, registry, policy, scheduler,
//! context engine, and filesystem tools. This module owns only fixture setup and result shaping.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aether_agent::{
    Agent, AgentMetrics, AgentRequest, AllowAllPermissionBroker, CancellationToken, FakeBackend,
    FakeStep, FakeToolCall,
};
use aether_core::{AgentEvent, SessionId, TurnId, WorkflowVerification};
use aether_tools::ToolRegistry;

use super::{Action, ExecutionMetrics, Task, TaskResult};

static NEXT_WORKSPACE_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn run_task(
    task: &Task,
    script: &[Action],
    optimize: bool,
) -> Result<TaskResult, String> {
    let copy_started = Instant::now();
    let root = copy_fixture(task.fixture)?;
    let harness_overhead_ms = copy_started.elapsed().as_millis();
    let steps = build_steps(task, &root, script, optimize)?;
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
        Err(error) => (None, AgentMetrics::default(), Some(error.to_string())),
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
    );
    let _ = fs::remove_dir_all(&root);
    Ok(result)
}

fn build_steps(
    task: &Task,
    root: &Path,
    script: &[Action],
    optimize: bool,
) -> Result<Vec<FakeStep>, String> {
    let mut hashes = HashMap::new();
    let mut inspected = std::collections::HashSet::new();
    let mut steps = Vec::with_capacity(script.len());
    let mut planner_supplied_read = false;
    for action in script {
        // The optimized trace models the provider consuming the planner's exact read evidence
        // instead of issuing the immediately redundant read round-trip. The baseline keeps the
        // original trace so the comparison measures the production path end to end.
        if optimize
            && planner_supplied_read
            && matches!(action, Action::Read { path } if inspected.contains(*path))
        {
            planner_supplied_read = false;
            continue;
        }
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
        planner_supplied_read = matches!(action, Action::Search { .. });
    }
    Ok(steps)
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
    ) -> Self {
        let after = ExecutionMetrics::from_agent(
            metrics,
            success,
            &verification_scope,
            &verification_quality,
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
            tool_calls: after.tool_calls,
            executed_tool_calls: after.executed_tool_calls,
            prevented_redundant_calls: after.prevented_redundant_calls,
            input_tokens: after.input_tokens,
            output_tokens: after.output_tokens,
            total_tokens: after.total_tokens,
            bytes_read: after.bytes_read,
            context_bytes: after.context_bytes,
            bytes_shown_to_model: after.bytes_shown_to_model,
            verification_attempts: after.verification_attempts,
            process_spawns: after.process_spawns,
            agent_wall_time_ms: after.wall_time_ms,
            cpu_time_ms: after.cpu_time_ms,
            peak_rss_bytes: after.peak_rss_bytes,
            allocation_count: after.allocation_count,
            verification_scope,
            verification_quality,
            final_test_status,
            failure,
        }
    }
}

impl ExecutionMetrics {
    fn from_agent(metrics: &AgentMetrics, success: bool, scope: &str, quality: &str) -> Self {
        Self {
            success,
            model_steps: metrics.model_steps as u32,
            model_requests: metrics.model_requests as u32,
            requested_tool_calls: metrics.requested_tool_calls as u32,
            tool_calls: metrics.requested_tool_calls as u32,
            executed_tool_calls: metrics.executed_tool_calls as u32,
            prevented_redundant_calls: metrics.prevented_calls as u32,
            input_tokens: metrics.input_tokens,
            output_tokens: metrics.output_tokens,
            total_tokens: metrics.total_tokens,
            bytes_read: metrics.bytes_read,
            context_bytes: metrics.context_bytes,
            bytes_shown_to_model: metrics.bytes_shown_to_model,
            verification_attempts: metrics.verification_attempts as u32,
            process_spawns: metrics.process_spawns as u32,
            wall_time_ms: metrics.wall_time_ms,
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
        }
    }
}
