use std::{future::Future, pin::Pin, time::Duration};

use aether_agent::{
    Agent, AgentRequest, BackendError, BackendFuture, CancellationToken, ModelBackend,
    schedule_ready_calls,
};
use aether_core::{
    ModelEvent, ModelRequest, PermissionClass, ToolDefinition, ToolExecutionContext, ToolExecutor,
    ToolFootprint, ToolInvocation, ToolResult,
};
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;
use tokio::{sync::mpsc, time::sleep};

struct BenchBackend {
    calls: usize,
}

impl ModelBackend for BenchBackend {
    fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a> {
        let calls = self.calls;
        Box::pin(async move {
            let (sender, receiver) = mpsc::channel(calls.saturating_mul(2).saturating_add(4));
            let is_tool_result = request
                .input
                .as_array()
                .and_then(|items| items.first())
                .and_then(|item| item.get("type"))
                .and_then(|value| value.as_str())
                == Some("function_call_output");
            if is_tool_result {
                sender
                    .send(Ok(ModelEvent::TextDelta { text: "done".to_owned() }))
                    .await
                    .map_err(|_| BackendError::Cancelled)?;
            } else {
                for index in 0..calls {
                    sender
                        .send(Ok(ModelEvent::ToolCall {
                            call_id: aether_core::ToolCallId::new(format!("bench-{index}"))
                                .map_err(|error| BackendError::Mapping {
                                    message: error.to_string(),
                                })?,
                            name: "read".to_owned(),
                            arguments: json!({"path": format!("file-{index}")}),
                        }))
                        .await
                        .map_err(|_| BackendError::Cancelled)?;
                }
            }
            sender
                .send(Ok(ModelEvent::Done { continuation: None }))
                .await
                .map_err(|_| BackendError::Cancelled)?;
            Ok(receiver)
        })
    }

    fn discover_models<'a>(
        &'a self,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Vec<aether_agent::ModelCatalogItem>, BackendError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(Vec::new()) })
    }
}

struct BenchTools;

impl ToolExecutor for BenchTools {
    fn definitions(&self) -> &[ToolDefinition] {
        static DEFINITIONS: std::sync::OnceLock<Vec<ToolDefinition>> = std::sync::OnceLock::new();
        DEFINITIONS.get_or_init(|| {
            vec![ToolDefinition {
                name: "read".to_owned(),
                description: "benchmark read".to_owned(),
                input_schema: json!({"type": "object"}),
                permission: PermissionClass::ReadOnly,
            }]
        })
    }

    fn permission_request(&self, _: &ToolInvocation) -> Option<aether_core::PermissionRequest> {
        None
    }

    fn footprint(&self, invocation: &ToolInvocation) -> ToolFootprint {
        ToolFootprint::read_workspace(vec![
            invocation
                .input
                .get("path")
                .and_then(|value| value.as_str())
                .unwrap_or("file")
                .to_owned(),
        ])
    }

    fn execute<'a>(
        &'a self,
        invocation: ToolInvocation,
        _: ToolExecutionContext,
    ) -> aether_core::tools::ToolFuture<'a> {
        Box::pin(async move {
            sleep(Duration::from_millis(2)).await;
            ToolResult::success_text(invocation.call_id, "ok", 1024)
        })
    }
}

fn benchmark_agent(c: &mut Criterion, calls: usize, name: &str, metrics: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark runtime");
    c.bench_function(name, |bencher| {
        bencher.iter(|| {
            runtime.block_on(async {
                let agent = Agent::new(BenchBackend { calls }, BenchTools);
                let mut request = AgentRequest::new(
                    aether_core::SessionId::new("bench-session").unwrap(),
                    aether_core::TurnId::new("bench-turn").unwrap(),
                    "benchmark",
                );
                request.metrics = metrics;
                let (sender, _receiver) = mpsc::channel(128);
                agent.run(request, sender, CancellationToken::new()).await.unwrap();
            });
        })
    });
}

fn pure_scheduler_overhead(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_overhead_pure");
    for calls in [1_usize, 2, 4] {
        let footprints = (0..calls)
            .map(|index| ToolFootprint::read_workspace(vec![format!("ready-{index}")]))
            .collect::<Vec<_>>();
        let name = format!("ready_independent_{calls}");
        let mut selected = [usize::MAX; 4];
        group.bench_function(name, |bencher| {
            bencher.iter(|| {
                let count =
                    schedule_ready_calls(std::hint::black_box(&footprints), 4, &mut selected);
                let selected_checksum =
                    selected[..count].iter().copied().fold(0_usize, usize::wrapping_add);
                std::hint::black_box((count, selected_checksum));
            });
        });
    }
    group.finish();
}

fn end_to_end_scheduler(c: &mut Criterion) {
    benchmark_agent(c, 1, "agent_turn_end_to_end_one_tool", false);
    benchmark_agent(c, 0, "agent_turn_metrics_disabled", false);
    benchmark_agent(c, 0, "agent_turn_metrics_enabled", true);
}

fn parallel_independent_tools(c: &mut Criterion) {
    benchmark_agent(c, 8, "parallel_independent_tool_execution", false);
}

fn tokio_fixture_latency(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("benchmark runtime");
    c.bench_function("fixture_sleep_2ms_with_tokio_wakeup", |bencher| {
        bencher.iter(|| {
            runtime.block_on(async {
                sleep(Duration::from_millis(2)).await;
                std::hint::black_box(());
            });
        });
    });
}

criterion_group!(
    benches,
    pure_scheduler_overhead,
    end_to_end_scheduler,
    parallel_independent_tools,
    tokio_fixture_latency
);
criterion_main!(benches);
