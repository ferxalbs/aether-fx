use std::{future::Future, pin::Pin, task::Poll};

use aether_core::{
    AgentEvent, DEFAULT_MAX_OUTPUT_BYTES, ExecutionPermit, PermissionDecision,
    ToolExecutionContext, ToolExecutor, ToolFootprint, ToolInvocation, ToolResult,
};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::agent::send_event;
use crate::{AgentError, CancellationToken, PermissionBroker};

/// Maximum number of tool futures allowed to run at once.
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;
const MAX_TOOL_CALLS_PER_STEP: usize = 64;

struct ScheduledCall {
    invocation: ToolInvocation,
    permit: Option<ExecutionPermit>,
    preflight: Option<ToolResult>,
    footprint: ToolFootprint,
    permission: String,
}

struct CompletedCall {
    call_id: aether_core::ToolCallId,
    name: String,
    input: Value,
    executed: bool,
}

/// A completed tool result with the original invocation needed for context observation.
pub(crate) struct ScheduledToolResult {
    pub(crate) name: String,
    pub(crate) input: Value,
    pub(crate) result: ToolResult,
    pub(crate) executed: bool,
    pub(crate) planned: bool,
    pub(crate) planner_actions: usize,
}

/// Deterministic pre-execution gate for workflow or composition-root policy.
pub(crate) trait ToolCallGate {
    /// Return a typed result when the call must not execute.
    fn preflight(&self, invocation: &ToolInvocation) -> Option<ToolResult>;
}

#[allow(dead_code)]
struct AllowAllToolCallGate;

impl ToolCallGate for AllowAllToolCallGate {
    fn preflight(&self, _: &ToolInvocation) -> Option<ToolResult> {
        None
    }
}

/// Select a conservative prefix of ready calls without allocating or waiting.
pub fn schedule_ready_calls(
    footprints: &[ToolFootprint],
    max_parallel: usize,
    selected: &mut [usize],
) -> usize {
    let limit = max_parallel.min(selected.len());
    let mut count = 0;
    for index in 0..footprints.len() {
        if count == limit {
            break;
        }
        if selected[..count].iter().any(|&prior| footprints[index].conflicts(&footprints[prior])) {
            break;
        }
        selected[count] = index;
        count += 1;
    }
    count
}

/// Resolve permissions, schedule safe calls, and return deterministic model output.
#[allow(dead_code)]
pub async fn execute_tool_calls<T, P>(
    tools: &T,
    tool_calls: Vec<(aether_core::ToolCallId, String, Value)>,
    events: &mpsc::Sender<AgentEvent>,
    cancellation: &CancellationToken,
    broker: &P,
) -> Result<Vec<ScheduledToolResult>, AgentError>
where
    T: ToolExecutor,
    P: PermissionBroker + ?Sized,
{
    execute_tool_calls_with_gate(
        tools,
        tool_calls,
        events,
        cancellation,
        broker,
        &AllowAllToolCallGate,
    )
    .await
}

