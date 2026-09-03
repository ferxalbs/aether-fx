use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
        .env_remove("AETHER_MODEL")
        .output()
        .unwrap()
}

fn run_piped(
    root: &Path,
    arguments: &[&str],
    state_dir: &Path,
    input: &[u8],
) -> std::process::Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_aether"))
        .args(arguments)
        .args(["--root", root.to_str().unwrap()])
        .env("AETHER_FX_STATE_DIR", state_dir)
        .env_remove("RAINY_API_KEY")
        .env_remove("AETHER_MODEL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

fn run_doctor_with_configuration(root: &Path, state_dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_aether"))
        .args(["doctor", "--root", root.to_str().unwrap()])
        .env("AETHER_FX_STATE_DIR", state_dir)
        .env("RAINY_API_KEY", "test-secret-value")
        .env("AETHER_MODEL", "rainy-test-model")
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
            assert!(stdout.contains("RAINY_API_KEY: not configured"), "{stdout}");
            assert!(stdout.contains("AETHER_MODEL: not configured"), "{stdout}");
            assert!(stdout.contains("provider endpoint: SDK default HTTPS endpoint"), "{stdout}");
            assert!(stdout.contains("update: supported"), "{stdout}");
            assert!(!stdout.contains("secret"), "{stdout}");
        }
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(state_root(&root));
    }
}

#[test]
fn version_reports_exact_package_version() {
    let root = temp_root("version");
    let output = run(&root, ["--version"].as_slice());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), concat!(env!("CARGO_PKG_VERSION"), "\n"));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn json_version_is_one_structured_machine_record() {
    let root = temp_root("json-version");
    let output = run(&root, ["--json", "--version"].as_slice());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["version"], env!("CARGO_PKG_VERSION"));
    assert!(!output.stdout.contains(&0x1b));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn update_help_documents_explicit_network_operation() {
    let root = temp_root("update-help");
    let output = run(&root, ["help", "update"].as_slice());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("aether update [--json]"), "{stdout}");
    assert!(stdout.contains("/update"), "{stdout}");
    assert!(!stdout.contains('\x1b'), "{stdout:?}");
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn json_help_lists_update_command() {
    let root = temp_root("json-help-update");
    let output = run(&root, ["--json", "help"].as_slice());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["commands"].as_array().unwrap().iter().any(|command| {
        command["name"] == "/update"
            && command["description"] == "Update AETHER Fx and leave the shell"
    }));
    assert!(!output.stdout.contains(&0x1b));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn doctor_json_reports_local_update_capability_without_network() {
    let root = temp_root("doctor-update");
    let output = run(&root, ["doctor", "--json"].as_slice());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["update"]["source"], "github.com/ferxalbs/aether-fx");
    assert_eq!(value["update"]["supported"], true);
    assert!(value["update"]["current_executable"].as_str().is_some_and(|path| !path.is_empty()));
    assert_eq!(value["update"]["reason"], serde_json::Value::Null);
    assert!(!root.join(".aether").exists());
    assert!(!root.join(".aether-fx").exists());
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn invalid_explicit_model_is_rejected_instead_of_falling_back() {
    let root = temp_root("invalid-model");
    let output = run(&root, ["--model", "model with spaces", "prompt"].as_slice());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--model"));
    assert!(output.stdout.is_empty());
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn doctor_reports_configuration_without_disclosing_credentials() {
    let root = temp_root("configured-doctor");
    let output = run_doctor_with_configuration(&root, &state_root(&root));
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("AETHER Fx "));
    assert!(stdout.contains("RAINY_API_KEY: configured"), "{stdout}");
    assert!(stdout.contains("AETHER_MODEL: configured"), "{stdout}");
    assert!(!stdout.contains("test-secret-value"), "{stdout}");
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
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
        assert!(stderr.contains("RAINY_API_KEY is not configured"), "{stderr}");
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

#[test]
fn piped_shell_has_no_banner_prompt_or_ansi_and_keeps_slash_commands_local() {
    let root = temp_root("piped-shell");
    let output = run_piped(&root, &[], &state_root(&root), b"/help\n/exit\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Interactive commands:"), "{stdout}");
    assert!(!stdout.contains('\x1b'), "{stdout:?}");
    assert!(!stdout.contains("›"), "{stdout:?}");
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn browser_status_is_offline_and_does_not_create_provider_state() {
    let root = temp_root("browser-status");
    let state = state_root(&root);
    let output = run_piped(&root, &[], &state, b"/browser status\n/exit\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Obscura: installed=no active=no healthy=no"), "{stdout}");
    assert!(!state.exists(), "browser status must not create the provider state root");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn browser_install_requires_interactive_consent_even_with_yolo() {
    let root = temp_root("browser-consent");
    let state = state_root(&root);
    let output = run_piped(&root, ["--yolo"].as_slice(), &state, b"/browser\n/exit\n");
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(combined.contains("explicit interactive consent"), "{combined}");
    assert!(!state.exists(), "declined/non-interactive install must not create provider state");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn piped_json_shell_emits_structured_local_help_without_decoration() {
    let root = temp_root("piped-json");
    let output = run_piped(&root, ["--json"].as_slice(), &state_root(&root), b"/help\n/exit\n");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains('\x1b'), "{stdout:?}");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(value["topic"], serde_json::Value::Null);
    assert!(value["commands"].as_array().is_some());
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn one_shot_execution_suppresses_identity_even_when_attached_to_a_tty_contract() {
    let root = temp_root("one-shot");
    let output = run(&root, ["--model", "test-model", "inspect"].as_slice());
    assert!(!output.status.success());
    assert!(output.stdout.is_empty(), "{}", String::from_utf8_lossy(&output.stdout));
    assert!(!output.stdout.contains(&0x1b), "{}", String::from_utf8_lossy(&output.stdout));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("workspace"));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state_root(&root));
}

