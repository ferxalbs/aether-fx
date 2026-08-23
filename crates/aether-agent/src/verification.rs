//! Deterministic focused verification planning and process-intent classification.

use std::path::Path;

use aether_core::VerificationStep;

use crate::{ManifestKind, RepoMapSnapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandIntent {
    Verification,
    Mutation,
    Unknown,
}

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
        if self.cwd.is_empty() {
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
    plan.commands.truncate(aether_core::MAX_WORKFLOW_VERIFICATIONS);
    plan
}

/// Classify only direct, well-known process invocations; shell interpreters remain unknown.
pub fn classify_command(program: &str, args: &[String]) -> CommandIntent {
    let program = Path::new(program).file_name().and_then(|p| p.to_str()).unwrap_or(program);
    if matches!(program, "sh" | "bash" | "zsh" | "cmd" | "powershell" | "pwsh") {
        return CommandIntent::Unknown;
    }
    match program {
        "cargo" => classify_cargo(args),
        "rustc" | "clippy-driver" | "pytest" | "mypy" | "ruff" | "tsc" => {
            CommandIntent::Verification
        }
        "python" | "python3" => classify_python(args),
        "npm" | "pnpm" | "yarn" => classify_node(args),
        "git" if args.first().is_some_and(|arg| matches!(arg.as_str(), "diff" | "status")) => {
            CommandIntent::Verification
        }
        _ => CommandIntent::Unknown,
    }
}

fn classify_cargo(args: &[String]) -> CommandIntent {
    match args.iter().find(|arg| !arg.starts_with('-')).map(String::as_str) {
        Some("check" | "test" | "clippy" | "build" | "bench") => CommandIntent::Verification,
        Some("fmt") if args.iter().any(|arg| arg == "--check") => CommandIntent::Verification,
        Some("fmt" | "fix" | "install" | "update" | "add" | "remove") => CommandIntent::Mutation,
        _ => CommandIntent::Unknown,
    }
}

fn classify_python(args: &[String]) -> CommandIntent {
    match args.first().map(String::as_str) {
        Some("-m") => match args.get(1).map(String::as_str) {
            Some("pytest" | "unittest" | "compileall" | "mypy" | "ruff") => {
                CommandIntent::Verification
            }
            Some("pip") => CommandIntent::Mutation,
            _ => CommandIntent::Unknown,
        },
        _ => CommandIntent::Unknown,
    }
}

fn classify_node(args: &[String]) -> CommandIntent {
    match args.first().map(String::as_str) {
        Some("test" | "lint" | "typecheck") => CommandIntent::Verification,
        Some("run")
            if args.get(1).is_some_and(|arg| {
                matches!(arg.as_str(), "test" | "lint" | "typecheck" | "check" | "build")
            }) =>
        {
            CommandIntent::Verification
        }
        Some("install" | "add" | "remove" | "update" | "uninstall" | "ci") => {
            CommandIntent::Mutation
        }
        _ => CommandIntent::Unknown,
    }
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
        ManifestKind::Cargo => extension == Some("rs") || path.ends_with("Cargo.lock"),
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
    use crate::{ManifestInfo, ManifestStatus, PackageInfo, RepoMapSnapshot};
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
    fn classifies_verification_mutation_and_ambiguous_shells() {
        assert_eq!(
            classify_command("cargo", &["test".into(), "-p".into(), "api".into()]),
            CommandIntent::Verification
        );
        assert_eq!(classify_command("cargo", &["fmt".into()]), CommandIntent::Mutation);
        assert_eq!(
            classify_command("bash", &["-c".into(), "cargo test".into()]),
            CommandIntent::Unknown
        );
        assert_eq!(classify_command("npm", &["install".into()]), CommandIntent::Mutation);
    }
}
