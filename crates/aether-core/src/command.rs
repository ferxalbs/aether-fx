//! Bounded, deterministic intelligence for direct-argv commands.
//!
//! This module intentionally does not parse shell syntax or execute anything. It recognizes a
//! small set of common tools and returns conservative effects for scheduling and workflow state.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{ToolEffect, ToolFootprint, ToolResource};

/// Maximum argv elements accepted by command intelligence.
pub const MAX_COMMAND_ARGS: usize = 256;
/// Maximum UTF-8 bytes scanned from one direct command.
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;
/// Maximum paths retained in one command effect record.
pub const MAX_COMMAND_PATHS: usize = 32;
/// Maximum packages retained in one command effect record.
pub const MAX_COMMAND_PACKAGES: usize = 16;
/// Maximum bytes retained for one command effect field.
pub const MAX_COMMAND_FIELD_BYTES: usize = 256;

/// Coarse deterministic intent for a direct command invocation.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    Inspection,
    Search,
    Verification,
    Build,
    Formatting,
    Mutation,
    DependencyManagement,
    GitRead,
    GitMutation,
    #[default]
    Unknown,
}

impl CommandClass {
    /// Return the stable compact spelling used by diagnostics and model context.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Inspection => "inspection",
            Self::Search => "search",
            Self::Verification => "verification",
            Self::Build => "build",
            Self::Formatting => "formatting",
            Self::Mutation => "mutation",
            Self::DependencyManagement => "dependency_management",
            Self::GitRead => "git_read",
            Self::GitMutation => "git_mutation",
            Self::Unknown => "unknown",
        }
    }

    /// Whether the command is a safe read-only observation for scheduling and caching.
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Inspection | Self::Search | Self::GitRead)
    }

    /// Whether the command is a verification attempt.
    pub const fn is_verification(self) -> bool {
        matches!(self, Self::Verification)
    }

    /// Whether the command changes source, dependency, Git, or other workspace state.
    pub const fn is_mutation(self) -> bool {
        matches!(
            self,
            Self::Formatting | Self::Mutation | Self::DependencyManagement | Self::GitMutation
        )
    }
}

/// Bounded effects inferred from one direct command argv.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandEffects {
    pub class: CommandClass,
    pub paths: Vec<String>,
    pub packages: Vec<String>,
    pub manifests: Vec<String>,
    pub verification_scope: Option<String>,
    /// True when the command writes build artifacts such as `target/`.
    pub writes_artifacts: bool,
    /// True when source, dependency, Git, or other workspace state may change.
    pub writes_workspace: bool,
    /// True when the effect record is incomplete and must be scheduled exclusively.
    pub uncertain: bool,
}

impl CommandEffects {
    /// Return whether this command changes state relevant to the coding workflow.
    pub const fn mutates_workspace(&self) -> bool {
        self.writes_workspace || self.writes_artifacts
    }

    /// Return whether context and verification state should be invalidated after success.
    pub const fn invalidates_context(&self) -> bool {
        self.writes_workspace || self.uncertain
    }

    /// Return a conservative scheduler footprint without executing the command.
    pub fn footprint(&self) -> ToolFootprint {
        if self.uncertain || self.class == CommandClass::Unknown {
            return ToolFootprint::unknown();
        }

        let mut effects = Vec::new();
        let mut add = |effect: ToolEffect| {
            if effects.len() < crate::MAX_TOOL_FOOTPRINT_RESOURCES {
                effects.push(effect);
            }
        };

        if self.class.is_read_only() {
            if self.paths.is_empty() && self.manifests.is_empty() {
                add(ToolEffect::Read(ToolResource::Workspace));
            } else {
                for path in self.paths.iter().chain(self.manifests.iter()) {
                    add(ToolEffect::Read(ToolResource::WorkspacePath(path.clone())));
                }
            }
        } else if self.writes_workspace {
            if self.paths.is_empty() && self.manifests.is_empty() {
                add(ToolEffect::Write(ToolResource::Workspace));
            } else {
                for path in self.paths.iter().chain(self.manifests.iter()) {
                    add(ToolEffect::Write(ToolResource::WorkspacePath(path.clone())));
                }
            }
        } else if self.writes_artifacts {
            let artifact = self.paths.first().cloned().unwrap_or_else(|| "target".to_owned());
            add(ToolEffect::Write(ToolResource::WorkspacePath(artifact.clone())));
            for path in self.paths.iter().chain(self.manifests.iter()) {
                if path != &artifact {
                    add(ToolEffect::Read(ToolResource::WorkspacePath(path.clone())));
                }
            }
        } else {
            if self.paths.is_empty() && self.manifests.is_empty() {
                add(ToolEffect::Read(ToolResource::Workspace));
            } else {
                for path in self.paths.iter().chain(self.manifests.iter()) {
                    add(ToolEffect::Read(ToolResource::WorkspacePath(path.clone())));
                }
            }
        }

        ToolFootprint::from_effects(effects)
    }
}

