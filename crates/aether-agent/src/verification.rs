//! Deterministic focused verification planning and process-intent classification.

use std::path::Path;

use aether_core::{CommandEffects, VerificationStep};

use crate::{ManifestKind, RepoMapSnapshot};

pub use aether_core::CommandClass as CommandIntent;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedCommand {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub scope: String,
}

impl PlannedCommand {
    pub fn display(&self) -> String {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .map(shell_word)
            .collect::<Vec<_>>()
            .join(" ")
    }

    pub fn workflow_step(&self, revision: u32) -> VerificationStep {
        VerificationStep::new(self.workflow_key(), &self.scope, revision)
    }

    pub(crate) fn workflow_key(&self) -> String {
        // The tool schema permits both an omitted/root cwd and the explicit `.` spelling. Treat
        // them as the same scope so a runtime-planned check can safely match a model request.
        if self.cwd.is_empty() || self.cwd == "." {
            self.display()
        } else {
            format!("[cwd={}] {}", shell_word(&self.cwd), self.display())
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VerificationPlan {
    pub commands: Vec<PlannedCommand>,
    pub unsupported: bool,
    /// Whether the change crosses a package boundary and needs a broader dependent check.
    pub requires_broader: bool,
}

/// Plan bounded checks ordered from the narrowest affected package to a safe fallback.
pub fn plan_verification(repo: &RepoMapSnapshot, modified: &[String]) -> VerificationPlan {
    let mut plan = VerificationPlan::default();
    for path in modified {
        let path = Path::new(path);
        let Some(manifest) = nearest_manifest(repo, path) else { continue };
        if !supports_path(manifest.kind, &manifest.path, path) {
            continue;
        }
        if requires_workspace_scope(repo, manifest, path) {
            plan.requires_broader = true;
        }
        let root = manifest.path.parent().unwrap_or_else(|| Path::new(""));
        match manifest.kind {
            ManifestKind::Cargo => plan_rust(&mut plan, repo, root, path),
            ManifestKind::Node => plan_node(&mut plan, root, path),
            ManifestKind::Python => plan_python(&mut plan, root, path),
            _ => {}
        }
    }
    if plan.commands.is_empty() {
        plan.unsupported = true;
        push_unique(&mut plan, command("git", &["diff", "--check"], "", "workspace diff"));
    }
    if plan.requires_broader {
        let broad = repo
            .manifests
            .iter()
            .find(|manifest| {
                manifest.kind == ManifestKind::Cargo && !manifest.workspace_members.is_empty()
            })
            .map(|manifest| manifest.path.as_path());
        if let Some(manifest) = broad {
            let workspace_change = modified.iter().any(|path| {
                let path = Path::new(path);
                path == manifest || path.ends_with("Cargo.lock")
            });
            let args = if workspace_change {
                vec!["test", "--workspace"]
            } else {
                vec!["check", "--workspace"]
            };
            let broad_command = command("cargo", &args, "", "workspace dependents");
            if !plan.commands.iter().any(|existing| {
                existing.display() == broad_command.display() && existing.cwd == broad_command.cwd
            }) {
                // Keep the cross-package check even when narrow package planning reaches its
                // bounded command limit.
                if plan.commands.len() >= aether_core::MAX_WORKFLOW_VERIFICATIONS {
                    plan.commands
                        .truncate(aether_core::MAX_WORKFLOW_VERIFICATIONS.saturating_sub(1));
                }
                push_unique(&mut plan, broad_command);
            }
        }
    }
    plan.commands.truncate(aether_core::MAX_WORKFLOW_VERIFICATIONS);
    plan
}

/// Classify only direct, well-known process invocations; shell interpreters remain unknown.
pub fn classify_command(program: &str, args: &[String]) -> CommandIntent {
    aether_core::classify_command(program, args)
}

/// Analyze one direct argv invocation for paths, packages, manifests, and scheduler effects.
pub fn command_effects(program: &str, args: &[String], cwd: &str) -> CommandEffects {
    aether_core::analyze_command(program, args, cwd)
}

fn plan_rust(plan: &mut VerificationPlan, repo: &RepoMapSnapshot, root: &Path, path: &Path) {
    let package = repo
        .packages
        .iter()
        .find(|package| package.root == root)
        .and_then(|package| package.name.as_deref());
    let cwd = display_root(root);
    let scope = package.unwrap_or_else(|| root.to_str().unwrap_or("workspace"));
    if path.starts_with(root.join("tests"))
        && let Some(stem) = path.file_stem().and_then(|stem| stem.to_str())
    {
        push_unique(plan, command_owned("cargo", vec!["test", "--test", stem], &cwd, scope));
    } else if let Some(package) = package {
        push_unique(plan, command_owned("cargo", vec!["test", "-p", package], "", scope));
    } else {
        push_unique(plan, command("cargo", &["test"], &cwd, scope));
    }
    if let Some(package) = package {
        push_unique(plan, command_owned("cargo", vec!["check", "-p", package], "", scope));
    } else {
        push_unique(plan, command("cargo", &["check"], &cwd, scope));
    }
}

fn plan_node(plan: &mut VerificationPlan, root: &Path, path: &Path) {
    let cwd = display_root(root);
    let scope = if cwd.is_empty() { "node package" } else { &cwd };
    if is_test_path(path) {
        push_unique(
            plan,
            command_owned("npm", vec!["test", "--", path.to_str().unwrap_or("")], &cwd, scope),
        );
    } else {
        push_unique(plan, command("npm", &["test"], &cwd, scope));
    }
}

fn plan_python(plan: &mut VerificationPlan, root: &Path, path: &Path) {
    let cwd = display_root(root);
    let scope = if cwd.is_empty() { "python package" } else { &cwd };
    if is_test_path(path) {
        push_unique(
            plan,
            command_owned("python", vec!["-m", "pytest", path.to_str().unwrap_or("")], &cwd, scope),
        );
    } else {
        push_unique(plan, command("python", &["-m", "pytest"], &cwd, scope));
    }
    push_unique(plan, command("python", &["-m", "compileall", "-q", "."], &cwd, scope));
}

fn nearest_manifest<'a>(repo: &'a RepoMapSnapshot, path: &Path) -> Option<&'a crate::ManifestInfo> {
    repo.manifests
        .iter()
        .filter(|manifest| {
            path.starts_with(manifest.path.parent().unwrap_or_else(|| Path::new("")))
        })
        .max_by_key(|manifest| manifest.path.components().count())
}

fn supports_path(kind: ManifestKind, manifest: &Path, path: &Path) -> bool {
    if path == manifest {
        return true;
    }
    let extension = path.extension().and_then(|extension| extension.to_str());
    match kind {
        ManifestKind::Cargo => {
            extension == Some("rs") || path.ends_with("Cargo.lock") || path.ends_with("Cargo.toml")
        }
        ManifestKind::Node => {
            matches!(extension, Some("js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx"))
                || matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("package-lock.json" | "pnpm-lock.yaml" | "yarn.lock")
                )
        }
        ManifestKind::Python => {
            extension == Some("py")
                || matches!(
                    path.file_name().and_then(|name| name.to_str()),
                    Some("uv.lock" | "poetry.lock" | "requirements.txt")
                )
        }
        _ => false,
    }
}