pub(crate) async fn execute_tool_calls_with_gate<T, P, G>(
    tools: &T,
    tool_calls: Vec<(aether_core::ToolCallId, String, Value)>,
    events: &mpsc::Sender<AgentEvent>,
    cancellation: &CancellationToken,
    broker: &P,
    gate: &G,
) -> Result<Vec<ScheduledToolResult>, AgentError>
where
    T: ToolExecutor,
    P: PermissionBroker + ?Sized,
    G: ToolCallGate + ?Sized,
{
    let mut calls = prepare_calls(tools, tool_calls, events, cancellation, broker, gate).await?;
    let mut completed = 0_usize;
    let mut outputs = Vec::with_capacity(calls.len());
    let mut selected = [0_usize; DEFAULT_MAX_PARALLEL_TOOLS];

    while completed < calls.len() {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        let selected_len = select_pending_calls(&calls, &mut selected);
        if selected_len == 0 {
            return Err(AgentError::Tool("tool dependency scheduler made no progress".to_owned()));
        }

        for &index in &selected[..selected_len] {
            let call = calls[index]
                .as_ref()
                .ok_or_else(|| AgentError::Tool("tool scheduler lost a pending call".to_owned()))?;
            send_event(
                events,
                AgentEvent::ToolStarted {
                    call_id: call.invocation.call_id.clone(),
                    name: call.invocation.name.clone(),
                    permission: call.permission.clone(),
                    operation: call.invocation.name.clone(),
                    step_id: None,
                },
            )
            .await?;
        }

        let mut futures = std::array::from_fn(|_| None);
        let mut batch_results = std::array::from_fn(|_| None);
        let mut metadata: [Option<CompletedCall>; DEFAULT_MAX_PARALLEL_TOOLS] =
            std::array::from_fn(|_| None);
        for (slot, &index) in selected[..selected_len].iter().enumerate() {
            let call = calls[index].take().ok_or_else(|| {
                AgentError::Tool("tool scheduler lost a selected call".to_owned())
            })?;
            let ScheduledCall { invocation, permit, preflight, .. } = call;
            let completed_call = CompletedCall {
                call_id: invocation.call_id.clone(),
                name: invocation.name.clone(),
                input: invocation.input.clone(),
                executed: permit.is_some() && preflight.is_none(),
            };
            metadata[slot] = Some(completed_call);
            if let Some(result) = preflight {
                batch_results[slot] = Some(result);
                continue;
            }
            let Some(permit) = permit else {
                let call_id = metadata[slot]
                    .as_ref()
                    .expect("metadata inserted for every selected call")
                    .call_id
                    .clone();
                batch_results[slot] = Some(ToolResult::failure(
                    call_id,
                    "permission_denied",
                    "user denied permission for this operation",
                    false,
                    DEFAULT_MAX_OUTPUT_BYTES,
                ));
                continue;
            };
            let future = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                tools.execute(invocation, ToolExecutionContext::new(cancellation.flag(), permit))
            }));
            match future {
                Ok(future) => futures[slot] = Some(future),
                Err(_) => {
                    let call_id = metadata[slot]
                        .as_ref()
                        .expect("metadata inserted for every selected call")
                        .call_id
                        .clone();
                    batch_results[slot] = Some(tool_panic_result(call_id));
                }
            }
        }

        wait_for_batch(&mut futures, &mut batch_results, &metadata, selected_len, cancellation)
            .await?;

        for slot in 0..selected_len {
            let metadata = metadata[slot]
                .take()
                .ok_or_else(|| AgentError::Tool("tool scheduler lost call metadata".to_owned()))?;
            let result = batch_results[slot]
                .take()
                .ok_or_else(|| AgentError::Tool("tool scheduler lost a result".to_owned()))?;
            completed += 1;
            send_event(
                events,
                AgentEvent::ToolOutput {
                    call_id: metadata.call_id.clone(),
                    output: result.output.clone(),
                },
            )
            .await?;
            send_event(
                events,
                AgentEvent::ToolFinished { call_id: metadata.call_id.clone(), ok: result.ok },
            )
            .await?;
            outputs.push((metadata, result));
        }
    }

    Ok(outputs
        .into_iter()
        .map(|(metadata, result)| ScheduledToolResult {
            name: metadata.name,
            input: metadata.input,
            result,
            executed: metadata.executed,
            planned: false,
            planner_actions: 0,
        })
        .collect())
}

async fn wait_for_batch<'a>(
    futures: &mut [Option<aether_core::tools::ToolFuture<'a>>; DEFAULT_MAX_PARALLEL_TOOLS],
    results: &mut [Option<ToolResult>; DEFAULT_MAX_PARALLEL_TOOLS],
    metadata: &[Option<CompletedCall>; DEFAULT_MAX_PARALLEL_TOOLS],
    count: usize,
    cancellation: &CancellationToken,
) -> Result<(), AgentError> {
    let cancellation_wait = cancellation.cancelled();
    tokio::pin!(cancellation_wait);
    std::future::poll_fn(|cx| {
        if cancellation.is_cancelled() || cancellation_wait.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err(AgentError::Cancelled));
        }

        let mut pending = false;
        for slot in 0..count {
            let Some(future) = futures[slot].as_mut() else {
                continue;
            };
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Pin::new(future).poll(cx)
            })) {
                Ok(Poll::Ready(result)) => {
                    results[slot] = Some(result);
                    futures[slot] = None;
                }
                Ok(Poll::Pending) => pending = true,
                Err(_) => {
                    let call_id = metadata[slot]
                        .as_ref()
                        .expect("metadata inserted for every selected call")
                        .call_id
                        .clone();
                    results[slot] = Some(tool_panic_result(call_id));
                    futures[slot] = None;
                }
            }
        }
        if pending { Poll::Pending } else { Poll::Ready(Ok(())) }
    })
    .await
}

fn tool_panic_result(call_id: aether_core::ToolCallId) -> ToolResult {
    ToolResult::failure(
        call_id,
        "operation_failed",
        "tool task panicked",
        false,
        DEFAULT_MAX_OUTPUT_BYTES,
    )
}