/// Classify a direct command without allocating or executing it.
pub fn classify_command(program: &str, args: &[String]) -> CommandClass {
    if !bounded_argv(program, args) || has_shell_syntax(args) {
        return CommandClass::Unknown;
    }
    classify_bounded(program, args)
}

/// Extract bounded effects from a direct command argv and cwd.
pub fn analyze_command(program: &str, args: &[String], cwd: &str) -> CommandEffects {
    let class = classify_command(program, args);
    let mut effects = CommandEffects { class, ..CommandEffects::default() };
    if class == CommandClass::Unknown {
        effects.uncertain = true;
        return effects;
    }
    if cwd.len() > MAX_COMMAND_FIELD_BYTES
        || unsafe_path_token(cwd)
        || args.iter().any(|arg| unsafe_path_token(arg))
    {
        effects.uncertain = true;
    }

    let command = base_name(program);
    match command {
        "cargo" => analyze_cargo(&mut effects, args, cwd),
        "git" => analyze_git(&mut effects, args, cwd),
        "rg" | "grep" | "egrep" | "fgrep" => analyze_search(&mut effects, args, cwd),
        "find" => analyze_find(&mut effects, args, cwd),
        "ls" | "dir" | "pwd" | "stat" | "head" | "tail" | "cat" | "wc" | "file" | "du" => {
            analyze_paths(&mut effects, args, cwd)
        }
        "npm" | "pnpm" | "yarn" => analyze_node(&mut effects, command, args, cwd),
        "python" | "python3" | "pytest" | "mypy" | "pip" | "pip3" | "uv" | "poetry" => {
            analyze_python(&mut effects, command, args, cwd)
        }
        "ruff" | "black" => analyze_formatter_paths(&mut effects, args, cwd),
        "rustfmt" => analyze_rustfmt(&mut effects, args, cwd),
        "rustc" | "clippy-driver" | "tsc" => analyze_compiler(&mut effects, command, args, cwd),
        _ => {}
    }
    if matches!(command, "rustc" | "clippy-driver" | "tsc") {
        effects.writes_artifacts = true;
    }
    if effects.writes_artifacts && !effects.paths.iter().any(|path| path == "target") {
        add_path(&mut effects.paths, "target");
    }
    effects
}

fn classify_bounded(program: &str, args: &[String]) -> CommandClass {
    match base_name(program) {
        "cargo" => classify_cargo(args),
        "rustc" | "clippy-driver" | "pytest" | "mypy" | "tsc" => CommandClass::Verification,
        "rustfmt" => {
            if args.iter().any(|arg| arg == "--check") {
                CommandClass::Verification
            } else {
                CommandClass::Formatting
            }
        }
        "python" | "python3" => classify_python(args),
        "npm" | "pnpm" | "yarn" => classify_node(args),
        "pip" | "pip3" | "uv" | "poetry" => classify_dependency_manager(args),
        "ruff" => classify_formatter(args),
        "black" => {
            if args.iter().any(|arg| arg == "--check") {
                CommandClass::Verification
            } else {
                CommandClass::Formatting
            }
        }
        "rg" | "grep" | "egrep" | "fgrep" => CommandClass::Search,
        "find" | "ls" | "dir" | "pwd" | "stat" | "head" | "tail" | "cat" | "wc" | "file" | "du" => {
            CommandClass::Inspection
        }
        "git" => classify_git(args),
        "sh" | "bash" | "dash" | "zsh" | "fish" | "cmd" | "powershell" | "pwsh" => {
            CommandClass::Unknown
        }
        _ => CommandClass::Unknown,
    }
}

fn classify_cargo(args: &[String]) -> CommandClass {
    match cargo_subcommand(args) {
        Some("locate-project") => CommandClass::Inspection,
        Some("metadata" | "tree")
            if args.iter().any(|arg| matches!(arg.as_str(), "--offline" | "--frozen")) =>
        {
            CommandClass::Inspection
        }
        Some("check" | "test" | "clippy" | "bench") => CommandClass::Verification,
        Some("build") => CommandClass::Build,
        Some("fmt") if args.iter().any(|arg| arg == "--check") => CommandClass::Verification,
        Some("fmt") => CommandClass::Formatting,
        Some("fix") => CommandClass::Mutation,
        Some("add" | "remove" | "install" | "uninstall" | "update" | "upgrade") => {
            CommandClass::DependencyManagement
        }
        Some("run") => CommandClass::Build,
        _ => CommandClass::Unknown,
    }
}