fn requires_workspace_scope(
    repo: &RepoMapSnapshot,
    manifest: &crate::ManifestInfo,
    path: &Path,
) -> bool {
    if manifest.kind != ManifestKind::Cargo
        || repo.packages.len() < 2
        || repo.workspace_members.is_empty()
    {
        return false;
    }
    let Some(workspace_manifest) = repo.manifests.iter().find(|candidate| {
        candidate.kind == ManifestKind::Cargo && !candidate.workspace_members.is_empty()
    }) else {
        return false;
    };
    let workspace_root = workspace_manifest.path.parent().unwrap_or_else(|| Path::new(""));
    let workspace_file =
        path == workspace_manifest.path || path == workspace_root.join("Cargo.lock");
    let public_api = repo.packages.iter().any(|package| package.manifest == manifest.path)
        && matches!(path.file_name().and_then(|name| name.to_str()), Some("lib.rs" | "mod.rs"));
    workspace_file || public_api
}

fn is_test_path(path: &Path) -> bool {
    path.components()
        .any(|part| matches!(part.as_os_str().to_str(), Some("test" | "tests" | "__tests__")))
        || path.file_name().and_then(|name| name.to_str()).is_some_and(|name| {
            name.starts_with("test_") || name.contains(".test.") || name.contains(".spec.")
        })
}

