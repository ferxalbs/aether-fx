use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_agent::SessionStore;
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

fn state_root(root: &Path) -> PathBuf {
    root.with_file_name(format!("{}-state", root.file_name().unwrap().to_string_lossy()))
}

fn run(root: &Path, arguments: &[&str]) -> std::process::Output {
    run_with_state(root, arguments, &state_root(root))
}

fn run_with_state(root: &Path, arguments: &[&str], state_dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aether"))
        .args(arguments)
        .args(["--root", root.to_str().unwrap()])
        .env("AETHER_FX_STATE_DIR", state_dir)
        .env_remove("RAINY_API_KEY")
        .output()
        .unwrap()
}

fn assert_no_workspace_state(root: &Path) {
    assert!(!root.join("workspaces").exists());
    assert!(!root.join(".aether").exists());
    assert!(!root.join(".aether-fx").exists());
}

fn state_workspace_dir(root: &Path) -> PathBuf {
    state_root(root).join("workspaces").join(SessionStore::workspace_id(root).unwrap())
}

fn session_path(root: &Path, name: &str) -> PathBuf {
    state_workspace_dir(root).join("sessions").join(format!("{name}.jsonl"))
}

fn native_path_hex(path: &Path) -> String {
    #[cfg(unix)]
    let bytes = {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    };
    #[cfg(windows)]
    let bytes = {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().flat_map(u16::to_ne_bytes).collect::<Vec<_>>()
    };
    #[cfg(not(any(unix, windows)))]
    let bytes = path.to_str().unwrap().as_bytes().to_vec();

    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    result
}