fn classify_python(args: &[String]) -> CommandClass {
    let module = args.iter().position(|arg| arg == "-m");
    match module {
        Some(index) => match args.get(index + 1).map(String::as_str) {
            Some("pytest" | "unittest" | "compileall" | "mypy") => CommandClass::Verification,
            Some("ruff") if args.get(index + 2).map(String::as_str) == Some("format") => {
                CommandClass::Formatting
            }
            Some("ruff") => CommandClass::Verification,
            Some("pip" | "pip3" | "uv") => CommandClass::DependencyManagement,
            _ => CommandClass::Unknown,
        },
        _ => CommandClass::Unknown,
    }
}

fn classify_node(args: &[String]) -> CommandClass {
    match node_subcommand(args) {
        Some("test" | "lint" | "typecheck") => CommandClass::Verification,
        Some("run") => match args.get(1).map(String::as_str) {
            Some("test" | "lint" | "typecheck" | "check") => CommandClass::Verification,
            Some("build") => CommandClass::Build,
            Some("format" | "fmt") => CommandClass::Formatting,
            _ => CommandClass::Unknown,
        },
        Some("build") => CommandClass::Build,
        Some("format" | "fmt") => CommandClass::Formatting,
        Some("install" | "add" | "remove" | "update" | "upgrade" | "uninstall" | "ci") => {
            CommandClass::DependencyManagement
        }
        _ => CommandClass::Unknown,
    }
}

fn classify_dependency_manager(args: &[String]) -> CommandClass {
    let first =
        first_subcommand(args, &["--proxy", "--index-url", "--extra-index-url", "--python"]);
    let subcommand = if first == Some("pip") {
        args.iter()
            .position(|arg| arg == "pip")
            .and_then(|index| first_subcommand(&args[index + 1..], &[]))
    } else {
        first
    };
    match subcommand {
        Some("run")
            if args.get(1).is_some_and(|arg| {
                matches!(arg.as_str(), "pytest" | "mypy" | "ruff" | "black")
            }) =>
        {
            CommandClass::Verification
        }
        Some("install" | "add" | "remove" | "uninstall" | "update" | "sync") => {
            CommandClass::DependencyManagement
        }
        Some("list" | "show" | "tree" | "freeze") => CommandClass::Inspection,
        _ => CommandClass::Unknown,
    }
}

fn classify_formatter(args: &[String]) -> CommandClass {
    if args.iter().any(|arg| arg == "--check" || arg == "check") {
        CommandClass::Verification
    } else if args.first().map(String::as_str) == Some("format") {
        CommandClass::Formatting
    } else {
        CommandClass::Unknown
    }
}

fn classify_git(args: &[String]) -> CommandClass {
    match first_subcommand(args, &["-C", "--git-dir", "--work-tree", "--namespace", "--config-env"])
    {
        Some("diff") if args.iter().any(|arg| arg == "--check") => CommandClass::Verification,
        Some(
            "status" | "diff" | "show" | "log" | "branch" | "tag" | "rev-parse" | "ls-files"
            | "grep" | "shortlog" | "describe",
        ) => CommandClass::GitRead,
        Some(
            "add" | "commit" | "checkout" | "switch" | "restore" | "reset" | "clean" | "rm" | "mv"
            | "apply" | "stash" | "rebase" | "merge" | "cherry-pick" | "pull" | "push" | "fetch"
            | "clone" | "init" | "worktree",
        ) => CommandClass::GitMutation,
        _ => CommandClass::Unknown,
    }
}

fn analyze_cargo(effects: &mut CommandEffects, args: &[String], cwd: &str) {
    let package = option_values(args, &["-p", "--package"]);
    for value in package {
        add_value(&mut effects.packages, &value);
    }
    if let Some(manifest) = option_value(args, &["--manifest-path"]) {
        add_path_with_cwd(&mut effects.manifests, cwd, manifest);
    } else {
        add_path_with_cwd(&mut effects.manifests, cwd, "Cargo.toml");
    }
    add_path_with_cwd(&mut effects.manifests, cwd, "Cargo.lock");
    if let Some(target) = option_value(args, &["--target-dir"]) {
        add_path_with_cwd(&mut effects.paths, cwd, target);
    } else if effects.class == CommandClass::Build
        || (effects.class.is_verification() && cargo_subcommand(args) != Some("fmt"))
    {
        add_path_with_cwd(&mut effects.paths, cwd, "target");
    }
    if let Some(index) = args.iter().position(|arg| arg == "--") {
        for path in args.iter().skip(index + 1) {
            add_path_with_cwd(&mut effects.paths, cwd, path);
        }
    }
    effects.writes_workspace = effects.class.is_mutation();
    effects.writes_artifacts = effects.class == CommandClass::Build
        || (effects.class.is_verification() && cargo_subcommand(args) != Some("fmt"));
    if matches!(effects.class, CommandClass::Formatting | CommandClass::Mutation)
        && !args.iter().any(|arg| arg == "--")
    {
        effects.uncertain = true;
    }
    if effects.class == CommandClass::Mutation
        && cargo_subcommand(args) == Some("fix")
        && effects.paths.is_empty()
    {
        effects.uncertain = true;
    }
    if cargo_subcommand(args) == Some("run") {
        effects.uncertain = true;
    }
    effects.verification_scope = if effects.packages.len() == 1 {
        Some(format!("package:{}", effects.packages[0]))
    } else if args.iter().any(|arg| arg == "--workspace") {
        Some("workspace".to_owned())
    } else {
        Some(cwd_scope(cwd))
    };
}