#[test]
fn config_show_redacts_credential_helper_arguments() {
    let root = temp_root("config-redaction");
    let state = state_root(&root);
    fs::create_dir_all(&state).unwrap();
    fs::write(
        state.join("config.json"),
        serde_json::to_vec(&serde_json::json!({
            "default_model": "catalog-model",
            "auth": {"credential_helper": ["credential-tool", "secret-token"]}
        }))
        .unwrap(),
    )
    .unwrap();
    let output = run(&root, ["config", "show", "--json"].as_slice());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(!stdout.contains("secret-token"), "{stdout}");
    assert!(stdout.contains("credential_helper_configured"), "{stdout}");
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state);
}

#[cfg(any(unix, windows))]
#[test]
fn config_symlink_is_rejected_without_reading_the_target() {
    let root = temp_root("config-link");
    let state = state_root(&root);
    fs::create_dir_all(&state).unwrap();
    let target = temp_root("config-link-target").join("secret-config.json");
    fs::write(&target, br#"{"default_model":"secret-model"}"#).unwrap();
    let link = state.join("config.json");
    if try_file_symlink(&target, &link) {
        let output = run(&root, ["config", "show"].as_slice());
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("symbolic link"), "{stderr}");
        assert!(!String::from_utf8_lossy(&output.stdout).contains("secret-model"));
    }
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(target.parent().unwrap());
    let _ = fs::remove_dir_all(state);
}

#[cfg(unix)]
#[test]
fn config_edit_creates_private_state_with_a_direct_editor_invocation() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_root("config-edit");
    let state = state_root(&root);
    let output = Command::new(env!("CARGO_BIN_EXE_aether"))
        .args(["config", "edit", "--root", root.to_str().unwrap()])
        .env("AETHER_FX_STATE_DIR", &state)
        .env_remove("RAINY_API_KEY")
        .env_remove("AETHER_MODEL")
        .env("VISUAL", "/usr/bin/true")
        .env_remove("EDITOR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let config = state.join("config.json");
    assert!(config.is_file());
    assert_eq!(fs::metadata(&state).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&config).unwrap().permissions().mode() & 0o777, 0o600);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(state);
}
