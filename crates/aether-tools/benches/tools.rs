use std::{fs, hint::black_box, path::PathBuf, process::Command, time::Duration};

use aether_agent::{AgentRequest, PermissionBroker, SessionPermissionBroker, SessionStore};
use aether_core::{
    AgentEvent, BoundedText, OpaqueContinuation, PermissionClass, PermissionDecision,
    PermissionEngine, PermissionPolicy, PermissionRequest, SessionId, SessionLine, SessionRecord,
    ToolCallId, ToolInvocation,
};
use aether_terminal::{Renderer, SelectorItem, select_from_items};
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

fn writable_registry(root: &PathBuf) -> ToolRegistry {
    ToolRegistry::with_policy(root, PermissionEngine::new(PermissionPolicy::allow_all()))
        .expect("benchmark writable workspace")
}

fn temporary_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("aether-bench-{label}-{}", std::process::id()));
    fs::create_dir_all(&root).expect("benchmark temp root");
    root
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

fn terminal_selectors(c: &mut Criterion) {
    let commands = [
        ("/model", "Model"),
        ("/status", "Status"),
        ("/context", "Context"),
        ("/config", "Config"),
        ("/auth", "Auth"),
        ("/sessions", "Sessions"),
        ("/resume", "Resume"),
        ("/doctor", "Doctor"),
        ("/help", "Help"),
        ("/clear", "Clear"),
        ("/exit", "Exit"),
    ]
    .into_iter()
    .map(|(value, label)| SelectorItem::new(value.to_owned(), label, None))
    .collect::<Vec<_>>();
    c.bench_function("slash_palette_open_filter", |b| {
        b.iter(|| {
            let mut input: &[u8] = b"model\r";
            let mut output = Vec::new();
            black_box(
                select_from_items(&mut input, &mut output, "Commands", &commands, None)
                    .expect("selector"),
            );
        })
    });

    let models = (0..1000)
        .map(|index| {
            SelectorItem::new(
                format!("model-{index:04}"),
                format!("Model {index:04}"),
                Some("tools · text".to_owned()),
            )
        })
        .collect::<Vec<_>>();
    c.bench_function("model_selector_filter_1000", |b| {
        b.iter(|| {
            let mut input: &[u8] = b"model-0999\r";
            let mut output = Vec::new();
            black_box(
                select_from_items(&mut input, &mut output, "Models", &models, None)
                    .expect("selector"),
            );
        })
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
    group.bench_function("blocking_dispatch_overhead", |b| {
        b.iter(|| {
            runtime.block_on(registry.dispatch(invocation(
                "read",
                json!({"files": [{"path": "Cargo.toml", "start_line": 1, "end_line": 1}]}),
                8,
            )))
        })
    });
    group.finish();
}

fn cancellation_permission_and_continuation(c: &mut Criterion) {
    c.bench_function("cooperative_search_cancellation_check", |b| {
        let cancellation = aether_core::CancellationFlag::new();
        b.iter(|| black_box(cancellation.check()))
    });

    let runtime = Builder::new_current_thread().enable_all().build().expect("benchmark runtime");
    let broker = SessionPermissionBroker::new();
    c.bench_function("permission_decision", |b| {
        b.iter(|| {
            runtime.block_on(async {
                let request = PermissionRequest {
                    call_id: ToolCallId::new("bench-permission").expect("benchmark id"),
                    tool: "write".to_owned(),
                    class: PermissionClass::WorkspaceWrite,
                    operation: "write".to_owned(),
                    target: Some("bench.txt".to_owned()),
                    details: json!({"bytes": 1}),
                };
                let call_id = request.call_id.clone();
                let decision = broker.decide(request);
                let _ = broker.resolve(&call_id, PermissionDecision::AllowOnce);
                black_box(decision.await)
            })
        })
    });

    c.bench_function("multi_turn_bookkeeping", |b| {
        let session = SessionId::new("bench-session").expect("benchmark id");
        b.iter(|| {
            let mut request = AgentRequest::new(
                session.clone(),
                aether_core::TurnId::new("turn-2").expect("benchmark id"),
                "continue",
            );
            request.continuation = Some(OpaqueContinuation(json!({
                "previous_response_id": "resp-bench"
            })));
            black_box(request)
        })
    });
}

fn atomic_and_staged_writes(c: &mut Criterion) {
    let runtime = Builder::new_current_thread().enable_all().build().expect("benchmark runtime");
    let root = temporary_root("writes");
    let registry = writable_registry(&root);
    fs::write(root.join("atomic.txt"), "old\n").expect("benchmark fixture");
    c.bench_function("atomic_write", |b| {
        b.iter(|| {
            black_box(runtime.block_on(registry.dispatch(invocation(
                "write",
                json!({"path": "atomic.txt", "content": "new\n"}),
                9,
            ))))
        })
    });

    c.bench_function("patch_staged_application", |b| {
        b.iter(|| {
            fs::write(root.join("patch.txt"), "first\nold\n").expect("benchmark fixture");
            black_box(runtime.block_on(registry.dispatch(invocation(
                "patch",
                json!({"files": [{
                    "path": "patch.txt",
                    "hunks": [{
                        "old_start": 2,
                        "old_count": 1,
                        "new_start": 2,
                        "new_count": 1,
                        "lines": ["-old", "+new"]
                    }]
                }]}),
                10,
            ))))
        })
    });
    runtime.block_on(registry.workspace().shutdown_processes());
    let _ = fs::remove_dir_all(root);
}

fn process_buffer_and_registry(c: &mut Criterion) {
    let runtime = Builder::new_current_thread().enable_all().build().expect("benchmark runtime");
    let root = temporary_root("process");
    let registry = writable_registry(&root);
    let (program, args) = if cfg!(windows) {
        ("cmd", vec!["/C".to_owned(), "for /L %i in (1,1,100000) do @echo x".to_owned()])
    } else {
        ("yes", vec!["x".to_owned()])
    };
    let started = runtime.block_on(registry.dispatch(invocation(
        "process",
        json!({"operation": "start", "program": program, "args": args}),
        11,
    )));
    if !started.ok {
        let _ = fs::remove_dir_all(root);
        return;
    }
    let process_id =
        started.data.expect("process data")["process_id"].as_u64().expect("process id");
    runtime.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
    c.bench_function("process_buffer_read", |b| {
        b.iter(|| {
            black_box(runtime.block_on(registry.dispatch(invocation(
                "process",
                json!({
                    "operation": "read",
                    "process_id": process_id,
                    "max_bytes": 4096,
                    "timeout_ms": 10
                }),
                12,
            ))))
        })
    });
    c.bench_function("process_registry_lookup", |b| {
        b.iter(|| {
            black_box(runtime.block_on(registry.dispatch(invocation(
                "process",
                json!({"operation": "status", "process_id": process_id}),
                13,
            ))))
        })
    });
    runtime.block_on(registry.workspace().shutdown_processes());
    let _ = fs::remove_dir_all(root);
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
    let root = std::env::temp_dir().join(format!("aether-bench-session-{}", std::process::id()));
    fs::create_dir_all(&root).expect("benchmark root");
    let session = SessionId::new("bench-session").expect("benchmark id");
    let path = SessionStore::directory_for(&root)
        .expect("session directory")
        .join(format!("{session}.jsonl"));
    let lines = (1..=32)
        .map(|sequence| {
            serde_json::to_string(&SessionLine::new(
                sequence,
                session.clone(),
                None,
                SessionRecord::Started {
                    workspace_root: "/workspace".to_owned(),
                    model: Some("bench".to_owned()),
                },
            ))
            .expect("session serialization")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::create_dir_all(path.parent().expect("session parent")).expect("session directory");
    fs::write(&path, format!("{lines}\n")).expect("session fixture");
    c.bench_function("session_replay", |b| {
        b.iter(|| {
            black_box(
                SessionStore::open(&root, session.clone())
                    .expect("session store")
                    .read()
                    .expect("session replay"),
            )
        })
    });
    let _ = fs::remove_dir_all(root);
}

fn session_discovery(c: &mut Criterion) {
    let root = temporary_root("session-discovery");
    for number in 0..100 {
        let session = SessionId::new(format!("discovery-{number:03}")).expect("benchmark id");
        let path = SessionStore::directory_for(&root)
            .expect("session directory")
            .join(format!("{session}.jsonl"));
        fs::create_dir_all(path.parent().expect("session parent")).expect("session directory");
        let started = SessionLine::new(
            1,
            session.clone(),
            None,
            SessionRecord::Started {
                workspace_root: root.display().to_string(),
                model: Some("bench".to_owned()),
            },
        );
        fs::write(
            path,
            format!("{}\n", serde_json::to_string(&started).expect("session serialization")),
        )
        .expect("session fixture");
    }
    let mut group = c.benchmark_group("session_discovery");
    for count in [10_usize, 100] {
        let subset = temporary_root(&format!("session-discovery-{count}"));
        let source = SessionStore::directory_for(&root).expect("session directory");
        let target = SessionStore::directory_for(&subset).expect("session directory");
        fs::create_dir_all(&target).expect("session directory");
        for number in 0..count {
            let name = format!("discovery-{number:03}.jsonl");
            fs::copy(source.join(&name), target.join(name)).expect("session copy");
        }
        group.bench_function(format!("{count}_sessions"), |b| {
            b.iter(|| black_box(SessionStore::summaries(&subset).expect("session discovery")))
        });
        let _ = fs::remove_dir_all(subset);
    }
    group.finish();
    let _ = fs::remove_dir_all(root);
}

fn state_workspace_resolution(c: &mut Criterion) {
    let root = temporary_root("state-resolution");
    c.bench_function("state_workspace_resolution", |b| {
        b.iter(|| {
            black_box((
                SessionStore::state_root().expect("state root"),
                SessionStore::canonical_workspace(&root).expect("canonical workspace"),
                SessionStore::workspace_id(&root).expect("workspace id"),
                SessionStore::directory_for(&root).expect("session directory"),
            ))
        })
    });
    let _ = fs::remove_dir_all(root);
}

criterion_group!(
    benches,
    process_startup,
    argument_parsing,
    terminal_renderer,
    event_dispatch,
    terminal_selectors,
    tool_operations,
    cancellation_permission_and_continuation,
    atomic_and_staged_writes,
    process_buffer_and_registry,
    search_comparison,
    patch_operations,
    session_replay,
    session_discovery,
    state_workspace_resolution
);
criterion_main!(benches);