fn analyze_git(effects: &mut CommandEffects, args: &[String], cwd: &str) {
    let subcommand =
        first_subcommand(args, &["-C", "--git-dir", "--work-tree", "--namespace", "--config-env"]);
    if matches!(effects.class, CommandClass::GitRead | CommandClass::Verification) {
        for path in args.iter().skip_while(|arg| arg.as_str() != "--").skip(1) {
            add_path_with_cwd(&mut effects.paths, cwd, path);
        }
        if effects.paths.is_empty() {
            effects.paths.push(cwd_scope(cwd));
        }
    } else {
        effects.writes_workspace = true;
        if matches!(subcommand, Some("add" | "rm" | "mv" | "restore")) {
            let start = args
                .iter()
                .position(|arg| Some(arg.as_str()) == subcommand)
                .map_or(0, |index| index + 1);
            analyze_positional_paths(
                effects,
                args.get(start..).unwrap_or_default(),
                cwd,
                &[],
                &["-m", "--message", "-C", "--pathspec-from-file"],
            );
        } else {
            for path in args.iter().skip_while(|arg| arg.as_str() != "--").skip(1) {
                add_path_with_cwd(&mut effects.paths, cwd, path);
            }
        }
    }
    effects.verification_scope = Some(cwd_scope(cwd));
}

fn analyze_search(effects: &mut CommandEffects, args: &[String], cwd: &str) {
    let mut pattern_seen = false;
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            pattern_seen = true;
            continue;
        }
        if arg.starts_with('-') && !pattern_seen {
            if matches!(
                arg.as_str(),
                "-e" | "--regexp"
                    | "-g"
                    | "--glob"
                    | "-t"
                    | "--type"
                    | "-C"
                    | "-A"
                    | "-B"
                    | "-m"
                    | "--max-count"
            ) {
                skip_next = true;
            }
            continue;
        }
        if !pattern_seen {
            pattern_seen = true;
            continue;
        }
        add_path_with_cwd(&mut effects.paths, cwd, arg);
    }
    if effects.paths.is_empty() {
        effects.paths.push(cwd_scope(cwd));
    }
}

fn analyze_find(effects: &mut CommandEffects, args: &[String], cwd: &str) {
    let mut predicates = false;
    for arg in args {
        if arg.starts_with('-') {
            predicates = true;
        } else if !predicates {
            add_path_with_cwd(&mut effects.paths, cwd, arg);
        }
    }
    if effects.paths.is_empty() {
        effects.paths.push(cwd_scope(cwd));
    }
}

fn analyze_paths(effects: &mut CommandEffects, args: &[String], cwd: &str) {
    analyze_positional_paths(
        effects,
        args,
        cwd,
        &[],
        &[
            "-A",
            "--all",
            "-B",
            "--bytes",
            "-C",
            "--context",
            "-c",
            "--count",
            "-d",
            "--max-depth",
            "-f",
            "--format",
            "-n",
            "--lines",
            "-T",
            "--time-style",
        ],
    );
    if effects.paths.is_empty() {
        effects.paths.push(cwd_scope(cwd));
    }
}

