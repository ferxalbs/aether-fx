use std::path::PathBuf;

use aether_agent::{
    ContextEngine, LoopGuardrails, ManifestInfo, ManifestKind, ManifestStatus, PackageInfo,
    RepoMapSnapshot, classify_command, command_effects, plan_verification,
};
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

fn benchmark_verification_planning(c: &mut Criterion) {
    let repo = RepoMapSnapshot {
        root: PathBuf::from("."),
        tracked_file_count: 100,
        tracked_files: vec![],
        manifests: vec![ManifestInfo {
            path: "crates/demo/Cargo.toml".into(),
            kind: ManifestKind::Cargo,
            package_name: Some("demo".into()),
            workspace_members: vec![],
            status: ManifestStatus::Parsed,
            truncated: false,
        }],
        packages: vec![PackageInfo {
            name: Some("demo".into()),
            root: "crates/demo".into(),
            manifest: "crates/demo/Cargo.toml".into(),
        }],
        workspace_members: vec![],
        source_roots: vec![],
        test_paths: vec![],
        documentation: vec![],
        instructions: vec![],
        truncated: false,
    };
    let modified = vec!["crates/demo/src/lib.rs".to_owned()];
    c.bench_function("verification_plan/normal_rust_repository", |bencher| {
        bencher.iter(|| std::hint::black_box(plan_verification(&repo, &modified)));
    });
    let args = vec!["test".to_owned(), "-p".to_owned(), "demo".to_owned()];
    c.bench_function("verification_plan/classify_cargo_test", |bencher| {
        bencher.iter(|| std::hint::black_box(classify_command("cargo", &args)));
    });
    c.bench_function("command_intelligence/classify_direct_argv", |bencher| {
        bencher.iter(|| std::hint::black_box(classify_command("cargo", &args)));
    });
    c.bench_function("command_intelligence/extract_footprint", |bencher| {
        bencher.iter(|| {
            std::hint::black_box(command_effects("cargo", &args, "crates/demo").footprint())
        });
    });
}

fn benchmark_guardrails(c: &mut Criterion) {
    let input = json!({"files":[{"path":"src/lib.rs","start_line":1,"end_line":20}]});
    let result = read_result("src/lib.rs");
    let state = WorkflowState::new();
    let mut guard = LoopGuardrails::new();
    guard.observe("read", &input, &result, &state, &state);
    c.bench_function("loop_guardrails/fingerprint_lookup", |bencher| {
        bencher.iter(|| {
            std::hint::black_box(guard.preflight(
                ToolCallId::new("lookup").unwrap(),
                "read",
                &input,
                0,
            ))
        })
    });
    c.bench_function("loop_guardrails/progress_state_update", |bencher| {
        bencher.iter(|| {
            let mut guard = LoopGuardrails::new();
            std::hint::black_box(guard.observe("read", &input, &result, &state, &state))
        })
    });
}

criterion_group!(
    benches,
    benchmark_state_updates,
    benchmark_context_render,
    benchmark_verification_planning,
    benchmark_guardrails
);
criterion_main!(benches);
