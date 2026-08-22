#![deny(unsafe_code)]

//! The exact nine model-visible local tools.

mod common;
mod find;
mod git;
mod list;
mod patch;
mod platform;
mod process;
mod read;
mod schema;
mod search;
mod shell;
mod write;

pub use common::Workspace;
pub use find::{FindInput, FindOutput};
pub use git::{GitInput, GitOperation, GitOutput};
pub use list::{ListEntry, ListInput, ListOutput};
pub use patch::{PatchFile, PatchHunk, PatchInput, PatchOutput, apply_patch_text};
pub use process::{
    ProcessInput, ProcessOperation, ProcessOutput, ProcessShutdownFailure, ProcessShutdownReport,
};
pub use read::{ReadFile, ReadInput, ReadLine, ReadTarget};
pub use schema::{TOOL_NAMES, ToolRegistry};
pub use search::{SearchInput, SearchMatch, SearchOutput};
pub use shell::{ShellInput, ShellOutput};
pub use write::{WriteInput, WriteOutput};

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };

    use aether_core::{
        CancellationFlag, ExecutionPermit, PermissionClass, PermissionEngine, PermissionPolicy,
        ToolCallId, ToolExecutionContext, ToolInvocation, ToolResult,
    };

    use super::{ToolRegistry, Workspace, common};

    fn test_root() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let path = std::env::temp_dir().join(format!(
            "aether-tools-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn call(name: &str, input: serde_json::Value, number: u64) -> ToolInvocation {
        ToolInvocation {
            call_id: ToolCallId::new(format!("test-{number}")).unwrap(),
            name: name.to_owned(),
            input,
        }
    }

    fn registry(root: &PathBuf, allow_writes: bool) -> ToolRegistry {
        let policy =
            if allow_writes { PermissionPolicy::allow_all() } else { PermissionPolicy::default() };
        ToolRegistry::with_workspace(
            Workspace::new(root).unwrap().with_policy(PermissionEngine::new(policy)),
        )
    }

    #[tokio::test]
    async fn read_write_and_patch_are_bounded_and_preconditioned() {
        let root = test_root();
        let registry = registry(&root, true);
        let write = registry
            .dispatch(call(
                "write",
                serde_json::json!({"path": "src.txt", "content": "one\ntwo\n"}),
                1,
            ))
            .await;
        assert!(write.ok);
        let hash = blake3::hash(b"one\ntwo\n").to_hex().to_string();
        let read = registry
            .dispatch(call(
                "read",
                serde_json::json!({
                    "files": [{"path": "src.txt", "start_line": 2, "end_line": 2}]
                }),
                2,
            ))
            .await;
        assert_eq!(read.data.unwrap()["files"][0]["lines"][0]["text"], "two");
        let patch = registry
            .dispatch(call(
                "patch",
                serde_json::json!({
                    "files": [{
                        "path": "src.txt",
                        "expected_hash": hash,
                        "hunks": [{
                            "old_start": 2,
                            "old_count": 1,
                            "new_start": 2,
                            "new_count": 1,
                            "lines": ["-two", "+changed"]
                        }]
                    }]
                }),
                3,
            ))
            .await;
        assert!(patch.ok);
        assert_eq!(fs::read_to_string(root.join("src.txt")).unwrap(), "one\nchanged\n");
        assert!(
            fs::read_dir(&root)
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_name().to_string_lossy().starts_with(".aether-"))
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacing_a_file_preserves_unix_executable_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = test_root();
        let path = root.join("executable.sh");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let registry = registry(&root, true);
        let result = registry
            .dispatch(call(
                "write",
                serde_json::json!({"path": "executable.sh", "content": "new\n"}),
                20,
            ))
            .await;
        assert!(result.ok, "{}", result.output.as_str());
        assert_eq!(fs::metadata(path).unwrap().permissions().mode() & 0o111, 0o111);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancelled_blocking_tool_returns_typed_cancelled_result() {
        let root = test_root();
        for index in 0..256 {
            fs::write(root.join(format!("file-{index}.txt")), "needle\n").unwrap();
        }
        let registry = registry(&root, true);
        let flag = CancellationFlag::new();
        flag.cancel();
        let invocation =
            call("search", serde_json::json!({"patterns": ["needle"], "max_results": 256}), 21);
        let context = ToolExecutionContext::new(
            flag,
            ExecutionPermit::new(invocation.call_id.clone(), "search", PermissionClass::ReadOnly),
        );
        let result = registry.dispatch_with_context(invocation, context).await;
        assert!(!result.ok);
        assert_eq!(result.error.unwrap().code, "cancelled");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_tool_does_not_monopolize_current_thread_control_plane() {
        let root = test_root();
        let workspace = Workspace::new(&root).unwrap();
        let invocation = call("search", serde_json::json!({"patterns": ["needle"]}), 33);
        let context = ToolExecutionContext::new(
            CancellationFlag::new(),
            ExecutionPermit::new(invocation.call_id.clone(), "search", PermissionClass::ReadOnly),
        );
        let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let released = Arc::new(AtomicU64::new(0));
        let released_in_blocking = Arc::clone(&released);
        let blocking = common::spawn_blocking_tool(
            &workspace,
            invocation.call_id.clone(),
            &context,
            move |_, call_id, _| {
                let _ = started_sender.send(());
                if release_receiver.recv_timeout(Duration::from_millis(250)).is_ok() {
                    released_in_blocking.store(1, Ordering::Release);
                }
                ToolResult::failure(call_id, "fixture", "done", false, 1024)
            },
        );
        let control_plane = async move {
            let _ = started_receiver.await;
            let _ = release_sender.send(());
        };
        let (result, ()) = tokio::join!(blocking, control_plane);
        assert!(!result.ok);
        assert_eq!(released.load(Ordering::Acquire), 1);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn default_policy_rejects_mutation_and_path_escape() {
        let root = test_root();
        let registry = registry(&root, false);
        let write = registry
            .dispatch(call("write", serde_json::json!({"path": "blocked.txt", "content": "no"}), 4))
            .await;
        assert!(!write.ok);
        assert_eq!(write.error.unwrap().code, "permission_required");
        let escape = registry
            .dispatch(call("read", serde_json::json!({"files": [{"path": "../outside.txt"}]}), 5))
            .await;
        assert!(!escape.ok);
        assert_eq!(escape.error.unwrap().code, "path_escape");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn write_precondition_mismatch_does_not_replace_destination() {
        let root = test_root();
        fs::write(root.join("guarded.txt"), "original").unwrap();
        let registry = registry(&root, true);
        let result = registry
            .dispatch(call(
                "write",
                serde_json::json!({
                    "path": "guarded.txt",
                    "content": "replacement",
                    "expected_hash": blake3::hash(b"different").to_hex().to_string()
                }),
                8,
            ))
            .await;
        assert!(!result.ok);
        assert_eq!(result.error.unwrap().code, "invalid_input");
        assert_eq!(fs::read_to_string(root.join("guarded.txt")).unwrap(), "original");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn create_only_fails_without_replacing_existing_destination() {
        let root = test_root();
        fs::write(root.join("existing.txt"), "keep").unwrap();
        let registry = registry(&root, true);
        let result = registry
            .dispatch(call(
                "write",
                serde_json::json!({
                    "path": "existing.txt",
                    "content": "replace",
                    "create_only": true
                }),
                60,
            ))
            .await;
        assert!(!result.ok);
        assert_eq!(result.error.unwrap().code, "already_exists");
        assert_eq!(fs::read_to_string(root.join("existing.txt")).unwrap(), "keep");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_writes_to_one_destination_are_not_torn() {
        let root = test_root();
        let registry = Arc::new(registry(&root, true));
        let first = Arc::clone(&registry);
        let second = Arc::clone(&registry);
        let (left, right) = tokio::join!(
            first.dispatch(call(
                "write",
                serde_json::json!({"path": "same.txt", "content": "left\n"}),
                61,
            )),
            second.dispatch(call(
                "write",
                serde_json::json!({"path": "same.txt", "content": "right\n"}),
                62,
            )),
        );
        assert!(left.ok, "{}", left.output.as_str());
        assert!(right.ok, "{}", right.output.as_str());
        let content = fs::read_to_string(root.join("same.txt")).unwrap();
        assert!(content == "left\n" || content == "right\n");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_write_and_patch_to_one_destination_are_not_torn() {
        let root = test_root();
        fs::write(root.join("same.txt"), "old\n").unwrap();
        let registry = Arc::new(registry(&root, true));
        let hash = blake3::hash(b"old\n").to_hex().to_string();
        let writer = Arc::clone(&registry);
        let patcher = Arc::clone(&registry);
        let (write, patch) = tokio::join!(
            writer.dispatch(call(
                "write",
                serde_json::json!({"path": "same.txt", "content": "written\n"}),
                63,
            )),
            patcher.dispatch(call(
                "patch",
                serde_json::json!({
                    "files": [{
                        "path": "same.txt",
                        "expected_hash": hash,
                        "hunks": [{
                            "old_start": 1,
                            "old_count": 1,
                            "new_start": 1,
                            "new_count": 1,
                            "lines": ["-old", "+patched"]
                        }]
                    }]
                }),
                64,
            )),
        );
        assert!(write.ok || patch.ok);
        let content = fs::read_to_string(root.join("same.txt")).unwrap();
        assert!(content == "written\n" || content == "patched\n");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = test_root();
        let outside = root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), "secret").unwrap();
        symlink(&outside, root.join("link")).unwrap();
        let registry = registry(&root, false);
        let result = registry
            .dispatch(call("read", serde_json::json!({"files": [{"path": "link/secret.txt"}]}), 6))
            .await;
        assert!(!result.ok);
        assert_eq!(result.error.unwrap().code, "path_escape");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[tokio::test]
    async fn shell_output_has_a_hard_ceiling() {
        let root = test_root();
        let registry = registry(&root, true);
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_owned(), "echo abcdefgh".to_owned()])
        } else {
            ("printf", vec!["abcdefgh".to_owned()])
        };
        let result = registry
            .dispatch(call(
                "shell",
                serde_json::json!({
                    "program": program,
                    "args": args,
                    "max_output_bytes": 4
                }),
                7,
            ))
            .await;
        assert!(result.ok);
        assert!(result.output.as_str().len() <= 65_536);
        assert_eq!(result.data.unwrap()["stdout_truncated"], true);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn huge_shell_stdout_and_stderr_remain_bounded() {
        let root = test_root();
        let registry = registry(&root, true);
        let result = registry
            .dispatch(call(
                "shell",
                serde_json::json!({
                    "program": "sh",
                    "args": ["-c", "head -c 1000000 /dev/zero; head -c 1000000 /dev/zero >&2"],
                    "max_output_bytes": 65536
                }),
                29,
            ))
            .await;
        assert!(result.ok, "{}", result.output.as_str());
        assert!(result.output.as_str().len() <= 64 * 1024);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persistent_process_output_is_drained_and_bounded() {
        let root = test_root();
        let registry = Arc::new(registry(&root, true));
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_owned(), "for /L %i in (1,1,400000) do @echo x".to_owned()])
        } else {
            ("sh", vec!["-c".to_owned(), "yes x | head -c 400000".to_owned()])
        };
        let started = registry
            .dispatch(call(
                "process",
                serde_json::json!({"operation": "start", "program": program, "args": args}),
                22,
            ))
            .await;
        assert!(started.ok, "{}", started.output.as_str());
        let process_id = started.data.unwrap()["process_id"].as_u64().unwrap();
        for number in 0..100 {
            let status = registry
                .dispatch(call(
                    "process",
                    serde_json::json!({"operation": "status", "process_id": process_id}),
                    23 + number,
                ))
                .await;
            assert!(status.ok, "{}", status.output.as_str());
            if !status.data.unwrap()["running"].as_bool().unwrap_or(false) {
                break;
            }
            tokio::task::yield_now().await;
        }
        let mut dropped_bytes = 0;
        for number in 0..16 {
            let read = registry
                .dispatch(call(
                    "process",
                    serde_json::json!({
                        "operation": "read",
                        "process_id": process_id,
                        "max_bytes": 65536,
                        "timeout_ms": 1000
                    }),
                    123 + number,
                ))
                .await;
            assert!(read.ok, "{}", read.output.as_str());
            let data = read.data.unwrap();
            assert!(data["buffered_bytes"].as_u64().unwrap() <= 256 * 1024);
            dropped_bytes = data["dropped_bytes"].as_u64().unwrap();
            if dropped_bytes > 0 || data["eof"].as_bool().unwrap_or(false) {
                break;
            }
        }
        assert!(dropped_bytes > 0);
        let killed = registry
            .dispatch(call(
                "process",
                serde_json::json!({"operation": "kill", "process_id": process_id}),
                24,
            ))
            .await;
        assert!(killed.ok, "{}", killed.output.as_str());
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn slow_process_read_does_not_block_another_process_status() {
        let root = test_root();
        let registry = Arc::new(registry(&root, true));
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_owned(), "ping -n 4 127.0.0.1 >NUL".to_owned()])
        } else {
            ("sh", vec!["-c".to_owned(), "sleep 2".to_owned()])
        };
        let started = registry
            .dispatch(call(
                "process",
                serde_json::json!({"operation": "start", "program": program, "args": args}),
                25,
            ))
            .await;
        assert!(started.ok, "{}", started.output.as_str());
        let process_id = started.data.unwrap()["process_id"].as_u64().unwrap();
        let reader_registry = Arc::clone(&registry);
        let reader = tokio::spawn(async move {
            reader_registry
                .dispatch(call(
                    "process",
                    serde_json::json!({
                        "operation": "read",
                        "process_id": process_id,
                        "timeout_ms": 600
                    }),
                    26,
                ))
                .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let status = tokio::time::timeout(
            Duration::from_millis(150),
            registry.dispatch(call(
                "process",
                serde_json::json!({"operation": "status", "process_id": process_id}),
                27,
            )),
        )
        .await
        .expect("status must not wait on another process read");
        assert!(status.ok, "{}", status.output.as_str());
        let _ = reader.await;
        let _ = registry
            .dispatch(call(
                "process",
                serde_json::json!({"operation": "kill", "process_id": process_id}),
                28,
            ))
            .await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_persistent_process_read() {
        let root = test_root();
        let registry = Arc::new(registry(&root, true));
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_owned(), "ping -n 4 127.0.0.1 >NUL".to_owned()])
        } else {
            ("sh", vec!["-c".to_owned(), "sleep 2".to_owned()])
        };
        let started = registry
            .dispatch(call(
                "process",
                serde_json::json!({"operation": "start", "program": program, "args": args}),
                30,
            ))
            .await;
        assert!(started.ok, "{}", started.output.as_str());
        let process_id = started.data.unwrap()["process_id"].as_u64().unwrap();
        let flag = CancellationFlag::new();
        let invocation = call(
            "process",
            serde_json::json!({"operation": "read", "process_id": process_id, "timeout_ms": 600}),
            31,
        );
        let context = ToolExecutionContext::new(
            flag.clone(),
            ExecutionPermit::new(
                invocation.call_id.clone(),
                "process",
                PermissionClass::ProcessPersistent,
            ),
        );
        let reader_registry = Arc::clone(&registry);
        let reader = tokio::spawn(async move {
            reader_registry.dispatch_with_context(invocation, context).await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        flag.cancel();
        let result = reader.await.unwrap();
        assert!(!result.ok);
        assert_eq!(result.error.unwrap().code, "cancelled");
        let _ = registry
            .dispatch(call(
                "process",
                serde_json::json!({"operation": "kill", "process_id": process_id}),
                32,
            ))
            .await;
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn persistent_process_limit_is_typed_and_bounded() {
        let root = test_root();
        let registry = registry(&root, true);
        let (program, args) = if cfg!(windows) {
            ("cmd", vec!["/C".to_owned(), "ping -n 4 127.0.0.1 >NUL".to_owned()])
        } else {
            ("sh", vec!["-c".to_owned(), "sleep 2".to_owned()])
        };
        for number in 0..16 {
            let result = registry
                .dispatch(call(
                    "process",
                    serde_json::json!({
                        "operation": "start",
                        "program": program,
                        "args": args.clone()
                    }),
                    40 + number,
                ))
                .await;
            assert!(result.ok, "{}", result.output.as_str());
        }
        let rejected = registry
            .dispatch(call(
                "process",
                serde_json::json!({
                    "operation": "start",
                    "program": program,
                    "args": args.clone()
                }),
                56,
            ))
            .await;
        assert!(!rejected.ok);
        assert_eq!(rejected.error.unwrap().code, "resource_limit");
        registry.workspace().shutdown_processes().await;
        let _ = fs::remove_dir_all(root);
    }
}