fn analyze_node(effects: &mut CommandEffects, command: &str, args: &[String], cwd: &str) {
    let manifest = match command {
        "pnpm" => "pnpm-lock.yaml",
        "yarn" => "yarn.lock",
        _ => "package-lock.json",
    };
    add_path_with_cwd(&mut effects.manifests, cwd, "package.json");
    add_path_with_cwd(&mut effects.manifests, cwd, manifest);
    for package in option_values(args, &["--workspace", "-w"]) {
        add_value(&mut effects.packages, &package);
    }
    if effects.class == CommandClass::DependencyManagement {
        add_dependency_packages(
            effects,
            args,
            cwd,
            &[],
            &["--workspace", "-w", "--prefix", "--registry", "--cache", "--cwd"],
        );
    }
    effects.writes_workspace =
        effects.class.is_mutation() || effects.class == CommandClass::Formatting;
    effects.verification_scope = Some(cwd_scope(cwd));
    if let Some(index) = args.iter().position(|arg| arg == "--") {
        for path in args.iter().skip(index + 1) {
            add_path_with_cwd(&mut effects.paths, cwd, path);
        }
    }
    if effects.class == CommandClass::Build {
        effects.writes_artifacts = true;
        add_path_with_cwd(&mut effects.paths, cwd, "dist");
    } else if effects.class == CommandClass::Verification {
        effects.writes_artifacts = true;
    }
    if effects.class == CommandClass::Formatting && effects.paths.is_empty() {
        effects.uncertain = true;
    }
}

fn analyze_python(effects: &mut CommandEffects, program: &str, args: &[String], cwd: &str) {
    let command = python_command(program, args);
    add_python_manifest(effects, cwd, command);
    effects.writes_workspace = effects.class.is_mutation();
    effects.writes_artifacts = effects.class.is_verification();
    effects.verification_scope = Some(cwd_scope(cwd));
    let start = usize::from(args.first().map(String::as_str) == Some("-m")) * 2;
    let remaining = args.get(start..).unwrap_or_default();
    if effects.class == CommandClass::DependencyManagement {
        add_dependency_packages(
            effects,
            remaining,
            cwd,
            &["pip", "pip3", "uv", "poetry"],
            &["--index-url", "--extra-index-url", "--proxy"],
        );
    }
    analyze_positional_paths(
        effects,
        remaining,
        cwd,
        &[
            "pytest",
            "unittest",
            "compileall",
            "mypy",
            "ruff",
            "pip",
            "pip3",
            "uv",
            "poetry",
            "install",
            "add",
            "remove",
            "uninstall",
            "update",
            "sync",
            "run",
            "check",
            "format",
            "discover",
        ],
        &[
            "-m",
            "--maxfail",
            "--max-count",
            "-k",
            "--config-file",
            "--config",
            "-r",
            "--requirement",
        ],
    );
}

fn analyze_formatter_paths(effects: &mut CommandEffects, args: &[String], cwd: &str) {
    effects.writes_workspace = effects.class == CommandClass::Formatting;
    effects.verification_scope = Some(cwd_scope(cwd));
    analyze_positional_paths(
        effects,
        args,
        cwd,
        &["check", "format"],
        &["--config", "--config-file"],
    );
    if effects.class == CommandClass::Formatting && effects.paths.is_empty() {
        effects.uncertain = true;
    }
}

fn analyze_rustfmt(effects: &mut CommandEffects, args: &[String], cwd: &str) {
    effects.writes_workspace = effects.class == CommandClass::Formatting;
    effects.verification_scope = Some(cwd_scope(cwd));
    analyze_positional_paths(
        effects,
        args,
        cwd,
        &[],
        &["--emit", "--edition", "--config-path", "--print-config"],
    );
    if effects.class == CommandClass::Formatting && effects.paths.is_empty() {
        effects.uncertain = true;
    }
}

fn analyze_compiler(effects: &mut CommandEffects, command: &str, args: &[String], cwd: &str) {
    effects.verification_scope = Some(cwd_scope(cwd));
    analyze_positional_paths(
        effects,
        args,
        cwd,
        &[],
        &[
            "--crate-name",
            "--edition",
            "--extern",
            "-L",
            "-o",
            "--out-dir",
            "--project",
            "-p",
            "--outFile",
            "--outDir",
        ],
    );
    if command == "tsc" {
        add_path_with_cwd(&mut effects.manifests, cwd, "tsconfig.json");
    }
}

fn add_python_manifest(effects: &mut CommandEffects, cwd: &str, command: Option<&str>) {
    add_path_with_cwd(&mut effects.manifests, cwd, "pyproject.toml");
    match command {
        Some("uv") => add_path_with_cwd(&mut effects.manifests, cwd, "uv.lock"),
        Some("poetry") => add_path_with_cwd(&mut effects.manifests, cwd, "poetry.lock"),
        Some("pip") | Some("pip3") => {
            add_path_with_cwd(&mut effects.manifests, cwd, "requirements.txt")
        }
        _ => {}
    }
}

fn python_command<'a>(program: &'a str, args: &'a [String]) -> Option<&'a str> {
    if matches!(program, "python" | "python3") {
        args.iter()
            .position(|arg| arg == "-m")
            .and_then(|index| args.get(index + 1))
            .map(String::as_str)
    } else {
        Some(program)
    }
}

