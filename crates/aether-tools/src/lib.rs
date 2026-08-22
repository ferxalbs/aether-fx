#![deny(unsafe_code)]

//! The exact nine model-visible local tools.

mod common;
mod find;
mod git;
mod list;
mod patch;
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
pub use process::{ProcessInput, ProcessOperation, ProcessOutput};
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
        sync::atomic::{AtomicU64, Ordering},
    };

    use aether_core::{PermissionEngine, PermissionPolicy, ToolCallId, ToolInvocation};

    use super::{ToolRegistry, Workspace};

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
}
