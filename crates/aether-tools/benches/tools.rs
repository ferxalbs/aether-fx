use std::{fs, hint::black_box, path::PathBuf, process::Command};

use aether_agent::SessionStore;
use aether_core::{
    AgentEvent, BoundedText, SessionId, SessionLine, SessionRecord, ToolCallId, ToolInvocation,
};
use aether_terminal::Renderer;
use aether_tools::{ToolRegistry, apply_patch_text};
use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use serde_json::json;
use tokio::runtime::{Builder, Runtime};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn registry() -> ToolRegistry {
    ToolRegistry::new(workspace_root()).expect("benchmark workspace")
}

fn invocation(name: &str, input: serde_json::Value, number: u64) -> ToolInvocation {
    ToolInvocation {
        call_id: ToolCallId::new(format!("bench-{number}")).expect("benchmark id"),
        name: name.to_owned(),
        input,
    }
}

fn process_startup(c: &mut Criterion) {
    c.bench_function("process_startup", |b| {
        b.iter(|| {
            #[cfg(windows)]
            let mut command = Command::new("cmd");
            #[cfg(not(windows))]
            let mut command = Command::new("true");
            black_box(command.status().expect("process startup"))
        })
    });
}

fn argument_parsing(c: &mut Criterion) {
    c.bench_function("argument_parsing", |b| {
        b.iter(|| {
            let args = ["aether", "--model", "catalog-model", "inspect", "src"];
            let mut values = Vec::new();
            let mut index = 0;
            while index < args.len() {
                if args[index].starts_with('-') {
                    index += 1;
                } else {
                    values.push(args[index]);
                }
                index += 1;
            }
            black_box(values)
        })
    });
}

fn terminal_renderer(c: &mut Criterion) {
    c.bench_function("terminal_renderer", |b| {
        b.iter_batched(
            Renderer::new,
            |mut renderer| {
                let mut output = Vec::new();
                let event = AgentEvent::TextDelta { text: BoundedText::new("token", 64) };
                renderer.handle(&event, &mut output).expect("render");
                black_box(output);
            },
            BatchSize::SmallInput,
        )
    });
}

fn event_dispatch(c: &mut Criterion) {
    c.bench_function("event_dispatch", |b| {
        b.iter_batched(
            Renderer::new,
            |mut renderer| {
                let event = AgentEvent::ToolFinished {
                    call_id: ToolCallId::new("bench-call").expect("benchmark id"),
                    ok: true,
                };
                renderer.apply(&event);
                black_box(renderer.buffer().lines().len());
            },
            BatchSize::SmallInput,
        )
    });
}

fn tool_operations(c: &mut Criterion) {
    let runtime: Runtime =
        Builder::new_current_thread().enable_all().build().expect("benchmark runtime");
    let registry = registry();
    let mut group = c.benchmark_group("tool_operations");
    group.bench_function("small_file_read", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "read",
                json!({"files": [{"path": "Cargo.toml", "start_line": 1, "end_line": 20}]}),
                1,
            )))
        })
    });
    group.bench_function("large_file_read", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "read",
                json!({"files": [{"path": "Cargo.lock"}], "max_bytes": 65536}),
                2,
            )))
        })
    });
    group.bench_function("directory_listing", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "list",
                json!({"path": ".", "depth": 1, "max_entries": 128}),
                3,
            )))
        })
    });
    group.bench_function("path_finding", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "find",
                json!({"patterns": ["**/*.rs"], "max_results": 128}),
                4,
            )))
        })
    });
    group.bench_function("content_search", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "search",
                json!({"patterns": ["AETHER"], "path": "docs", "max_results": 128}),
                5,
            )))
        })
    });
    group.bench_function("tool_dispatch", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "read",
                json!({"files": [{"path": "Cargo.toml"}]}),
                6,
            )))
        })
    });
    group.finish();
}

fn search_comparison(c: &mut Criterion) {
    let rg_available =
        Command::new("rg").args(["--version"]).status().is_ok_and(|status| status.success());
    if !rg_available {
        return;
    }

    let runtime: Runtime =
        Builder::new_current_thread().enable_all().build().expect("benchmark runtime");
    let registry = registry();
    let mut group = c.benchmark_group("search_comparison");
    group.bench_function("aether", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "search",
                json!({"patterns": ["AETHER"], "path": "docs", "max_results": 128}),
                7,
            )))
        })
    });
    group.bench_function("rg", |b| {
        b.iter(|| {
            let output = Command::new("rg")
                .args(["--color", "never", "--line-number", "AETHER", "docs"])
                .output()
                .expect("rg search");
            black_box(output)
        })
    });
    group.finish();
}

fn patch_operations(c: &mut Criterion) {
    let hunk = aether_tools::PatchHunk {
        old_start: 2,
        old_count: 1,
        new_start: 2,
        new_count: 1,
        lines: vec!["-old".to_owned(), "+new".to_owned()],
    };
    c.bench_function("patch_validation", |b| {
        b.iter(|| black_box(apply_patch_text("first\nold\n", std::slice::from_ref(&hunk))))
    });
    c.bench_function("patch_application", |b| {
        b.iter(|| black_box(apply_patch_text("first\nold\n", std::slice::from_ref(&hunk))))
    });
}

fn session_replay(c: &mut Criterion) {
    let path =
        std::env::temp_dir().join(format!("aether-bench-session-{}.jsonl", std::process::id()));
    let session = SessionId::new("bench-session").expect("benchmark id");
    let lines = (1..=32)
        .map(|sequence| {
            serde_json::to_string(&SessionLine::new(
                sequence,
                session.clone(),
                None,
                SessionRecord::UserPrompt { prompt: "benchmark".to_owned() },
            ))
            .expect("session serialization")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&path, format!("{lines}\n")).expect("session fixture");
    c.bench_function("session_replay", |b| {
        b.iter(|| black_box(SessionStore::replay(&path).expect("session replay")))
    });
    let _ = fs::remove_file(path);
}

criterion_group!(
    benches,
    process_startup,
    argument_parsing,
    terminal_renderer,
    event_dispatch,
    tool_operations,
    search_comparison,
    patch_operations,
    session_replay
);
criterion_main!(benches);
