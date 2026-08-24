use aether_agent::{AutonomousCodingPolicy, ContextEngine};
use aether_core::{
    ContextSnapshot, DecisionAction, DecisionState, ToolCallId, ToolResult, WorkflowPhase,
};
use criterion::{Criterion, criterion_group, criterion_main};
use serde_json::json;
use std::hint::black_box;

fn policy_update(c: &mut Criterion) {
    c.bench_function("policy_update_completed_observation", |benchmark| {
        benchmark.iter_batched(
            || {
                let mut engine = ContextEngine::new("", None);
                engine.set_task("inspect parser");
                let policy = AutonomousCodingPolicy::new();
                let result = ToolResult::success_json(
                    ToolCallId::new("policy-bench").expect("valid id"),
                    json!({
                        "files": [{
                            "path": "src/parser.rs",
                            "content_hash": "hash",
                            "lines": [{"number": 1, "text": "fn parse() {}"}]
                        }]
                    }),
                    4096,
                );
                (policy, engine, result)
            },
            |(mut policy, mut engine, result)| {
                let feedback = policy.observe(
                    &mut engine,
                    "read",
                    &json!({"files":[{"path":"src/parser.rs"}]}),
                    &result,
                    true,
                );
                black_box(feedback);
                black_box(engine.snapshot());
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

fn next_action_ranking(c: &mut Criterion) {
    let mut snapshot = ContextSnapshot::new("", None);
    snapshot.workflow.phase = WorkflowPhase::Inspect;
    snapshot.workflow.decision = DecisionState::new();
    snapshot.workflow.decision.upsert_candidate("src/parser.rs", 90, 2, false, false, false);
    snapshot.workflow.decision.progress_confidence = 30;
    let policy = AutonomousCodingPolicy::new();
    c.bench_function("policy_next_action_ranking", |benchmark| {
        benchmark.iter(|| {
            let actions = policy.rank_next_actions(black_box(&snapshot));
            assert_eq!(actions[0].action, DecisionAction::InspectCandidate);
            black_box(actions);
        });
    });
}

criterion_group!(policy_benches, policy_update, next_action_ranking);
criterion_main!(policy_benches);
