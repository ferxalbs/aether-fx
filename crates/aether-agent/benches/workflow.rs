use aether_agent::ContextEngine;
use aether_core::{ToolCallId, ToolResult, WorkflowState};
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;

fn read_result(path: &str) -> ToolResult {
    ToolResult::success_json(
        ToolCallId::new("workflow-bench").unwrap(),
        json!({
            "files": [{
                "path": path,
                "content_hash": "current",
                "lines": [{"number": 1, "text": "fn main() {}"}]
            }]
        }),
        4096,
    )
}

fn benchmark_state_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("workflow_state_updates");
    group.bench_function("bounded_transition_sequence", |bencher| {
        bencher.iter(|| {
            let mut state = WorkflowState::new();
            state.record_relevant_file("src/lib.rs");
            state.record_inspection();
            state.begin_modification();
            state.record_mutation();
            state.record_verification(true);
            state.complete_if_ready(&["src/lib.rs".to_owned()]);
            std::hint::black_box(state);
        });
    });
    group.finish();
}

fn benchmark_context_render(c: &mut Criterion) {
    let mut engine = ContextEngine::new("/workspace", Some("bench-model".to_owned()));
    let input = json!({"files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]});
    engine.observe_tool("read", &input, &read_result("src/lib.rs"));
    engine.observe_tool(
        "write",
        &json!({"path": "src/lib.rs"}),
        &ToolResult::success_json(
            ToolCallId::new("workflow-write-bench").unwrap(),
            json!({"path": "src/lib.rs", "hash": "current"}),
            4096,
        ),
    );
    let mut group = c.benchmark_group("workflow_context_render");
    group.bench_function("compact_actionable_state", |bencher| {
        bencher.iter(|| std::hint::black_box(engine.render(false)));
    });
    group.finish();
}

criterion_group!(benches, benchmark_state_updates, benchmark_context_render);
criterion_main!(benches);