fn write_workspace_metadata(root: &Path) {
    let canonical = fs::canonicalize(root).unwrap();
    let path = state_workspace_dir(root).join("workspace.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    if !path.exists() {
        let metadata = serde_json::json!({
            "version": 1,
            "workspace": canonical.to_str().unwrap(),
            "workspace_native": native_path_hex(&canonical),
        });
        fs::write(path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    }
}

fn write_session(root: &Path, name: &str) {
    let path = session_path(root, name);
    write_session_at(root, name, &path);
}

fn write_session_at(root: &Path, name: &str, path: &Path) {
    let session = SessionId::new(name).unwrap();
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    write_workspace_metadata(root);
    let line = SessionLine::new(
        1,
        session.clone(),
        None,
        SessionRecord::Started {
            workspace_root: root.to_str().unwrap().to_owned(),
            model: Some("test-model".to_owned()),
        },
    );
    let turn_id = TurnId::new("turn-1").unwrap();
    let turn = SessionLine::new(
        2,
        session,
        Some(turn_id.clone()),
        SessionRecord::turn_snapshot(TurnSnapshot {
            turn_id,
            steps: 1,
            cancelled: false,
            context: ContextSnapshot::new(
                root.to_str().unwrap().to_owned(),
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

fn try_dir_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(target, link).is_ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

fn remove_dir_link(path: &Path) {
    #[cfg(unix)]
    let _ = fs::remove_file(path);
    #[cfg(windows)]
    let _ = fs::remove_dir(path);
    #[cfg(not(any(unix, windows)))]
    let _ = path;
}

fn try_file_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (target, link);
        false
    }
}

#[test]
fn informational_commands_report_os_state_without_touching_workspace() {
    for arguments in [["--help"].as_slice(), ["--version"].as_slice(), ["doctor"].as_slice()] {
        let root = temp_root("informational");
        let output = run(&root, arguments);
        assert!(output.status.success());
        assert!(!root.join(".aether").exists());
        assert!(!root.join(".aether-fx").exists());
        let stdout = String::from_utf8_lossy(&output.stdout);
        if arguments == ["--help"] {
            assert!(stdout.contains("OS application-state storage"), "{stdout}");
        }
        if arguments == ["doctor"] {
            assert!(stdout.contains("workspace: "), "{stdout}");
            assert!(stdout.contains("state: "), "{stdout}");
            assert!(stdout.contains("sessions: "), "{stdout}");
            assert!(!stdout.contains("RAINY_API_KEY"), "{stdout}");
        }
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(state_root(&root));
    }
}

#[test]
fn state_override_writes_only_outside_workspace() {
    let root = temp_root("state-override");
    write_session(&root, "outside-only");
    let listed = run(&root, ["sessions"].as_slice());
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    assert!(String::from_utf8_lossy(&listed.stdout).contains("outside-only"));
    assert!(!root.join(".aether").exists());
    assert!(!root.join(".aether-fx").exists());
    assert!(state_root(&root).join("workspaces").exists());
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn relative_state_override_is_rejected_before_workspace_state_creation() {
    let root = temp_root("relative-state-root");
    let output = run_with_state(&root, ["sessions"].as_slice(), Path::new("relative-state"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must be absolute"));
    assert_no_workspace_state(&root);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn traversal_state_override_is_rejected_before_workspace_state_creation() {
    let root = temp_root("traversal-state-root");
    let override_path = root.join("outside").join("..").join("state");
    let output = run_with_state(&root, ["sessions"].as_slice(), &override_path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must not contain . or .."));
    assert_no_workspace_state(&root);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn workspace_or_nested_state_override_is_rejected_before_creation() {
    let root = temp_root("nested-state-root");
    let nested = root.join("state");
    for state_dir in [&root, &nested] {
        let output = run_with_state(&root, ["sessions"].as_slice(), state_dir);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("outside the workspace"));
        assert_no_workspace_state(&root);
    }
    let _ = fs::remove_dir_all(&root);
}

#[cfg(any(unix, windows))]
#[test]
fn intermediate_state_symlink_into_workspace_is_rejected_before_creation() {
    let root = temp_root("intermediate-state-link");
    let parent = temp_root("intermediate-state-parent");
    let linked = parent.join("redirect");
    if !try_dir_symlink(&root, &linked) {
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&parent);
        return;
    }
    let output = run_with_state(&root, ["sessions"].as_slice(), &linked.join("state"));
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("outside the workspace"));
    assert_no_workspace_state(&root);
    remove_dir_link(&linked);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&parent);
}

#[cfg(any(unix, windows))]
#[test]
fn state_root_links_are_rejected_without_following_them() {
    let root = temp_root("state-root-link");
    let outside = temp_root("state-root-link-outside");
    if !try_dir_symlink(&outside, &state_root(&root)) {
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        return;
    }
    let output = run(&root, ["sessions"].as_slice());
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("symbolic link") || stderr.contains("reparse"), "{stderr}");
    assert!(!outside.join("workspaces").exists());
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
    remove_dir_link(&state_root(&root));
}

#[test]
fn legacy_workspace_sessions_are_ignored() {
    let root = temp_root("legacy-ignored");
    let legacy_aether = root.join(".aether/sessions/legacy-aether.jsonl");
    let legacy_product = root.join(".aether-fx/sessions/legacy-product.jsonl");
    fs::create_dir_all(legacy_aether.parent().unwrap()).unwrap();
    fs::create_dir_all(legacy_product.parent().unwrap()).unwrap();
    fs::write(&legacy_aether, "valid legacy data\n").unwrap();
    fs::write(&legacy_product, "valid legacy data\n").unwrap();

    let listed = run(&root, ["sessions"].as_slice());
    assert!(listed.status.success(), "{}", String::from_utf8_lossy(&listed.stderr));
    let stdout = String::from_utf8_lossy(&listed.stdout);
    assert!(!stdout.contains("legacy-aether"));
    assert!(!stdout.contains("legacy-product"));

    let resumed = run(&root, ["resume", "--latest"].as_slice());
    assert!(!resumed.status.success());
    let stderr = String::from_utf8_lossy(&resumed.stderr);
    assert!(stderr.contains("session not found"), "{stderr}");
    assert!(!stderr.contains("legacy-aether"));
    assert!(!stderr.contains("legacy-product"));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn sessions_are_scoped_to_the_canonical_workspace_bucket() {
    let workspace_a = temp_root("scope-a");
    let workspace_b = temp_root("scope-b");
    write_session(&workspace_a, "only-a");
    write_session(&workspace_b, "only-b");

    let listed_a = run(&workspace_a, ["sessions"].as_slice());
    let listed_b = run(&workspace_b, ["sessions"].as_slice());
    assert!(listed_a.status.success());
    assert!(listed_b.status.success());
    assert!(String::from_utf8_lossy(&listed_a.stdout).contains("only-a"));
    assert!(!String::from_utf8_lossy(&listed_a.stdout).contains("only-b"));
    assert!(String::from_utf8_lossy(&listed_b.stdout).contains("only-b"));
    assert!(!String::from_utf8_lossy(&listed_b.stdout).contains("only-a"));

    let _ = fs::remove_dir_all(&workspace_a);
    let _ = fs::remove_dir_all(&workspace_b);
    let _ = fs::remove_dir_all(state_root(&workspace_a));
    let _ = fs::remove_dir_all(state_root(&workspace_b));
}

#[test]
fn sessions_lists_one_hundred_newest_first() {
    let root = temp_root("sessions-hundred");
    for number in 0..100 {
        write_session(&root, &format!("session-{number:03}"));
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let output = run(&root, ["sessions"].as_slice());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 100);
    assert!(lines[0].starts_with("session-099\t"));
    assert!(lines[99].starts_with("session-000\t"));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn resume_latest_skips_newer_corrupt_empty_and_unsupported_sessions() {
    for invalid in ["not-json\n", "", "{\"schema_version\":2}\n"] {
        let root = temp_root("resume-latest");
        write_session(&root, "older-valid");
        std::thread::sleep(std::time::Duration::from_millis(5));
        fs::write(session_path(&root, "newest-invalid"), invalid).unwrap();

        let output = run(&root, ["resume", "--latest"].as_slice());
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("skipped corrupt session: newest-invalid"), "{stderr}");
        assert!(stderr.contains("Rainy credentials are unavailable"), "{stderr}");
        assert!(!stderr.contains("session not found"), "{stderr}");
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(state_root(&root));
    }
}

#[cfg(any(unix, windows))]
#[test]
fn state_directory_links_are_rejected_without_following_them() {
    for nested in [false, true] {
        let root = temp_root("state-link");
        let outside = temp_root("state-link-outside");
        let bucket = state_workspace_dir(&root);
        if nested {
            fs::create_dir_all(&bucket).unwrap();
        } else {
            fs::create_dir_all(bucket.parent().unwrap()).unwrap();
        }
        let link = if nested { bucket.join("sessions") } else { bucket };
        if !try_dir_symlink(&outside, &link) {
            let _ = fs::remove_dir_all(&root);
            let _ = fs::remove_dir_all(&outside);
            let _ = fs::remove_dir_all(state_root(&root));
            continue;
        }
        let output = run(&root, ["sessions"].as_slice());
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("outside"));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
        let _ = fs::remove_dir_all(state_root(&root));
    }
}

#[cfg(any(unix, windows))]
#[test]
fn state_session_file_links_are_rejected() {
    let root = temp_root("state-file-link");
    let outside = temp_root("state-file-link-outside");
    write_session(&outside, "outside-session");
    let link = session_path(&root, "outside-session");
    let target = session_path(&outside, "outside-session");
    fs::create_dir_all(link.parent().unwrap()).unwrap();
    if try_file_symlink(&target, &link) {
        let output = run(&root, ["sessions"].as_slice());
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("outside-session"));
    }
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&outside);
    let _ = fs::remove_dir_all(state_root(&root));
    let _ = fs::remove_dir_all(state_root(&outside));
}