async fn prepare_calls<T, P, G>(
    tools: &T,
    tool_calls: Vec<(aether_core::ToolCallId, String, Value)>,
    events: &mpsc::Sender<AgentEvent>,
    cancellation: &CancellationToken,
    broker: &P,
    gate: &G,
) -> Result<Vec<Option<ScheduledCall>>, AgentError>
where
    T: ToolExecutor,
    P: PermissionBroker + ?Sized,
    G: ToolCallGate + ?Sized,
{
    if tool_calls.len() > MAX_TOOL_CALLS_PER_STEP {
        return Err(AgentError::Tool(
            "tool call batch exceeds the bounded scheduler limit".to_owned(),
        ));
    }
    let mut calls = Vec::with_capacity(tool_calls.len());
    for (call_id, name, arguments) in tool_calls {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }
        let invocation =
            ToolInvocation { call_id: call_id.clone(), name: name.clone(), input: arguments };
        let class = definition_class(tools, &name);
        let permission_request = tools.permission_request(&invocation);
        let permit = if let Some(permission_request) = permission_request {
            let needs_prompt = broker.needs_prompt(&permission_request);
            let decision_future = broker.decide(permission_request.clone());
            if needs_prompt {
                send_event(
                    events,
                    AgentEvent::PermissionRequested { request: permission_request.clone() },
                )
                .await?;
            }
            let decision = tokio::select! {
                _ = cancellation.cancelled() => {
                    broker.cancel(&call_id);
                    return Err(AgentError::Cancelled);
                }
                decision = decision_future => decision,
            };
            if cancellation.is_cancelled() {
                broker.cancel(&call_id);
                return Err(AgentError::Cancelled);
            }
            if needs_prompt {
                send_event(
                    events,
                    AgentEvent::PermissionResolved { call_id: call_id.clone(), decision },
                )
                .await?;
            }
            match decision {
                PermissionDecision::AllowOnce | PermissionDecision::AllowSession => Some(
                    ExecutionPermit::new(call_id.clone(), name.clone(), permission_request.class),
                ),
                PermissionDecision::Deny => None,
            }
        } else {
            Some(ExecutionPermit::new(call_id.clone(), name.clone(), class))
        };
        let preflight = permit.as_ref().and_then(|_| gate.preflight(&invocation));
        let permission = class.to_string();
        let footprint = if permit.is_some() && preflight.is_none() {
            tools.footprint(&invocation)
        } else {
            ToolFootprint::empty()
        };
        calls.push(Some(ScheduledCall { invocation, permit, preflight, footprint, permission }));
    }
    Ok(calls)
}

fn definition_class<T: ToolExecutor + ?Sized>(
    tools: &T,
    name: &str,
) -> aether_core::PermissionClass {
    tools
        .definitions()
        .iter()
        .find(|definition| definition.name == name)
        .map(|definition| definition.permission)
        .unwrap_or(aether_core::PermissionClass::ReadOnly)
}

fn blocked_by_pending_conflict(index: usize, calls: &[Option<ScheduledCall>]) -> bool {
    (0..index).any(|prior| {
        let Some(prior_call) = calls[prior].as_ref() else {
            return false;
        };
        calls[index].as_ref().is_some_and(|call| call.footprint.conflicts(&prior_call.footprint))
    })
}