fn analyze_positional_paths(
    effects: &mut CommandEffects,
    args: &[String],
    cwd: &str,
    skip_words: &[&str],
    value_options: &[&str],
) {
    let mut skip_next = false;
    let mut after_separator = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "--" {
            after_separator = true;
            continue;
        }
        if !after_separator && skip_words.iter().any(|word| arg == word) {
            continue;
        }
        if !after_separator && arg.starts_with('-') {
            if value_options.iter().any(|option| arg == option) {
                skip_next = true;
            }
            continue;
        }
        add_path_with_cwd(&mut effects.paths, cwd, arg);
    }
}

fn add_dependency_packages(
    effects: &mut CommandEffects,
    args: &[String],
    cwd: &str,
    wrapper_words: &[&str],
    value_options: &[&str],
) {
    let mut operation_seen = false;
    let mut ignore_next = false;
    let mut path_next = false;
    for arg in args {
        if ignore_next {
            ignore_next = false;
            continue;
        }
        if path_next {
            path_next = false;
            add_path_with_cwd(&mut effects.paths, cwd, arg);
            continue;
        }
        if !operation_seen {
            if wrapper_words.iter().any(|word| arg == word) {
                continue;
            }
            if arg.starts_with('-') {
                if value_options.iter().any(|option| arg == option) {
                    ignore_next = true;
                }
                continue;
            }
            operation_seen = true;
            continue;
        }
        if arg == "--" {
            break;
        }
        if arg.starts_with('-') {
            if matches!(arg.as_str(), "-r" | "--requirement" | "--file") {
                path_next = true;
            }
            continue;
        }
        if arg.starts_with('@') || !looks_like_path(arg) {
            add_value(&mut effects.packages, arg);
        } else {
            add_path_with_cwd(&mut effects.paths, cwd, arg);
        }
    }
}

fn cargo_subcommand(args: &[String]) -> Option<&str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if matches!(
            arg.as_str(),
            "-p" | "--package"
                | "--manifest-path"
                | "--target-dir"
                | "--profile"
                | "--features"
                | "--config"
                | "--color"
                | "--message-format"
                | "-j"
                | "--jobs"
        ) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn node_subcommand(args: &[String]) -> Option<&str> {
    first_subcommand(
        args,
        &[
            "--prefix",
            "--userconfig",
            "--registry",
            "--cache",
            "--filter",
            "--dir",
            "--cwd",
            "-w",
            "--workspace",
        ],
    )
}

fn first_subcommand<'a>(args: &'a [String], value_options: &[&str]) -> Option<&'a str> {
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if value_options.iter().any(|option| arg == option) {
            skip_next = true;
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        return Some(arg.as_str());
    }
    None
}

fn option_value<'a>(args: &'a [String], names: &[&str]) -> Option<&'a str> {
    for (index, arg) in args.iter().enumerate() {
        for name in names {
            if arg == name {
                return args.get(index + 1).map(String::as_str);
            }
            if let Some(value) = arg.strip_prefix(name).and_then(|value| value.strip_prefix('=')) {
                return Some(value);
            }
        }
    }
    None
}

fn option_values(args: &[String], names: &[&str]) -> Vec<String> {
    let mut values = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        for name in names {
            let value = if arg == name {
                args.get(index + 1).map(String::as_str)
            } else {
                arg.strip_prefix(name).and_then(|value| value.strip_prefix('='))
            };
            if let Some(value) = value {
                add_value(&mut values, value);
            }
        }
    }
    values
}

fn add_value(values: &mut Vec<String>, value: &str) {
    if values.len() >= MAX_COMMAND_PACKAGES
        || value.is_empty()
        || value.len() > MAX_COMMAND_FIELD_BYTES
        || values.iter().any(|existing| existing == value)
    {
        return;
    }
    values.push(value.to_owned());
}

fn add_path(paths: &mut Vec<String>, path: &str) {
    if paths.len() >= MAX_COMMAND_PATHS
        || path.is_empty()
        || path.len() > MAX_COMMAND_FIELD_BYTES
        || paths.iter().any(|existing| existing == path)
    {
        return;
    }
    paths.push(path.to_owned());
}

fn add_path_with_cwd(paths: &mut Vec<String>, cwd: &str, path: &str) {
    if path.is_empty() || path.starts_with('-') {
        return;
    }
    let normalized = normalize_path(cwd, path);
    if normalized.is_empty() {
        return;
    }
    add_path(paths, &normalized);
}

