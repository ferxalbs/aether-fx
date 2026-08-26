use serde::Serialize;
use std::fs;
use std::path::Path;

#[cfg(test)]
use aether_agent::RepositoryActionPlanner;
#[cfg(test)]
use aether_core::ContextSnapshot;

mod real;

#[derive(Clone, Debug)]
pub enum Action {
    List { path: &'static str },
    Read { path: &'static str },
    Search { query: &'static str, path: &'static str },
    Write { path: &'static str, contents: &'static str },
    Shell { program: &'static str, args: &'static [&'static str] },
    Verify,
    Finish,
}

/// Replaceable model boundary. Real-provider adapters can implement this trait without changing
/// fixture execution, metrics, thresholds, or result formatting.
pub trait EvalBackend {
    fn name(&self) -> &str;
    fn reset(&mut self, task: &Task);
    fn next_action(&mut self, observation: &str) -> Action;

    fn trace(&mut self, task: &Task) -> Vec<Action> {
        self.reset(task);
        let mut trace = Vec::new();
        for _ in 0..task.max_steps.saturating_add(task.max_tool_calls).saturating_add(1) {
            let action = self.next_action("");
            let done = matches!(action, Action::Finish);
            trace.push(action);
            if done {
                break;
            }
        }
        trace
    }
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
    pub model_requests: u32,
    pub requested_tool_calls: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub verification_attempts: u32,
    pub process_spawns: u32,
    pub wall_time_ms: u64,
    pub cpu_time_ms: u64,
    pub peak_rss_bytes: u64,
    pub allocation_count: u64,
    pub verification_scope: String,
    pub verification_quality: String,
    pub planner_attempts: u32,
    pub planner_hits: u32,
    pub planner_aborted_ambiguity: u32,
    pub planner_actions: u32,
    pub planner_bytes_read: u64,
}

impl ExecutionMetrics {
    #[must_use]
    pub fn planner_hit_rate(&self) -> f64 {
        if self.planner_attempts == 0 {
            0.0
        } else {
            self.planner_hits as f64 / self.planner_attempts as f64
        }
    }
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
    pub model_requests: u32,
    pub requested_tool_calls: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub verification_attempts: u32,
    pub process_spawns: u32,
    pub agent_wall_time_ms: u64,
    pub cpu_time_ms: u64,
    pub peak_rss_bytes: u64,
    pub allocation_count: u64,
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
    pub model_requests: u32,
    pub requested_tool_calls: u32,
    pub tool_calls: u32,
    pub executed_tool_calls: u32,
    pub prevented_redundant_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub bytes_read: u64,
    pub context_bytes: u64,
    pub bytes_shown_to_model: u64,
    pub verification_attempts: u32,
    pub process_spawns: u32,
    pub agent_wall_time_ms: u64,
    pub cpu_time_ms: u64,
    pub peak_rss_bytes: u64,
    pub allocation_count: u64,
    pub wall_time_ms: u128,
    pub harness_overhead_ms: u128,
    pub baseline_succeeded: usize,
    pub baseline_success_rate: f64,
    pub baseline_model_steps: u32,
    pub baseline_model_requests: u32,
    pub baseline_requested_tool_calls: u32,
    pub baseline_tool_calls: u32,
    pub baseline_executed_tool_calls: u32,
    pub baseline_bytes_read: u64,
    pub baseline_context_bytes: u64,
    pub baseline_bytes_shown_to_model: u64,
    pub verification_scope_bytes: u64,
    pub planner_attempts: u32,
    pub planner_hits: u32,
    pub planner_hit_rate: f64,
    pub planner_aborted_ambiguity: u32,
    pub planner_actions: u32,
    pub planner_bytes_read: u64,
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

    fn next_action(&mut self, observation: &str) -> Action {
        if observation.starts_with("planner_observation:") {
            while matches!(self.script.get(self.cursor), Some(Action::Read { .. })) {
                self.cursor += 1;
            }
        }
        let action = self.script.get(self.cursor).cloned().unwrap_or(Action::Finish);
        self.cursor += 1;
        action
    }
}

pub fn run_suite(output: Option<&Path>) -> Result<SuiteResult, String> {
    let mut results = Vec::new();
    for scenario in scenarios() {
        let baseline = real::run_task(scenario.task, scenario.script, false)?;
        let baseline_metrics = baseline.after.clone();
        let mut policy = real::run_task(scenario.task, scenario.script, true)?;
        policy.baseline_success = baseline.success;
        policy.before = baseline_metrics;
        results.push(policy);
    }

    let aggregate = aggregate(&results);
    let thresholds = RegressionThresholds {
        minimum_success_rate: 1.0,
        maximum_model_steps: 55,
        maximum_executed_tool_calls: 40,
        maximum_context_bytes: 40_000,
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
pub fn task_catalog() -> [&'static Task; 9] {
    [
        &TARGETED_BUG,
        &MULTI_FILE,
        &FAILED_FIRST,
        &REDUNDANT_READ,
        &FOCUSED_VERIFY,
        &INSUFFICIENT_EVIDENCE,
        &TARGETED_EXPLORATION,
        &SHELL_HEAVY,
        &RELATIONSHIP_HEAVY_EXPLORATION,
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
        "total: {}/{} ({:.0}%) baseline={}/{} ({:.0}%) requests={}/{} steps={}/{} tools={}/{} baseline_tools={}/{} redundant={} context={}B/{}B shown={}B/{}B planner={}/{} ({:.0}%) ambiguous={} actions={} read={}B spawns={} wall={}ms overhead={}ms\n",
        suite.aggregate.succeeded,
        suite.aggregate.tasks,
        suite.aggregate.success_rate * 100.0,
        suite.aggregate.baseline_succeeded,
        suite.aggregate.tasks,
        suite.aggregate.baseline_success_rate * 100.0,
        suite.aggregate.model_requests,
        suite.aggregate.baseline_model_requests,
        suite.aggregate.model_steps,
        suite.aggregate.baseline_model_steps,
        suite.aggregate.executed_tool_calls,
        suite.aggregate.tool_calls,
        suite.aggregate.baseline_tool_calls,
        suite.aggregate.baseline_executed_tool_calls,
        suite.aggregate.prevented_redundant_calls,
        suite.aggregate.context_bytes,
        suite.aggregate.baseline_context_bytes,
        suite.aggregate.bytes_shown_to_model,
        suite.aggregate.baseline_bytes_shown_to_model,
        suite.aggregate.planner_hits,
        suite.aggregate.planner_attempts,
        suite.aggregate.planner_hit_rate * 100.0,
        suite.aggregate.planner_aborted_ambiguity,
        suite.aggregate.planner_actions,
        suite.aggregate.planner_bytes_read,
        suite.aggregate.process_spawns,
        suite.aggregate.wall_time_ms,
        suite.aggregate.harness_overhead_ms
    ));
    output
}

struct Scenario {
    task: &'static Task,
    script: &'static [Action],
}

fn scenarios() -> [Scenario; 9] {
    [
        Scenario { task: &TARGETED_BUG, script: TARGETED_SCRIPT },
        Scenario { task: &MULTI_FILE, script: MULTI_FILE_SCRIPT },
        Scenario { task: &FAILED_FIRST, script: FAILED_FIRST_SCRIPT },
        Scenario { task: &REDUNDANT_READ, script: REDUNDANT_SCRIPT },
        Scenario { task: &FOCUSED_VERIFY, script: FOCUSED_SCRIPT },
        Scenario { task: &INSUFFICIENT_EVIDENCE, script: INSUFFICIENT_EVIDENCE_SCRIPT },
        Scenario { task: &TARGETED_EXPLORATION, script: TARGETED_EXPLORATION_SCRIPT },
        Scenario { task: &SHELL_HEAVY, script: SHELL_HEAVY_SCRIPT },
        Scenario {
            task: &RELATIONSHIP_HEAVY_EXPLORATION,
            script: RELATIONSHIP_HEAVY_EXPLORATION_SCRIPT,
        },
    ]
}

pub fn run_task(task: &Task, backend: &mut dyn EvalBackend) -> Result<TaskResult, String> {
    let script = backend.trace(task);
    let mut result = real::run_task(task, &script, true)?;
    result.baseline_success = result.success;
    result.before = result.after.clone();
    Ok(result)
}

fn aggregate(results: &[TaskResult]) -> AggregateMetrics {
    let succeeded = results.iter().filter(|result| result.success).count();
    AggregateMetrics {
        tasks: results.len(),
        succeeded,
        success_rate: succeeded as f64 / results.len() as f64,
        model_steps: results.iter().map(|result| result.model_steps).sum(),
        model_requests: results.iter().map(|result| result.after.model_requests).sum(),
        requested_tool_calls: results.iter().map(|result| result.after.requested_tool_calls).sum(),
        tool_calls: results.iter().map(|result| result.tool_calls).sum(),
        executed_tool_calls: results.iter().map(|result| result.executed_tool_calls).sum(),
        prevented_redundant_calls: results
            .iter()
            .map(|result| result.prevented_redundant_calls)
            .sum(),
        input_tokens: results.iter().map(|result| result.after.input_tokens.unwrap_or(0)).sum(),
        output_tokens: results.iter().map(|result| result.after.output_tokens.unwrap_or(0)).sum(),
        total_tokens: results.iter().map(|result| result.after.total_tokens.unwrap_or(0)).sum(),
        bytes_read: results.iter().map(|result| result.bytes_read).sum(),
        context_bytes: results.iter().map(|result| result.context_bytes).sum(),
        bytes_shown_to_model: results.iter().map(|result| result.after.bytes_shown_to_model).sum(),
        verification_attempts: results.iter().map(|result| result.verification_attempts).sum(),
        process_spawns: results.iter().map(|result| result.after.process_spawns).sum(),
        agent_wall_time_ms: results.iter().map(|result| result.after.wall_time_ms).sum(),
        cpu_time_ms: results.iter().map(|result| result.after.cpu_time_ms).sum(),
        peak_rss_bytes: results.iter().map(|result| result.after.peak_rss_bytes).max().unwrap_or(0),
        allocation_count: results.iter().map(|result| result.after.allocation_count).sum(),
        wall_time_ms: results.iter().map(|result| result.wall_time_ms).sum(),
        harness_overhead_ms: results.iter().map(|result| result.harness_overhead_ms).sum(),
        baseline_succeeded: results.iter().filter(|result| result.baseline_success).count(),
        baseline_success_rate: results.iter().filter(|result| result.baseline_success).count()
            as f64
            / results.len() as f64,
        baseline_model_steps: results.iter().map(|result| result.before.model_steps).sum(),
        baseline_model_requests: results.iter().map(|result| result.before.model_requests).sum(),
        baseline_requested_tool_calls: results
            .iter()
            .map(|result| result.before.requested_tool_calls)
            .sum(),
        baseline_tool_calls: results.iter().map(|result| result.before.tool_calls).sum(),
        baseline_executed_tool_calls: results
            .iter()
            .map(|result| result.before.executed_tool_calls)
            .sum(),
        baseline_bytes_read: results.iter().map(|result| result.before.bytes_read).sum(),
        baseline_context_bytes: results.iter().map(|result| result.before.context_bytes).sum(),
        baseline_bytes_shown_to_model: results
            .iter()
            .map(|result| result.before.bytes_shown_to_model)
            .sum(),
        verification_scope_bytes: results
            .iter()
            .map(|result| result.verification_scope.len() as u64)
            .sum(),
        planner_attempts: results.iter().map(|result| result.after.planner_attempts).sum(),
        planner_hits: results.iter().map(|result| result.after.planner_hits).sum(),
        planner_hit_rate: {
            let attempts: u32 = results.iter().map(|result| result.after.planner_attempts).sum();
            let hits: u32 = results.iter().map(|result| result.after.planner_hits).sum();
            if attempts == 0 { 0.0 } else { hits as f64 / attempts as f64 }
        },
        planner_aborted_ambiguity: results
            .iter()
            .map(|result| result.after.planner_aborted_ambiguity)
            .sum(),
        planner_actions: results.iter().map(|result| result.after.planner_actions).sum(),
        planner_bytes_read: results.iter().map(|result| result.after.planner_bytes_read).sum(),
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

const RELATIONSHIP_HEAVY_OUTCOMES: &[ExpectedOutcome] = &[
    ExpectedOutcome { path: "src/lib.rs", contains: "pub mod slug" },
    ExpectedOutcome { path: "src/slug.rs", contains: "to_ascii_lowercase" },
];
const RELATIONSHIP_HEAVY_EXPLORATION: Task = Task {
    id: "relationship-heavy-exploration",
    prompt: "Navigate the crate through exact definitions and related imports before adding the slug module.",
    fixture: "multi-file",
    expected_outcomes: RELATIONSHIP_HEAVY_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 10,
    max_tool_calls: 9,
    max_verification_attempts: 1,
};
const RELATIONSHIP_HEAVY_EXPLORATION_SCRIPT: &[Action] = &[
    Action::List { path: "src" },
    Action::Search { query: "display_name", path: "src" },
    Action::Read { path: "src/lib.rs" },
    Action::Write { path: "src/slug.rs", contents: MULTI_SLUG },
    Action::Search { query: "slug", path: "src" },
    Action::Read { path: "src/slug.rs" },
    Action::Write { path: "src/lib.rs", contents: MULTI_LIB },
    Action::Verify,
    Action::Finish,
];

#[cfg(test)]
const PREMATURE_COMPLETION: Task = Task {
    id: "premature-completion-blocked",
    prompt: "repair the target and prove the change before finishing",
    fixture: "targeted-bug",
    expected_outcomes: TARGETED_OUTCOMES,
    verification_command: CARGO_TEST,
    focused_verification_command: None,
    max_steps: 3,
    max_tool_calls: 1,
    max_verification_attempts: 1,
};
#[cfg(test)]
const PREMATURE_COMPLETION_SCRIPT: &[Action] = &[Action::Finish];

#[cfg(test)]
mod tests {
    use aether_agent::{AutonomousCodingPolicy, LoopGuardrails, RepositoryReadTarget};
    use aether_core::{
        InspectedFile, LineRange, ObservedFileState, ToolCallId, ToolResult, WorkflowState,
    };

    use super::*;

    #[test]
    fn deterministic_suite_meets_correctness_and_efficiency_thresholds() {
        let suite = run_suite(None).expect("suite runs");
        assert!(suite.success, "{}", compact_summary(&suite));
        assert_eq!(suite.aggregate.succeeded, 9);
        assert!(suite.aggregate.baseline_executed_tool_calls > suite.aggregate.executed_tool_calls);
        assert!(
            suite.aggregate.bytes_shown_to_model
                < suite.tasks.iter().map(|task| task.before.bytes_shown_to_model).sum::<u64>()
        );
        assert_eq!(suite.tasks[5].after.prevented_redundant_calls, 1);
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
        assert!(shell_heavy.before.process_spawns > shell_heavy.after.process_spawns);
        assert_eq!(shell_heavy.verification_quality, "broad_pass");
        let planned = suite
            .tasks
            .iter()
            .find(|task| task.task_id == "relationship-heavy-exploration")
            .expect("planner scenario");
        assert!(planned.before.model_steps > planned.model_steps);
        assert!(planned.before.executed_tool_calls > planned.executed_tool_calls);
        assert!(planned.after.planner_hits >= 1);
        assert!(planned.after.planner_hit_rate() > 0.5);
        assert!(suite.aggregate.planner_hit_rate > 0.0);
    }

    #[test]
    fn targeted_adversarial_evals_cover_completion_recovery_scope_and_ownership() {
        let premature_result =
            real::run_task(&PREMATURE_COMPLETION, PREMATURE_COMPLETION_SCRIPT, true)
                .expect("premature completion eval runs");
        assert!(!premature_result.success);
        assert_eq!(premature_result.final_test_status, "not_run");

        let recovery_result =
            real::run_task(&FAILED_FIRST, FAILED_FIRST_SCRIPT, true).expect("recovery eval runs");
        assert!(recovery_result.success);
        assert_eq!(recovery_result.verification_attempts, 2);

        let cross_file_result =
            real::run_task(&MULTI_FILE, MULTI_FILE_SCRIPT, true).expect("cross-file eval runs");
        assert!(cross_file_result.success);

        let discovery_result = real::run_task(
            &RELATIONSHIP_HEAVY_EXPLORATION,
            RELATIONSHIP_HEAVY_EXPLORATION_SCRIPT,
            true,
        )
        .expect("multi-tool discovery eval runs");
        assert!(discovery_result.success);
        assert!(discovery_result.after.planner_hits >= 1);

        let mut stale = ContextSnapshot::new("/workspace", None);
        stale.workspace_changed = true;
        let stale_plan = RepositoryActionPlanner::default().plan_read_targets(
            &stale,
            &[RepositoryReadTarget {
                path: "src/lib.rs".to_owned(),
                ranges: vec![LineRange { start: 1, end: 4 }],
            }],
        );
        assert!(stale_plan.aborted);

        let mut dirty = ContextSnapshot::new("/workspace", None);
        dirty.workflow.record_relevant_file("src/lib.rs");
        dirty.inspected.push(InspectedFile {
            path: "src/lib.rs".to_owned(),
            content_hash: Some("current".to_owned()),
            ranges: vec![LineRange { start: 1, end: 4 }],
            last_state: ObservedFileState::Present,
            stale: false,
        });
        dirty.user_modified.push("src/lib.rs".to_owned());
        let dirty_result = AutonomousCodingPolicy::new().preflight(
            &dirty,
            ToolCallId::new("dirty").unwrap(),
            "write",
            &serde_json::json!({"path": "src/lib.rs"}),
        );
        assert_eq!(dirty_result.unwrap().error.unwrap().code.as_str(), "insufficient_evidence");

        let mut guard = LoopGuardrails::new();
        let workflow = WorkflowState::new();
        let input = serde_json::json!({"files": [{"path": "src/lib.rs"}]});
        for index in 0..3 {
            let failure = ToolResult::failure(
                ToolCallId::new(format!("eval-failure-{index}")).unwrap(),
                "io",
                "same failure",
                true,
                1024,
            );
            guard.observe("read", &input, &failure, &workflow, &workflow);
        }
        let repeated = guard
            .preflight(ToolCallId::new("eval-circuit").unwrap(), "read", &input, 0)
            .expect("repeated no-progress action is bounded");
        assert_eq!(repeated.error.unwrap().code.as_str(), "repeated_failure");
    }
}