fn select_pending_calls(
    calls: &[Option<ScheduledCall>],
    selected: &mut [usize; DEFAULT_MAX_PARALLEL_TOOLS],
) -> usize {
    let mut count = 0;
    for index in 0..calls.len() {
        if count == DEFAULT_MAX_PARALLEL_TOOLS {
            break;
        }
        let Some(call) = calls[index].as_ref() else {
            continue;
        };
        if call.permit.is_none() || !blocked_by_pending_conflict(index, calls) {
            selected[count] = index;
            count += 1;
        } else {
            // Preserve model-order event visibility: a later call is not
            // admitted around an earlier unresolved dependency.
            break;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    use aether_core::{PermissionClass, ToolDefinition, ToolExecutionContext, ToolInvocation};
    use serde_json::json;

    use super::*;
    use crate::NoPermissionBroker;

    #[derive(Clone)]
    struct TestTools {
        active: Arc<AtomicUsize>,
        maximum: Arc<AtomicUsize>,
        executions: Arc<AtomicUsize>,
        started: Arc<Mutex<Vec<String>>>,
    }

    impl TestTools {
        fn new() -> Self {
            Self {
                active: Arc::new(AtomicUsize::new(0)),
                maximum: Arc::new(AtomicUsize::new(0)),
                executions: Arc::new(AtomicUsize::new(0)),
                started: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl ToolExecutor for TestTools {
        fn definitions(&self) -> &[ToolDefinition] {
            static DEFINITIONS: std::sync::OnceLock<Vec<ToolDefinition>> =
                std::sync::OnceLock::new();
            DEFINITIONS.get_or_init(|| {
                vec![ToolDefinition {
                    name: "fixture".to_owned(),
                    description: "scheduler fixture".to_owned(),
                    input_schema: json!({"type": "object"}),
                    permission: PermissionClass::ReadOnly,
                }]
            })
        }

        fn permission_request(
            &self,
            invocation: &ToolInvocation,
        ) -> Option<aether_core::PermissionRequest> {
            invocation.input.get("deny").and_then(|value| value.as_bool()).filter(|deny| *deny).map(
                |_| aether_core::PermissionRequest {
                    call_id: invocation.call_id.clone(),
                    tool: invocation.name.clone(),
                    class: PermissionClass::WorkspaceWrite,
                    operation: "fixture write".to_owned(),
                    target: Some("fixture".to_owned()),
                    details: json!({}),
                },
            )
        }

        fn footprint(&self, invocation: &ToolInvocation) -> ToolFootprint {
            match invocation.input.get("effect").and_then(|value| value.as_str()) {
                Some("read") => ToolFootprint::read_workspace(vec![
                    invocation
                        .input
                        .get("path")
                        .and_then(|value| value.as_str())
                        .unwrap_or("fixture")
                        .to_owned(),
                ]),
                Some("write") => ToolFootprint::exclusive_workspace(),
                Some("unknown") => ToolFootprint::unknown(),
                _ => ToolFootprint::empty(),
            }
        }

        fn execute<'a>(
            &'a self,
            invocation: ToolInvocation,
            context: ToolExecutionContext,
        ) -> aether_core::tools::ToolFuture<'a> {
            let active = Arc::clone(&self.active);
            let maximum = Arc::clone(&self.maximum);
            let executions = Arc::clone(&self.executions);
            let started = Arc::clone(&self.started);
            Box::pin(async move {
                executions.fetch_add(1, Ordering::Relaxed);
                started.lock().unwrap().push(invocation.call_id.to_string());
                let now = active.fetch_add(1, Ordering::Relaxed) + 1;
                let mut observed = maximum.load(Ordering::Relaxed);
                while now > observed {
                    match maximum.compare_exchange_weak(
                        observed,
                        now,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(value) => observed = value,
                    }
                }
                if invocation.input.get("hang").and_then(|value| value.as_bool()).unwrap_or(false) {
                    while !context.cancellation().is_cancelled() {
                        tokio::time::sleep(Duration::from_millis(2)).await;
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(
                        invocation
                            .input
                            .get("delay_ms")
                            .and_then(|value| value.as_u64())
                            .unwrap_or(5),
                    ))
                    .await;
                }
                active.fetch_sub(1, Ordering::Relaxed);
                ToolResult::success_text(invocation.call_id, "ok", 1024)
            })
        }
    }

    fn call(
        id: usize,
        effect: &str,
        path: &str,
        delay_ms: u64,
    ) -> (aether_core::ToolCallId, String, Value) {
        (
            aether_core::ToolCallId::new(format!("call-{id}")).unwrap(),
            "fixture".to_owned(),
            json!({"effect": effect, "path": path, "delay_ms": delay_ms}),
        )
    }

    #[test]
    fn pure_scheduler_selects_ready_independent_prefix_without_crossing_conflict() {
        let footprints = [
            ToolFootprint::read_workspace(vec!["one".to_owned()]),
            ToolFootprint::read_workspace(vec!["two".to_owned()]),
            ToolFootprint::exclusive_workspace(),
        ];
        let mut selected = [usize::MAX; DEFAULT_MAX_PARALLEL_TOOLS];
        assert_eq!(schedule_ready_calls(&footprints, 4, &mut selected), 2);
        assert_eq!(&selected[..2], &[0, 1]);
    }

    async fn run_scheduler(
        tools: TestTools,
        calls: Vec<(aether_core::ToolCallId, String, Value)>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<ScheduledToolResult>, AgentError> {
        let tools = Arc::new(tools);
        let (sender, mut receiver) = mpsc::channel(128);
        let result =
            execute_tool_calls(tools.as_ref(), calls, &sender, cancellation, &NoPermissionBroker)
                .await;
        while receiver.try_recv().is_ok() {}
        result
    }

    #[tokio::test]
    async fn independent_reads_execute_concurrently() {
        let tools = TestTools::new();
        let maximum = Arc::clone(&tools.maximum);
        let started = Arc::clone(&tools.started);
        let started_at = Instant::now();
        let result = run_scheduler(
            tools,
            vec![call(1, "read", "one", 30), call(2, "read", "two", 30)],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(started_at.elapsed() < Duration::from_millis(55));
        assert_eq!(maximum.load(Ordering::Relaxed), 2);
        assert_eq!(result.len(), 2);
        assert_eq!(*started.lock().unwrap(), vec!["call-1", "call-2"]);
    }

    #[tokio::test]
    async fn conflicting_read_and_write_are_serialized() {
        let tools = TestTools::new();
        let maximum = Arc::clone(&tools.maximum);
        let result = run_scheduler(
            tools,
            vec![call(1, "write", "same", 10), call(2, "read", "same", 10)],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(maximum.load(Ordering::Relaxed), 1);
        assert_eq!(result.len(), 2);
    }

    #[tokio::test]
    async fn multiple_writes_preserve_model_order_and_do_not_overlap() {
        let tools = TestTools::new();
        let maximum = Arc::clone(&tools.maximum);
        let started = Arc::clone(&tools.started);
        run_scheduler(
            tools,
            vec![
                call(1, "write", "one", 5),
                call(2, "write", "two", 5),
                call(3, "write", "three", 5),
            ],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(maximum.load(Ordering::Relaxed), 1);
        assert_eq!(*started.lock().unwrap(), vec!["call-1", "call-2", "call-3"]);
    }

    #[tokio::test]
    async fn cancellation_stops_active_scheduler_work() {
        let tools = TestTools::new();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let task = tokio::spawn(async move {
            run_scheduler(
                tools,
                vec![(
                    aether_core::ToolCallId::new("call-1").unwrap(),
                    "fixture".to_owned(),
                    json!({"effect": "read", "hang": true}),
                )],
                &task_cancellation,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        cancellation.cancel();
        assert!(matches!(task.await.unwrap(), Err(AgentError::Cancelled)));
    }

    #[tokio::test]
    async fn permission_denial_does_not_execute_tool() {
        let tools = TestTools::new();
        let executions = Arc::clone(&tools.executions);
        let calls = vec![(
            aether_core::ToolCallId::new("denied").unwrap(),
            "fixture".to_owned(),
            json!({"effect": "write", "deny": true}),
        )];
        let result = run_scheduler(tools, calls, &CancellationToken::new()).await.unwrap();
        assert_eq!(executions.load(Ordering::Relaxed), 0);
        assert_eq!(result[0].result.error.as_ref().unwrap().code, "permission_denied");
    }

    #[tokio::test]
    async fn results_are_ordered_even_when_completion_is_not() {
        let tools = TestTools::new();
        let result = run_scheduler(
            tools,
            vec![call(1, "read", "one", 30), call(2, "read", "two", 1)],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(result[0].result.call_id.as_str(), "call-1");
        assert_eq!(result[1].result.call_id.as_str(), "call-2");
    }

    #[tokio::test]
    async fn lifecycle_events_commit_in_call_order() {
        let tools = Arc::new(TestTools::new());
        let (sender, mut receiver) = mpsc::channel(32);
        execute_tool_calls(
            tools.as_ref(),
            vec![call(1, "read", "one", 30), call(2, "read", "two", 1)],
            &sender,
            &CancellationToken::new(),
            &NoPermissionBroker,
        )
        .await
        .unwrap();
        let mut events = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
        let call_ids = events
            .iter()
            .filter_map(|event| match event {
                AgentEvent::ToolStarted { call_id, .. }
                | AgentEvent::ToolOutput { call_id, .. }
                | AgentEvent::ToolFinished { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(call_ids, vec!["call-1", "call-2", "call-1", "call-1", "call-2", "call-2"]);
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let tools = TestTools::new();
        let maximum = Arc::clone(&tools.maximum);
        let calls = (0..12).map(|id| call(id, "read", &format!("file-{id}"), 15)).collect();
        run_scheduler(tools, calls, &CancellationToken::new()).await.unwrap();
        assert!(maximum.load(Ordering::Relaxed) <= DEFAULT_MAX_PARALLEL_TOOLS);
    }
}