fn normalize_path(cwd: &str, path: &str) -> String {
    if path.starts_with('/') || path.starts_with('\\') || path.as_bytes().get(1) == Some(&b':') {
        return String::new();
    }
    if path.split(['/', '\\']).any(|part| part == "..") {
        return String::new();
    }
    let path = path.strip_prefix("./").unwrap_or(path);
    let cwd = cwd.trim_matches(['/', '\\']).trim_start_matches("./");
    let joined = if cwd.is_empty() || cwd == "." {
        path.to_owned()
    } else if path == "." {
        cwd.to_owned()
    } else {
        format!("{cwd}/{path}")
    };
    joined.trim_matches('/').to_owned()
}

fn cwd_scope(cwd: &str) -> String {
    let scope = normalize_path("", if cwd.is_empty() { "." } else { cwd });
    if scope.is_empty() || scope.len() > MAX_COMMAND_FIELD_BYTES { ".".to_owned() } else { scope }
}

fn looks_like_path(value: &str) -> bool {
    value == "."
        || value == ".."
        || value.contains('/')
        || value.contains('\\')
        || value.starts_with('.')
        || [
            ".rs", ".toml", ".lock", ".json", ".js", ".jsx", ".ts", ".tsx", ".py", ".md", ".txt",
            ".yaml", ".yml",
        ]
        .iter()
        .any(|suffix| value.ends_with(suffix))
}

fn bounded_argv(program: &str, args: &[String]) -> bool {
    if args.len() > MAX_COMMAND_ARGS {
        return false;
    }
    let mut bytes = program.len();
    for arg in args {
        bytes = bytes.saturating_add(arg.len());
        if bytes > MAX_COMMAND_BYTES {
            return false;
        }
    }
    true
}

fn unsafe_path_token(value: &str) -> bool {
    let candidate = value.split_once('=').map_or(value, |(_, value)| value);
    candidate.starts_with('/')
        || candidate.starts_with('\\')
        || candidate.as_bytes().get(1) == Some(&b':')
        || candidate.split(['/', '\\']).any(|part| part == "..")
}

fn has_shell_syntax(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(arg.as_str(), "|" | ">" | ">>" | "<" | ";" | "&&" | "||" | "&")
            || arg.contains("&&")
            || arg.contains("||")
            || arg.contains('|')
            || arg.contains(';')
            || arg.contains('>')
            || arg.contains('<')
            || arg.starts_with(">")
            || arg.starts_with("<")
            || arg.starts_with("$(")
            || arg.contains('`')
    })
}

