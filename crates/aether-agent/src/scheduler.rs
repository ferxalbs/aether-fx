use std::sync::Arc;

use aether_core::{
    AgentEvent, DEFAULT_MAX_OUTPUT_BYTES, ExecutionPermit, PermissionDecision,
    ToolExecutionContext, ToolExecutor, ToolFootprint, ToolInvocation, ToolResult,
};
use serde_json::Value;
use tokio::{sync::mpsc, task::JoinHandle};

use crate::agent::send_event;
use crate::{AgentError, CancellationToken, PermissionBroker};

/// Maximum number of tool futures allowed to run at once.
pub const DEFAULT_MAX_PARALLEL_TOOLS: usize = 4;
const MAX_TOOL_CALLS_PER_STEP: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallState {
    Pending,
    Done,
}

struct ScheduledCall {
    invocation: ToolInvocation,
    permit: Option<ExecutionPermit>,
    footprint: ToolFootprint,
    permission: String,
}

/// A completed tool result with the original invocation needed for context observation.
pub(crate) struct ScheduledToolResult {
    pub(crate) name: String,
    pub(crate) input: Value,
    pub(crate) result: ToolResult,
}

/// Resolve permissions, schedule safe calls, and return deterministic model output.
pub async fn execute_tool_calls<T, P>(
    tools: &Arc<T>,
    tool_calls: Vec<(aether_core::ToolCallId, String, Value)>,
    events: &mpsc::Sender<AgentEvent>,
    cancellation: &CancellationToken,
    broker: &P,
) -> Result<Vec<ScheduledToolResult>, AgentError>
where
    T: ToolExecutor + 'static,
    P: PermissionBroker + ?Sized,
{
    let calls = prepare_calls(tools, tool_calls, events, cancellation, broker).await?;
    let mut states = vec![CallState::Pending; calls.len()];
    let mut results = (0..calls.len()).map(|_| None).collect::<Vec<Option<ToolResult>>>();
    let mut completed = 0_usize;
    let mut outputs = Vec::with_capacity(calls.len());

    while completed < calls.len() {
        if cancellation.is_cancelled() {
            return Err(AgentError::Cancelled);
        }

        let mut selected = Vec::with_capacity(DEFAULT_MAX_PARALLEL_TOOLS);
        for index in 0..calls.len() {
            if states[index] != CallState::Pending {
                continue;
            }
            if calls[index].permit.is_none() || !blocked_by_pending_conflict(index, &calls, &states)
            {
                selected.push(index);
                if selected.len() == DEFAULT_MAX_PARALLEL_TOOLS {
                    break;
                }
            } else {
                // Preserve model-order event visibility: a later call is not
                // admitted around an earlier unresolved dependency.
                break;
            }
        }
        if selected.is_empty() {
            return Err(AgentError::Tool("tool dependency scheduler made no progress".to_owned()));
        }

        for &index in &selected {
            let call = &calls[index];
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

        let mut handles = Vec::with_capacity(selected.len());
        for &index in &selected {
            let call = &calls[index];
            let Some(permit) = call.permit.clone() else {
                results[index] = Some(ToolResult::failure(
                    call.invocation.call_id.clone(),
                    "permission_denied",
                    "user denied permission for this operation",
                    false,
                    DEFAULT_MAX_OUTPUT_BYTES,
                ));
                continue;
            };
            let tools = Arc::clone(tools);
            let invocation = call.invocation.clone();
            let tool_cancellation = cancellation.flag();
            handles.push((
                index,
                tokio::spawn(async move {
                    tools
                        .execute(invocation, ToolExecutionContext::new(tool_cancellation, permit))
                        .await
                }),
            ));
        }

        for position in 0..handles.len() {
            let wait = wait_for_tool(&mut handles[position].1, cancellation).await;
            if wait.is_err() {
                for (_, active) in &mut handles {
                    active.abort();
                }
                return Err(AgentError::Cancelled);
            }
            let result = match wait.unwrap_or_else(|_| unreachable!()) {
                Ok(result) => result,
                Err(error) => ToolResult::failure(
                    calls[handles[position].0].invocation.call_id.clone(),
                    "operation_failed",
                    format!("tool task failed: {error}"),
                    false,
                    DEFAULT_MAX_OUTPUT_BYTES,
                ),
            };
            results[handles[position].0] = Some(result);
        }

        for &index in &selected {
            states[index] = CallState::Done;
            completed += 1;
            let call = &calls[index];
            let result = results[index]
                .take()
                .ok_or_else(|| AgentError::Tool("tool scheduler lost a result".to_owned()))?;
            send_event(
                events,
                AgentEvent::ToolOutput {
                    call_id: call.invocation.call_id.clone(),
                    output: result.output.clone(),
                },
            )
            .await?;
            send_event(
                events,
                AgentEvent::ToolFinished {
                    call_id: call.invocation.call_id.clone(),
                    ok: result.ok,
                },
            )
            .await?;
            outputs.push((index, result));
        }
    }

    outputs.sort_unstable_by_key(|(index, _)| *index);
    Ok(outputs
        .into_iter()
        .map(|(index, result)| ScheduledToolResult {
            name: calls[index].invocation.name.clone(),
            input: calls[index].invocation.input.clone(),
            result,
        })
        .collect())
}

async fn wait_for_tool(
    handle: &mut JoinHandle<ToolResult>,
    cancellation: &CancellationToken,
) -> Result<Result<ToolResult, tokio::task::JoinError>, ()> {
    tokio::select! {
        _ = cancellation.cancelled() => Err(()),
        result = handle => Ok(result),
    }
}

async fn prepare_calls<T, P>(
    tools: &Arc<T>,
    tool_calls: Vec<(aether_core::ToolCallId, String, Value)>,
    events: &mpsc::Sender<AgentEvent>,
    cancellation: &CancellationToken,
    broker: &P,
) -> Result<Vec<ScheduledCall>, AgentError>
where
    T: ToolExecutor + 'static,
    P: PermissionBroker + ?Sized,
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
            let class = definition_class(tools.as_ref(), &name);
            Some(ExecutionPermit::new(call_id.clone(), name.clone(), class))
        };
        let permission = definition_class(tools.as_ref(), &name).to_string();
        let footprint =
            if permit.is_some() { tools.footprint(&invocation) } else { ToolFootprint::empty() };
        calls.push(ScheduledCall { invocation, permit, footprint, permission });
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

fn blocked_by_pending_conflict(
    index: usize,
    calls: &[ScheduledCall],
    states: &[CallState],
) -> bool {
    (0..index).any(|prior| {
        states[prior] == CallState::Pending
            && calls[index].footprint.conflicts(&calls[prior].footprint)
    })
}

#[allow(dead_code)]
fn _join_handle_type_is_used(_: JoinHandle<ToolResult>) {}

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

    async fn run_scheduler(
        tools: TestTools,
        calls: Vec<(aether_core::ToolCallId, String, Value)>,
        cancellation: &CancellationToken,
    ) -> Result<Vec<ScheduledToolResult>, AgentError> {
        let tools = Arc::new(tools);
        let (sender, mut receiver) = mpsc::channel(128);
        let result =
            execute_tool_calls(&tools, calls, &sender, cancellation, &NoPermissionBroker).await;
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
            &tools,
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
