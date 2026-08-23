use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_core::{ContextSnapshot, SessionId, SessionLine, SessionRecord, TurnId, TurnSnapshot};

fn temp_root(label: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let root = std::env::temp_dir().join(format!(
        "aether-cli-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

fn run(root: &Path, arguments: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aether"))
        .args(arguments)
        .args(["--root", root.to_str().unwrap()])
        .env_remove("RAINY_API_KEY")
        .output()
        .unwrap()
}

fn write_session(root: &Path, name: &str) {
    let session = SessionId::new(name).unwrap();
    let path = root.join(".aether/sessions").join(format!("{name}.jsonl"));
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let line = SessionLine::new(
        1,
        session,
        None,
        SessionRecord::Started {
            workspace_root: root.display().to_string(),
            model: Some("test-model".to_owned()),
        },
    );
    let turn_id = TurnId::new("turn-1").unwrap();
    let turn = SessionLine::new(
        2,
        SessionId::new(name).unwrap(),
        Some(turn_id.clone()),
        SessionRecord::turn_snapshot(TurnSnapshot {
            turn_id,
            steps: 1,
            cancelled: false,
            context: ContextSnapshot::new(
                root.display().to_string(),
                Some("test-model".to_owned()),
            ),
            continuation: None,
        }),
    );
    fs::write(
        path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&line).unwrap(),
            serde_json::to_string(&turn).unwrap()
        ),
    )
    .unwrap();
}

#[test]
fn informational_commands_create_no_session_storage() {
    for arguments in [["--help"].as_slice(), ["--version"].as_slice(), ["doctor"].as_slice()] {
        let root = temp_root("informational");
        let output = run(&root, arguments);
        assert!(output.status.success());
        assert!(!root.join(".aether").exists());
        let _ = fs::remove_dir_all(root);
    }
}

#[test]
fn sessions_lists_one_hundred_newest_first() {
    let root = temp_root("sessions-hundred");
    for number in 0..100 {
        write_session(&root, &format!("session-{number:03}"));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let output = run(&root, &["sessions"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 100);
    assert!(lines[0].starts_with("session-099\t"));
    assert!(lines[99].starts_with("session-000\t"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn resume_latest_skips_newer_corrupt_empty_and_unsupported_sessions() {
    for invalid in ["not-json\n", "", "{\"schema_version\":2}\n"] {
        let root = temp_root("resume-latest");
        write_session(&root, "older-valid");
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(root.join(".aether/sessions/newest-invalid.jsonl"), invalid).unwrap();

        let output = run(&root, &["resume", "--latest"]);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("skipped corrupt session: newest-invalid"), "{stderr}");
        assert!(stderr.contains("Rainy credentials are unavailable"), "{stderr}");
        assert!(!stderr.contains("session not found"), "{stderr}");
        let _ = fs::remove_dir_all(root);
    }
}

#[cfg(any(unix, windows))]
#[test]
fn sessions_does_not_follow_storage_directory_links() {
    for nested in [false, true] {
        let root = temp_root("session-link");
        let outside = temp_root("session-link-outside");
        write_session(&outside, "outside-session");
        if nested {
            fs::create_dir_all(root.join(".aether")).unwrap();
        }
        let link = if nested { root.join(".aether/sessions") } else { root.join(".aether") };
        #[cfg(unix)]
        let linked = std::os::unix::fs::symlink(outside.join(".aether/sessions"), &link).is_ok();
        #[cfg(windows)]
        let linked =
            std::os::windows::fs::symlink_dir(outside.join(".aether/sessions"), &link).is_ok();
        if !linked {
            let _ = fs::remove_dir_all(root);
            let _ = fs::remove_dir_all(outside);
            continue;
        }
        let output = run(&root, &["sessions"]);
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("outside-session"));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }
}
