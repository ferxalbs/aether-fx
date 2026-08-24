use aether_core::analyze_command;
use serde::Serialize;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolKind {
    List,
    Read,
    Shell,
    Write,
    Verify,
}

#[derive(Clone, Debug)]
pub enum Action {
    List { path: &'static str },
    Read { path: &'static str },
    Write { path: &'static str, contents: &'static str },
    Shell { program: &'static str, args: &'static [&'static str] },
    Verify,
    Finish,
}

impl Action {
    fn fingerprint(&self) -> Option<String> {
        match self {
            Self::List { path } => Some(format!("list:{path}")),
            Self::Read { path } => Some(format!("read:{path}")),
            Self::Write { path, contents } => Some(format!("write:{path}:{contents}")),
            Self::Shell { program, args } => {
                Some(format!("shell:{program}:{}", args.join("\u{1f}")))
            }
            Self::Verify => None,
            Self::Finish => None,
        }
    }

    fn kind(&self) -> Option<ToolKind> {
        match self {
            Self::List { .. } => Some(ToolKind::List),
            Self::Read { .. } => Some(ToolKind::Read),
            Self::Write { .. } => Some(ToolKind::Write),
            Self::Shell { .. } => Some(ToolKind::Shell),
            Self::Verify => Some(ToolKind::Verify),
            Self::Finish => None,
        }
    }
}

/// Replaceable model boundary. Real-provider adapters can implement this trait without changing
/// fixture execution, metrics, thresholds, or result formatting.
pub trait EvalBackend {
    fn name(&self) -> &str;
    fn reset(&mut self, task: &Task);
    fn next_action(&mut self, observation: &str) -> Action;
}

#[derive(Clone, Debug)]
pub struct ExpectedOutcome {
    pub path: &'static str,
    pub contains: &'static str,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub id: &'static str,
    pub prompt: &'static str,
    pub fixture: &'static str,
    pub expected_outcomes: &'static [ExpectedOutcome],
    pub verification_command: &'static [&'static str],
    pub focused_verification_command: Option<&'static [&'static str]>,
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_verification_attempts: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ExecutionMetrics {
    pub success: bool,
    pub model_steps: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub verification_attempts: u32,
    pub verification_scope: String,
    pub verification_quality: String,
}

#[derive(Debug, Serialize)]
pub struct TaskResult {
    pub task_id: String,
    pub prompt: String,
    pub expected_outcomes: Vec<String>,
    pub verification_command: Vec<String>,
    pub max_steps: u32,
    pub max_tool_calls: u32,
    pub max_verification_attempts: u32,
    pub baseline_success: bool,
    pub before: ExecutionMetrics,
    pub after: ExecutionMetrics,
    pub success: bool,
    pub wall_time_ms: u128,
    pub harness_overhead_ms: u128,
    pub model_steps: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub verification_attempts: u32,
    pub verification_scope: String,
    pub verification_quality: String,
    pub final_test_status: String,
    pub failure: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct AggregateMetrics {
    pub tasks: usize,
    pub succeeded: usize,
    pub success_rate: f64,
    pub model_steps: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub verification_attempts: u32,
    pub wall_time_ms: u128,
    pub harness_overhead_ms: u128,
    pub baseline_succeeded: usize,
    pub baseline_success_rate: f64,
    pub baseline_executed_tool_calls: u32,
    pub baseline_bytes_read: u64,
    pub baseline_context_bytes: u64,
    pub verification_scope_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct RegressionThresholds {
    pub minimum_success_rate: f64,
    pub maximum_model_steps: u32,
    pub maximum_executed_tool_calls: u32,
    pub maximum_context_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct SuiteResult {
    pub schema_version: u32,
    pub backend: String,
    pub success: bool,
    pub thresholds: RegressionThresholds,
    pub aggregate: AggregateMetrics,
    pub tasks: Vec<TaskResult>,
}

pub struct ScriptedBackend {
    name: &'static str,
    script: &'static [Action],
    cursor: usize,
}

impl ScriptedBackend {
    #[must_use]
    pub const fn new(name: &'static str, script: &'static [Action]) -> Self {
        Self { name, script, cursor: 0 }
    }
}

impl EvalBackend for ScriptedBackend {
    fn name(&self) -> &str {
        self.name
    }

    fn reset(&mut self, _task: &Task) {
        self.cursor = 0;
    }

    fn next_action(&mut self, _observation: &str) -> Action {
        let action = self.script.get(self.cursor).cloned().unwrap_or(Action::Finish);
        self.cursor += 1;
        action
    }
}

pub fn run_suite(output: Option<&Path>) -> Result<SuiteResult, String> {
    let mut results = Vec::new();
    for scenario in scenarios() {
        let mut backend = ScriptedBackend::new("deterministic-fake", scenario.script);
        let baseline = run_task_mode(scenario.task, &mut backend, false)?;
        let baseline_metrics = baseline.after.clone();
        let mut policy = run_task_mode(scenario.task, &mut backend, true)?;
        policy.baseline_success = baseline.success;
        policy.before = baseline_metrics;
        results.push(policy);
    }

    let aggregate = aggregate(&results);
    let thresholds = RegressionThresholds {
        minimum_success_rate: 1.0,
        maximum_model_steps: 55,
        maximum_executed_tool_calls: 40,
        maximum_context_bytes: 32_000,
    };
    let success = aggregate.success_rate >= thresholds.minimum_success_rate
        && aggregate.model_steps <= thresholds.maximum_model_steps
        && aggregate.executed_tool_calls <= thresholds.maximum_executed_tool_calls
        && aggregate.context_bytes <= thresholds.maximum_context_bytes;
    let suite = SuiteResult {
        schema_version: 1,
        backend: "deterministic-fake".to_owned(),
        success,
        thresholds,
        aggregate,
        tasks: results,
    };
    if let Some(path) = output {
        let json = serde_json::to_vec_pretty(&suite).map_err(|error| error.to_string())?;
        fs::write(path, json).map_err(|error| format!("write {}: {error}", path.display()))?;
    }
    Ok(suite)
}

/// Built-in task definitions for provider comparison runners.
#[must_use]
pub fn task_catalog() -> [&'static Task; 8] {
    [
        &TARGETED_BUG,
        &MULTI_FILE,
        &FAILED_FIRST,
        &REDUNDANT_READ,
        &FOCUSED_VERIFY,
        &INSUFFICIENT_EVIDENCE,
        &TARGETED_EXPLORATION,
        &SHELL_HEAVY,
    ]
}

pub fn compact_summary(suite: &SuiteResult) -> String {
    let mut output = String::new();
    for result in &suite.tasks {
        let mark = if result.success { "PASS" } else { "FAIL" };
        output.push_str(&format!(
            "{mark:4} {:28} steps={:2} tools={:2}/{:2} before={} verify={} scope={} quality={} read={}B\n",
            result.task_id,
            result.model_steps,
            result.executed_tool_calls,
            result.tool_calls,
            result.before.executed_tool_calls,
            result.verification_attempts,
            result.verification_scope,
            result.verification_quality,
            result.bytes_read
        ));
    }
    output.push_str(&format!(
        "total: {}/{} ({:.0}%) baseline={}/{} ({:.0}%) steps={} tools={}/{} baseline_tools={} redundant={} context={}B baseline_context={}B wall={}ms overhead={}ms\n",
        suite.aggregate.succeeded,
        suite.aggregate.tasks,
        suite.aggregate.success_rate * 100.0,
        suite.aggregate.baseline_succeeded,
        suite.aggregate.tasks,
        suite.aggregate.baseline_success_rate * 100.0,
        suite.aggregate.model_steps,
        suite.aggregate.executed_tool_calls,
        suite.aggregate.tool_calls,
        suite.aggregate.baseline_executed_tool_calls,
        suite.aggregate.prevented_redundant_calls,
        suite.aggregate.context_bytes,
        suite.aggregate.baseline_context_bytes,
        suite.aggregate.wall_time_ms,
        suite.aggregate.harness_overhead_ms
    ));
    output
}

struct Scenario {
    task: &'static Task,
    script: &'static [Action],
}

fn scenarios() -> [Scenario; 8] {
    [
        Scenario { task: &TARGETED_BUG, script: TARGETED_SCRIPT },
        Scenario { task: &MULTI_FILE, script: MULTI_FILE_SCRIPT },
        Scenario { task: &FAILED_FIRST, script: FAILED_FIRST_SCRIPT },
        Scenario { task: &REDUNDANT_READ, script: REDUNDANT_SCRIPT },
        Scenario { task: &FOCUSED_VERIFY, script: FOCUSED_SCRIPT },
        Scenario { task: &INSUFFICIENT_EVIDENCE, script: INSUFFICIENT_EVIDENCE_SCRIPT },
        Scenario { task: &TARGETED_EXPLORATION, script: TARGETED_EXPLORATION_SCRIPT },
        Scenario { task: &SHELL_HEAVY, script: SHELL_HEAVY_SCRIPT },
    ]
}

pub fn run_task(task: &Task, backend: &mut dyn EvalBackend) -> Result<TaskResult, String> {
    let mut result = run_task_mode(task, backend, true)?;
    result.baseline_success = result.success;
    result.before = result.after.clone();
    Ok(result)
}

fn run_task_mode(
    task: &Task,
    backend: &mut dyn EvalBackend,
    policy_active: bool,
) -> Result<TaskResult, String> {
    let started = Instant::now();
    let copy_started = Instant::now();
    let workspace = Workspace::copy_fixture(task.fixture)?;
    let mut overhead = copy_started.elapsed();
    backend.reset(task);
    let mut observation = task.prompt.to_owned();
    let mut seen_reads = HashSet::new();
    let mut seen_commands = HashSet::new();
    let mut inspected_paths = HashSet::new();
    let mut discovered_dirs = HashSet::new();
    let mut model_steps = 0;
    let mut tool_calls = 0;
    let mut executed_tool_calls = 0;
    let mut prevented = 0;
    let mut bytes_read = 0;
    let mut context_bytes = observation.len() as u64;
    let mut verification_attempts = 0;
    let mut final_test_status = "not_run".to_owned();
    let mut verification_scope = "none".to_owned();
    let mut verification_quality = "none".to_owned();
    let mut failure = None;

    loop {
        if model_steps >= task.max_steps {
            failure = Some("model step budget exceeded".to_owned());
            break;
        }
        let model_started = Instant::now();
        let action = backend.next_action(&observation);
        overhead += model_started.elapsed();
        model_steps += 1;
        if matches!(action, Action::Finish) {
            break;
        }
        tool_calls += 1;
        if tool_calls > task.max_tool_calls {
            failure = Some("tool-call budget exceeded".to_owned());
            break;
        }

        let tool_started = Instant::now();
        if matches!(action.kind(), Some(ToolKind::Read | ToolKind::List))
            && action.fingerprint().is_some_and(|key| !seen_reads.insert(key))
        {
            prevented += 1;
            observation = "redundant call prevented; reuse the previous observation".to_owned();
            overhead += tool_started.elapsed();
            context_bytes += observation.len() as u64;
            continue;
        }
        if policy_active && mutation_without_inspection(&action, &inspected_paths, &discovered_dirs)
        {
            prevented += 1;
            observation =
                "insufficient evidence; inspect the exact mutation target first".to_owned();
            overhead += tool_started.elapsed();
            context_bytes += observation.len() as u64;
            continue;
        }
        if policy_active
            && let Action::Shell { program, args } = &action
            && analyze_direct_command(program, args)
                .is_some_and(|effects| effects.class.is_read_only())
            && action.fingerprint().is_some_and(|key| !seen_commands.insert(key))
        {
            prevented += 1;
            observation = "reuse prior scoped observation".to_owned();
            overhead += tool_started.elapsed();
            context_bytes += observation.len() as u64;
            continue;
        }
        executed_tool_calls += 1;
        match action {
            Action::List { path } => {
                observation = workspace.list(path)?;
                discovered_dirs.insert(path.to_owned());
                bytes_read += observation.len() as u64;
                overhead += tool_started.elapsed();
            }
            Action::Read { path } => {
                observation = workspace.read(path)?;
                inspected_paths.insert(path.to_owned());
                bytes_read += observation.len() as u64;
                overhead += tool_started.elapsed();
            }
            Action::Write { path, contents } => {
                workspace.write(path, contents)?;
                observation = format!("wrote {path}");
                overhead += tool_started.elapsed();
                seen_reads.clear();
                seen_commands.clear();
            }
            Action::Shell { program, args } => {
                let effects = analyze_direct_command(program, args);
                observation = format!("{} {}", program, args.join(" "));
                if effects.as_ref().is_some_and(|effects| effects.class.is_read_only()) {
                    bytes_read += observation.len() as u64;
                }
                overhead += tool_started.elapsed();
            }
            Action::Verify => {
                verification_attempts += 1;
                if verification_attempts > task.max_verification_attempts {
                    failure = Some("verification-attempt budget exceeded".to_owned());
                    break;
                }
                let command = if policy_active {
                    task.focused_verification_command.unwrap_or(task.verification_command)
                } else {
                    task.verification_command
                };
                verification_scope = if policy_active && task.focused_verification_command.is_some()
                {
                    "focused".to_owned()
                } else {
                    "broad".to_owned()
                };
                let status = workspace.verify(command)?;
                final_test_status = if status.success { "passed" } else { "failed" }.to_owned();
                verification_quality = if status.success {
                    if verification_scope == "focused" {
                        "focused_pass".to_owned()
                    } else {
                        "broad_pass".to_owned()
                    }
                } else {
                    "failed".to_owned()
                };
                observation = status.output;
            }
            Action::Finish => unreachable!(),
        }
        context_bytes += observation.len() as u64;
    }

    let outcomes_ok = task.expected_outcomes.iter().all(|expected| {
        workspace.read(expected.path).is_ok_and(|contents| contents.contains(expected.contains))
    });
    let success = failure.is_none() && outcomes_ok && final_test_status == "passed";
    if failure.is_none() && !outcomes_ok {
        failure = Some("expected file outcome was not present".to_owned());
    } else if failure.is_none() && final_test_status != "passed" {
        failure = Some("final verification did not pass".to_owned());
    }
    Ok(TaskResult {
        task_id: task.id.to_owned(),
        prompt: task.prompt.to_owned(),
        expected_outcomes: task
            .expected_outcomes
            .iter()
            .map(|item| format!("{} contains {:?}", item.path, item.contains))
            .collect(),
        verification_command: task.verification_command.iter().map(ToString::to_string).collect(),
        max_steps: task.max_steps,
        max_tool_calls: task.max_tool_calls,
        max_verification_attempts: task.max_verification_attempts,
        success,
        wall_time_ms: started.elapsed().as_millis(),
        harness_overhead_ms: overhead.as_millis(),
        model_steps,
        tool_calls,
        executed_tool_calls,
        prevented_redundant_calls: prevented,
        bytes_read,
        context_bytes,
        verification_attempts,
        final_test_status,
        verification_quality: verification_quality.clone(),
        failure,
        baseline_success: false,
        before: ExecutionMetrics::default(),
        after: ExecutionMetrics {
            success,
            model_steps,
            tool_calls,
            executed_tool_calls,
            prevented_redundant_calls: prevented,
            bytes_read,
            context_bytes,
            verification_attempts,
            verification_scope: verification_scope.clone(),
            verification_quality,
        },
        verification_scope,
    })
}

fn mutation_without_inspection(
    action: &Action,
    inspected: &HashSet<String>,
    discovered_dirs: &HashSet<String>,
) -> bool {
    match action {
        Action::Write { path, .. } => {
            !inspected.contains(*path)
                && Path::new(path)
                    .parent()
                    .and_then(Path::to_str)
                    .is_none_or(|parent| !discovered_dirs.contains(parent))
        }
        _ => false,
    }
}

fn analyze_direct_command(program: &str, args: &[&str]) -> Option<aether_core::CommandEffects> {
    let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
    Some(analyze_command(program, &args, ""))
}

struct VerifyStatus {
    success: bool,
    output: String,
}

struct Workspace {
    root: PathBuf,
}

impl Workspace {
    fn copy_fixture(fixture: &str) -> Result<Self, String> {
        let source =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/fixtures").join(fixture);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_nanos();
        let root = std::env::temp_dir().join(format!("aether-eval-{}-{nonce}", std::process::id()));
        copy_tree(&source, &root).map_err(|error| format!("copy fixture {fixture}: {error}"))?;
        Ok(Self { root })
    }

    fn resolve(&self, relative: &str) -> Result<PathBuf, String> {
        let path = Path::new(relative);
        if path.is_absolute() || path.components().any(|part| matches!(part, Component::ParentDir))
        {
            return Err(format!("fixture path escapes workspace: {relative}"));
        }
        Ok(self.root.join(path))
    }

    fn list(&self, relative: &str) -> Result<String, String> {
        let mut entries = fs::read_dir(self.resolve(relative)?)
            .map_err(|error| error.to_string())?
            .map(|entry| entry.map(|item| item.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        entries.sort();
        Ok(entries.join("\n"))
    }

    fn read(&self, relative: &str) -> Result<String, String> {
        fs::read_to_string(self.resolve(relative)?).map_err(|error| error.to_string())
    }

    fn write(&self, relative: &str, contents: &str) -> Result<(), String> {
        let path = self.resolve(relative)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, contents).map_err(|error| error.to_string())
    }

    fn verify(&self, command: &[&str]) -> Result<VerifyStatus, String> {
        let (program, arguments) =
            command.split_first().ok_or_else(|| "verification command is empty".to_owned())?;
        let output = Command::new(program)
            .args(arguments)
            .current_dir(&self.root)
            .env("CARGO_NET_OFFLINE", "true")
            .output()
            .map_err(|error| format!("run verification: {error}"))?;
        let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
        combined.push_str(&String::from_utf8_lossy(&output.stderr));
        if combined.len() > 8_192 {
            combined.truncate(8_192);
        }
        Ok(VerifyStatus { success: output.status.success(), output: combined })
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn aggregate(results: &[TaskResult]) -> AggregateMetrics {
    let succeeded = results.iter().filter(|result| result.success).count();
    AggregateMetrics {
        tasks: results.len(),
        succeeded,
        success_rate: succeeded as f64 / results.len() as f64,
        model_steps: results.iter().map(|result| result.model_steps).sum(),
        tool_calls: results.iter().map(|result| result.tool_calls).sum(),
        executed_tool_calls: results.iter().map(|result| result.executed_tool_calls).sum(),
        prevented_redundant_calls: results
            .iter()
            .map(|result| result.prevented_redundant_calls)
            .sum(),
        bytes_read: results.iter().map(|result| result.bytes_read).sum(),
        context_bytes: results.iter().map(|result| result.context_bytes).sum(),
        verification_attempts: results.iter().map(|result| result.verification_attempts).sum(),
        wall_time_ms: results.iter().map(|result| result.wall_time_ms).sum(),
        harness_overhead_ms: results.iter().map(|result| result.harness_overhead_ms).sum(),
        baseline_succeeded: results.iter().filter(|result| result.baseline_success).count(),
        baseline_success_rate: results.iter().filter(|result| result.baseline_success).count()
            as f64
            / results.len() as f64,
        baseline_executed_tool_calls: results
            .iter()
            .map(|result| result.before.executed_tool_calls)
            .sum(),
        baseline_bytes_read: results.iter().map(|result| result.before.bytes_read).sum(),
        baseline_context_bytes: results.iter().map(|result| result.before.context_bytes).sum(),
        verification_scope_bytes: results
            .iter()
            .map(|result| result.verification_scope.len() as u64)
            .sum(),
    }
}

const CARGO_TEST: &[&str] = &["cargo", "test", "--quiet", "--offline"];
const FOCUSED_TEST: &[&str] = &["cargo", "test", "--quiet", "--offline", "formats_slug"];

const TARGETED_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/lib.rs", contains: "values.len() - 1" }];
const TARGETED_BUG: Task = Task {
    id: "targeted-bug-repair",
    prompt: "Fix last_index so it returns the final valid index instead of one past the slice.",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 5,
    max_tool_calls: 4,
    max_verification_attempts: 1,
};
const TARGETED_FIXED: &str = "pub fn last_index(values: &[u8]) -> Option<usize> {\n    (!values.is_empty()).then(|| values.len() - 1)\n}\n\n#[cfg(test)]\nmod tests {\n    use super::last_index;\n\n    #[test]\n    fn returns_last_valid_index() {\n        assert_eq!(last_index(&[4, 8, 15]), Some(2));\n        assert_eq!(last_index(&[]), None);\n    }\n}\n";
const TARGETED_SCRIPT: &[Action] = &[
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

const MULTI_OUTCOMES: &[ExpectedOutcome] = &[
    ExpectedOutcome { path: "src/lib.rs", contains: "pub mod slug" },
    ExpectedOutcome { path: "src/slug.rs", contains: "to_ascii_lowercase" },
];
const MULTI_FILE: Task = Task {
    id: "multi-file-feature",
    prompt: "Navigate the crate and add a public slug module, then use it from the display_name API.",
    fixture: "multi-file",
    expected_outcomes: MULTI_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 7,
    max_tool_calls: 6,
    max_verification_attempts: 1,
};
const MULTI_LIB: &str = "pub mod slug;\n\npub fn display_name(input: &str) -> String {\n    slug::format(input)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn display_name_uses_slug_format() {\n        assert_eq!(super::display_name(\"Hello World\"), \"hello-world\");\n    }\n}\n";
const MULTI_SLUG: &str = "pub fn format(input: &str) -> String {\n    input.trim().to_ascii_lowercase().replace(' ', \"-\")\n}\n";
const MULTI_FILE_SCRIPT: &[Action] = &[
    Action::List { path: "src" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/slug.rs", contents: MULTI_SLUG },
    Action::Write { path: "src/lib.rs", contents: MULTI_LIB },
    Action::Verify,
    Action::Finish,
];

const RECOVERY_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/lib.rs", contains: "parse::<u16>()" }];
const FAILED_FIRST: Task = Task {
    id: "failed-first-recovery",
    prompt: "Repair parse_port and recover if the first verification exposes an incorrect edit.",
    fixture: "failed-first",
    expected_outcomes: RECOVERY_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 7,
    max_tool_calls: 6,
    max_verification_attempts: 2,
};
const RECOVERY_WRONG: &str = "pub fn parse_port(value: &str) -> Option<u16> {\n    value.parse::<u8>().ok().map(u16::from)\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn accepts_full_port_range() {\n        assert_eq!(super::parse_port(\"8080\"), Some(8080));\n    }\n}\n";
const RECOVERY_FIXED: &str = "pub fn parse_port(value: &str) -> Option<u16> {\n    value.parse::<u16>().ok()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn accepts_full_port_range() {\n        assert_eq!(super::parse_port(\"8080\"), Some(8080));\n    }\n}\n";
const FAILED_FIRST_SCRIPT: &[Action] = &[
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: RECOVERY_WRONG },
    Action::Verify,
    Action::Write { path: "src/lib.rs", contents: RECOVERY_FIXED },
    Action::Verify,
    Action::Finish,
];

const REDUNDANT_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/lib.rs", contains: "value.trim().is_empty()" }];
const REDUNDANT_READ: Task = Task {
    id: "redundant-read-avoidance",
    prompt: "Fix is_blank while avoiding repeated reads of unchanged files.",
    fixture: "redundant-read",
    expected_outcomes: REDUNDANT_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 6,
    max_tool_calls: 5,
    max_verification_attempts: 1,
};
const REDUNDANT_FIXED: &str = "pub fn is_blank(value: &str) -> bool {\n    value.trim().is_empty()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn whitespace_is_blank() {\n        assert!(super::is_blank(\"  \\n\"));\n    }\n}\n";
const REDUNDANT_SCRIPT: &[Action] = &[
    Action::Read { path: "src/lib.rs" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: REDUNDANT_FIXED },
    Action::Verify,
    Action::Finish,
];

const FOCUSED_OUTCOMES: &[ExpectedOutcome] =
    &[ExpectedOutcome { path: "src/slug.rs", contains: "trim_matches('-')" }];
const FOCUSED_VERIFY: Task = Task {
    id: "focused-verification",
    prompt: "Fix slug formatting and verify only the directly affected formats_slug test.",
    fixture: "focused-verification",
    expected_outcomes: FOCUSED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: Some(FOCUSED_TEST),
    max_steps: 5,
    max_tool_calls: 4,
    max_verification_attempts: 1,
};
const FOCUSED_FIXED: &str = "pub fn format_slug(value: &str) -> String {\n    value.trim().to_ascii_lowercase().replace(' ', \"-\").trim_matches('-').to_owned()\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn formats_slug() {\n        assert_eq!(super::format_slug(\" Hello World \"), \"hello-world\");\n    }\n\n    #[test]\n    fn unrelated_behavior() {\n        assert_eq!(2 + 2, 4);\n    }\n}\n";
const FOCUSED_SCRIPT: &[Action] = &[
    Action::Read { path: "src/slug.rs" },
    Action::Write { path: "src/slug.rs", contents: FOCUSED_FIXED },
    Action::Verify,
    Action::Finish,
];

const INSUFFICIENT_EVIDENCE: Task = Task {
    id: "insufficient-evidence-recovery",
    prompt: "Repair last_index, even if the first edit arrives before the target was inspected.",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 6,
    max_tool_calls: 5,
    max_verification_attempts: 1,
};
const INSUFFICIENT_EVIDENCE_SCRIPT: &[Action] = &[
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

const TARGETED_EXPLORATION: Task = Task {
    id: "targeted-exploration-reuse",
    prompt: "Fix is_blank after a model repeatedly lists the same source directory.",
    fixture: "redundant-read",
    expected_outcomes: REDUNDANT_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 7,
    max_tool_calls: 6,
    max_verification_attempts: 1,
};
const TARGETED_EXPLORATION_SCRIPT: &[Action] = &[
    Action::List { path: "src" },
    Action::List { path: "src" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: REDUNDANT_FIXED },
    Action::Verify,
    Action::Finish,
];

const SHELL_RG_ARGS: &[&str] = &["last_index", "src"];
const SHELL_FIND_ARGS: &[&str] = &["src", "-name", "*.rs"];
const SHELL_CAT_ARGS: &[&str] = &["src/lib.rs"];
const SHELL_HEAVY: Task = Task {
    id: "shell-heavy-command-reuse",
    prompt: "Repair last_index after an inefficient shell-heavy discovery loop.",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 10,
    max_tool_calls: 9,
    max_verification_attempts: 1,
};
const SHELL_HEAVY_SCRIPT: &[Action] = &[
    Action::Shell { program: "rg", args: SHELL_RG_ARGS },
    Action::Shell { program: "rg", args: SHELL_RG_ARGS },
    Action::Shell { program: "find", args: SHELL_FIND_ARGS },
    Action::Shell { program: "find", args: SHELL_FIND_ARGS },
    Action::Shell { program: "cat", args: SHELL_CAT_ARGS },
    Action::Shell { program: "cat", args: SHELL_CAT_ARGS },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/lib.rs", contents: TARGETED_FIXED },
    Action::Verify,
    Action::Finish,
];

#[cfg(test)]
mod tests {
    use super::{compact_summary, run_suite};

    #[test]
    fn deterministic_suite_meets_correctness_and_efficiency_thresholds() {
        let suite = run_suite(None).expect("suite runs");
        assert!(suite.success, "{}", compact_summary(&suite));
        assert_eq!(suite.aggregate.succeeded, 8);
        assert!(suite.aggregate.baseline_executed_tool_calls > suite.aggregate.executed_tool_calls);
        assert!(suite.aggregate.baseline_context_bytes >= suite.aggregate.context_bytes);
        assert!(
            suite.tasks[5].before.executed_tool_calls > suite.tasks[5].after.executed_tool_calls
        );
        assert_eq!(suite.tasks[4].verification_scope, "focused");
        assert!(suite.aggregate.prevented_redundant_calls >= 3);
        assert_eq!(suite.tasks[2].verification_attempts, 2);
        assert_eq!(suite.tasks[4].verification_command, ["cargo", "test", "--quiet", "--offline"]);
        let shell_heavy = suite
            .tasks
            .iter()
            .find(|task| task.task_id == "shell-heavy-command-reuse")
            .expect("shell-heavy scenario");
        assert!(shell_heavy.before.executed_tool_calls > shell_heavy.after.executed_tool_calls);
        assert!(shell_heavy.before.bytes_read > shell_heavy.after.bytes_read);
        assert_eq!(shell_heavy.verification_quality, "broad_pass");
    }
}