fn command(program: &str, args: &[&str], cwd: &str, scope: &str) -> PlannedCommand {
    command_owned(program, args.to_vec(), cwd, scope)
}

fn command_owned(program: &str, args: Vec<&str>, cwd: &str, scope: &str) -> PlannedCommand {
    PlannedCommand {
        program: program.to_owned(),
        args: args.into_iter().map(str::to_owned).collect(),
        cwd: cwd.to_owned(),
        scope: scope.to_owned(),
    }
}

fn push_unique(plan: &mut VerificationPlan, command: PlannedCommand) {
    if !plan
        .commands
        .iter()
        .any(|existing| existing.display() == command.display() && existing.cwd == command.cwd)
    {
        plan.commands.push(command);
    }
}

fn display_root(root: &Path) -> String {
    if root.as_os_str().is_empty() { String::new() } else { root.to_string_lossy().into_owned() }
}

fn shell_word(word: &str) -> String {
    if !word.is_empty()
        && word.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"-_=./:".contains(&byte))
    {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ManifestInfo, ManifestStatus, PackageInfo, RepoMap, RepoMapSnapshot, WorkspaceMember,
    };
    use std::path::PathBuf;

    fn repo(kind: ManifestKind, manifest: &str, package: Option<&str>) -> RepoMapSnapshot {
        RepoMapSnapshot {
            root: PathBuf::from("."),
            tracked_file_count: 0,
            tracked_files: vec![],
            manifests: vec![ManifestInfo {
                path: manifest.into(),
                kind,
                package_name: package.map(str::to_owned),
                workspace_members: vec![],
                status: ManifestStatus::Parsed,
                truncated: false,
            }],
            packages: vec![PackageInfo {
                name: package.map(str::to_owned),
                root: Path::new(manifest).parent().unwrap().into(),
                manifest: manifest.into(),
            }],
            workspace_members: vec![],
            source_roots: vec![],
            test_paths: vec![],
            documentation: vec![],
            instructions: vec![],
            truncated: false,
        }
    }

    #[test]
    fn targeted_rust_test_precedes_package_check() {
        let plan = plan_verification(
            &repo(ManifestKind::Cargo, "crates/api/Cargo.toml", Some("api")),
            &["crates/api/tests/login.rs".into()],
        );
        assert_eq!(plan.commands[0].display(), "cargo test --test login");
        assert_eq!(plan.commands[1].display(), "cargo check -p api");
    }

    #[test]
    fn package_scoped_rust_checks_are_deduplicated() {
        let repo = repo(ManifestKind::Cargo, "crates/api/Cargo.toml", Some("api"));
        let plan =
            plan_verification(&repo, &["crates/api/src/a.rs".into(), "crates/api/src/b.rs".into()]);
        assert_eq!(plan.commands.len(), 2);
        assert_eq!(plan.commands[0].display(), "cargo test -p api");
    }

    #[test]
    fn cargo_manifest_changes_keep_package_scope() {
        let repo = repo(ManifestKind::Cargo, "crates/api/Cargo.toml", Some("api"));
        let plan = plan_verification(&repo, &["crates/api/Cargo.toml".into()]);
        assert!(!plan.unsupported);
        assert_eq!(plan.commands[0].display(), "cargo test -p api");
        assert_eq!(plan.commands[1].display(), "cargo check -p api");
    }

    #[test]
    fn public_rust_api_changes_add_workspace_dependent_check() {
        let mut repo = repo(ManifestKind::Cargo, "crates/api/Cargo.toml", Some("api"));
        repo.manifests.push(ManifestInfo {
            path: "Cargo.toml".into(),
            kind: ManifestKind::Cargo,
            package_name: None,
            workspace_members: vec!["crates/*".to_owned()],
            status: ManifestStatus::Parsed,
            truncated: false,
        });
        repo.packages.push(PackageInfo {
            name: Some("app".to_owned()),
            root: "crates/app".into(),
            manifest: "crates/app/Cargo.toml".into(),
        });
        repo.workspace_members.push(WorkspaceMember {
            declared_by: "Cargo.toml".into(),
            pattern: "crates/*".to_owned(),
            manifest: Some("crates/api/Cargo.toml".into()),
        });
        let plan = plan_verification(&repo, &["crates/api/src/lib.rs".to_owned()]);
        assert!(plan.requires_broader);
        assert_eq!(plan.commands.last().unwrap().display(), "cargo check --workspace");
        assert_eq!(plan.commands.last().unwrap().scope, "workspace dependents");
    }

    #[test]
    fn workspace_fixture_public_change_escalates_from_package_to_dependents() {
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures/workspace-task");
        let snapshot = RepoMap::new(&root).snapshot().expect("workspace fixture map");
        let plan = plan_verification(&snapshot, &["crates/codec/src/lib.rs".to_owned()]);
        assert!(plan.requires_broader);
        assert!(plan.commands.iter().any(|command| command.display() == "cargo check --workspace"));
    }

    #[test]
    fn node_and_python_test_files_get_targeted_tests() {
        let node = plan_verification(
            &repo(ManifestKind::Node, "web/package.json", Some("web")),
            &["web/src/a.test.ts".into()],
        );
        assert!(node.commands[0].display().contains("web/src/a.test.ts"));
        let python = plan_verification(
            &repo(ManifestKind::Python, "py/pyproject.toml", Some("py")),
            &["py/tests/test_api.py".into()],
        );
        assert!(python.commands[0].display().contains("pytest py/tests/test_api.py"));
    }

    #[test]
    fn unsupported_repository_uses_safe_fallback() {
        let plan = plan_verification(&repo(ManifestKind::Go, "go.mod", None), &["main.go".into()]);
        assert!(plan.unsupported);
        assert_eq!(plan.commands[0].display(), "git diff --check");
    }

    #[test]
    fn root_cwd_spellings_share_a_verification_key() {
        let omitted = PlannedCommand {
            program: "git".to_owned(),
            args: vec!["diff".to_owned(), "--check".to_owned()],
            cwd: String::new(),
            scope: "workspace diff".to_owned(),
        };
        let explicit = PlannedCommand { cwd: ".".to_owned(), ..omitted.clone() };
        assert_eq!(omitted.workflow_key(), explicit.workflow_key());
    }

    #[test]
    fn classifies_verification_mutation_and_ambiguous_shells() {
        assert_eq!(
            classify_command("cargo", &["test".into(), "-p".into(), "api".into()]),
            CommandIntent::Verification
        );
        assert_eq!(classify_command("cargo", &["fmt".into()]), CommandIntent::Formatting);
        assert_eq!(
            classify_command("bash", &["-c".into(), "cargo test".into()]),
            CommandIntent::Unknown
        );
        assert_eq!(
            classify_command("npm", &["install".into()]),
            CommandIntent::DependencyManagement
        );
    }
}