fn base_name(program: &str) -> &str {
    Path::new(program).file_name().and_then(|name| name.to_str()).unwrap_or(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn direct_commands_have_stable_classes() {
        assert_eq!(
            classify_command("cargo", &args(&["test", "-p", "api"])),
            CommandClass::Verification
        );
        assert_eq!(classify_command("cargo", &args(&["build"])), CommandClass::Build);
        assert_eq!(classify_command("cargo", &args(&["fmt"])), CommandClass::Formatting);
        assert_eq!(
            classify_command("cargo", &args(&["metadata", "--offline"])),
            CommandClass::Inspection
        );
        assert_eq!(
            classify_command("cargo", &args(&["locate-project", "--message-format", "plain"])),
            CommandClass::Inspection
        );
        assert_eq!(
            classify_command("cargo", &args(&["tree", "--offline"])),
            CommandClass::Inspection
        );
        assert_eq!(classify_command("cargo", &args(&["fix"])), CommandClass::Mutation);
        assert_eq!(
            classify_command("cargo", &args(&["add", "serde"])),
            CommandClass::DependencyManagement
        );
        assert_eq!(classify_command("rg", &args(&["needle", "src"])), CommandClass::Search);
        assert_eq!(
            classify_command("find", &args(&["src", "-name", "*.rs"])),
            CommandClass::Inspection
        );
        assert_eq!(classify_command("git", &args(&["status", "--short"])), CommandClass::GitRead);
        assert_eq!(
            classify_command("git", &args(&["diff", "--check"])),
            CommandClass::Verification
        );
        assert_eq!(
            classify_command("git", &args(&["-C", "repo", "status"])),
            CommandClass::GitRead
        );
        assert_eq!(
            classify_command("git", &args(&["commit", "-am", "change"])),
            CommandClass::GitMutation
        );
    }

    #[test]
    fn cargo_metadata_and_tree_require_network_disabled_flags_for_inspection() {
        for subcommand in ["metadata", "tree"] {
            for flag in ["--offline", "--frozen"] {
                let effects = analyze_command("cargo", &args(&[subcommand, flag]), "fixture");
                assert_eq!(effects.class, CommandClass::Inspection);
                assert!(!effects.uncertain);
                assert!(!effects.mutates_workspace());
            }

            let effects = analyze_command("cargo", &args(&[subcommand]), "fixture");
            assert_eq!(effects.class, CommandClass::Unknown);
            assert!(effects.uncertain);
        }

        assert_eq!(classify_command("cargo", &args(&["locate-project"])), CommandClass::Inspection);
    }

    #[test]
    fn common_language_unix_and_package_commands_are_covered() {
        assert_eq!(
            classify_command("rustfmt", &args(&["--check", "src/lib.rs"])),
            CommandClass::Verification
        );
        assert_eq!(classify_command("rustfmt", &args(&["src/lib.rs"])), CommandClass::Formatting);
        assert_eq!(classify_command("npm", &args(&["test"])), CommandClass::Verification);
        assert_eq!(classify_command("pnpm", &args(&["run", "build"])), CommandClass::Build);
        assert_eq!(
            classify_command("yarn", &args(&["add", "serde"])),
            CommandClass::DependencyManagement
        );
        assert_eq!(classify_command("pytest", &args(&["tests"])), CommandClass::Verification);
        assert_eq!(
            classify_command("python3", &args(&["-m", "pytest"])),
            CommandClass::Verification
        );
        assert_eq!(
            classify_command("python", &args(&["-m", "pip", "install", "httpx"])),
            CommandClass::DependencyManagement
        );
        assert_eq!(
            classify_command("pip", &args(&["install", "httpx"])),
            CommandClass::DependencyManagement
        );
        assert_eq!(classify_command("rg", &args(&["needle", "src"])), CommandClass::Search);
        assert_eq!(classify_command("grep", &args(&["-R", "needle", "src"])), CommandClass::Search);
        assert_eq!(
            classify_command("find", &args(&[".", "-name", "*.rs"])),
            CommandClass::Inspection
        );
        assert_eq!(classify_command("./script.sh", &args(&[])), CommandClass::Unknown);
        assert_eq!(classify_command("node", &args(&["script.js"])), CommandClass::Unknown);
    }

    #[test]
    fn shell_interpreters_scripts_redirects_and_pipes_fail_closed() {
        assert_eq!(classify_command("bash", &args(&["-c", "cargo test"])), CommandClass::Unknown);
        assert_eq!(classify_command("python", &args(&["script.py"])), CommandClass::Unknown);
        assert_eq!(classify_command("rg", &args(&["needle", ">", "out"])), CommandClass::Unknown);
        assert_eq!(classify_command("rg", &args(&["needle", "|", "sed"])), CommandClass::Unknown);
    }

    #[test]
    fn effects_extract_scope_paths_packages_and_manifests() {
        let effects = analyze_command("cargo", &args(&["test", "-p", "api"]), "crates/api");
        assert_eq!(effects.class, CommandClass::Verification);
        assert_eq!(effects.packages, ["api"]);
        assert!(effects.manifests.iter().any(|path| path == "crates/api/Cargo.toml"));
        assert_eq!(effects.verification_scope.as_deref(), Some("package:api"));
        assert!(effects.writes_artifacts);
        let pnpm = analyze_command("pnpm", &args(&["install"]), "web");
        assert!(pnpm.manifests.iter().any(|path| path == "web/pnpm-lock.yaml"));
        assert!(
            analyze_command("pytest", &args(&["tests"]), "web")
                .paths
                .iter()
                .any(|path| path == "web/tests")
        );
        assert_eq!(analyze_command("cat", &args(&["README"]), "").paths, ["README"]);
        let npm = analyze_command("npm", &args(&["install", "lodash", "@types/node"]), "web");
        assert_eq!(npm.packages, ["lodash", "@types/node"]);
        let pip = analyze_command("pip", &args(&["install", "httpx"]), "python");
        assert_eq!(pip.packages, ["httpx"]);
        let git = analyze_command("git", &args(&["add", "src/lib.rs"]), "repo");
        assert_eq!(git.paths, ["repo/src/lib.rs"]);
    }

    #[test]
    fn read_only_effects_can_parallelize_and_mutations_cannot() {
        let read = analyze_command("rg", &args(&["needle", "src"]), "");
        let other_read = analyze_command("find", &args(&["tests", "-name", "*.rs"]), "");
        assert!(!read.footprint().conflicts(&other_read.footprint()));
        let write = analyze_command("cargo", &args(&["fmt"]), "");
        assert!(read.footprint().conflicts(&write.footprint()));
        let test = analyze_command("cargo", &args(&["test", "--", "src/lib.rs"]), "");
        let source_write = ToolFootprint::from_effects(vec![ToolEffect::Write(
            ToolResource::WorkspacePath("src/lib.rs".to_owned()),
        )]);
        assert!(test.footprint().conflicts(&source_write));
    }

    #[test]
    fn bounds_make_large_argv_unknown() {
        let huge = vec!["x".repeat(MAX_COMMAND_BYTES)];
        assert_eq!(classify_command("rg", &huge), CommandClass::Unknown);
    }
}
