//! Bounded trajectory analysis for offline evaluation.
//!
//! This module deliberately stays outside the production agent loop. It classifies the scripted
//! model decisions and supplies the optimized replay with only the conservative, exact
//! observation reuse that the trace makes measurable.

use std::collections::HashSet;

use serde::Serialize;

use super::Action;

const MAX_ANALYZED_ACTIONS: usize = 64;
const MAX_PATTERN_EXAMPLES: usize = 3;
const MAX_PATTERNS: usize = 16;

/// One bounded trajectory pattern observed in a task trace.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TrajectoryPattern {
    pub kind: String,
    pub count: u32,
    pub examples: Vec<String>,
}

/// Compact, bounded analysis of one model-decision trace.
#[derive(Clone, Debug, Default, Serialize)]
pub struct TrajectoryMetrics {
    pub analyzed_actions: u32,
    pub truncated: bool,
    pub reused_observations: u32,
    pub tool_turns: u32,
    pub completion_turns: u32,
    pub patterns: Vec<TrajectoryPattern>,
}

impl TrajectoryMetrics {
    pub(crate) fn analyze(
        script: &[Action],
        indices: &[usize],
        existing_write_targets: &HashSet<String>,
    ) -> Self {
        let mut result = Self {
            analyzed_actions: indices.len().min(MAX_ANALYZED_ACTIONS) as u32,
            truncated: indices.len() > MAX_ANALYZED_ACTIONS,
            ..Self::default()
        };
        let mut observations = HashSet::new();
        let mut inspected_paths = HashSet::new();
        let mut previous = None;
        let mut previous_previous = None;
        let mut verification_open = false;

        for &index in indices.iter().take(MAX_ANALYZED_ACTIONS) {
            let Some(action) = script.get(index) else { break };
            if matches!(action, Action::Finish) {
                result.completion_turns = result.completion_turns.saturating_add(1);
            } else {
                result.tool_turns = result.tool_turns.saturating_add(1);
            }

            if matches!(action, Action::Verify) {
                verification_open = true;
            } else if verification_open && matches!(action, Action::Write { .. }) {
                record(&mut result.patterns, "failed_verify_recovery", action_label(action));
                verification_open = false;
            }

            if let Action::Write { path, .. } = action
                && existing_write_targets.contains(*path)
                && !inspected_paths.contains(*path)
            {
                record(&mut result.patterns, "mutation_before_inspection", action_label(action));
            }
            if let Action::Read { path } = action {
                inspected_paths.insert((*path).to_owned());
            }

            if matches!(previous, Some(ActionKind::Search)) && matches!(action, Action::Read { .. })
            {
                record(&mut result.patterns, "search_read", action_label(action));
            }
            if matches!(previous_previous, Some(ActionKind::Search))
                && matches!(previous, Some(ActionKind::Read))
                && matches!(action, Action::Read { .. })
            {
                record(&mut result.patterns, "search_read_read", action_label(action));
            }
            if matches!(previous, Some(ActionKind::ShellGitRead))
                && matches!(action, Action::Read { .. })
            {
                record(&mut result.patterns, "git_read", action_label(action));
            }

            if is_repeatable_observation(action) {
                let fingerprint = action_fingerprint(action);
                if !observations.insert(fingerprint) {
                    record(&mut result.patterns, "repeated_observation", action_label(action));
                    match action {
                        Action::Read { .. } => {
                            record(&mut result.patterns, "repeated_read", action_label(action));
                        }
                        Action::List { .. } => {
                            record(&mut result.patterns, "repeated_list", action_label(action));
                        }
                        Action::Search { .. } | Action::Shell { .. } => {
                            record(
                                &mut result.patterns,
                                "repeated_search_or_shell",
                                action_label(action),
                            );
                        }
                        _ => {}
                    }
                }
            } else if matches!(action, Action::Write { .. } | Action::Verify) {
                observations.clear();
                if let Action::Write { path, .. } = action {
                    inspected_paths.remove(*path);
                }
            }

            previous_previous = previous;
            previous = Some(action_kind(action));
        }
        result.reused_observations = script.len().saturating_sub(indices.len()) as u32;
        result
    }
}

/// Return the action indices retained by optimized deterministic replay.
pub(crate) fn retained_indices(script: &[Action], optimize: bool) -> Vec<usize> {
    let mut retained = Vec::with_capacity(script.len());
    let mut inspected = HashSet::new();
    let mut observations = HashSet::new();
    let mut planner_supplied_read = false;

    for (index, action) in script.iter().enumerate() {
        let planner_read = optimize
            && planner_supplied_read
            && matches!(action, Action::Read { path } if inspected.contains(*path));
        let repeated = optimize
            && is_repeatable_observation(action)
            && !observations.insert(action_fingerprint(action));
        if planner_read || repeated {
            planner_supplied_read = false;
            continue;
        }

        retained.push(index);
        match action {
            Action::Read { path } => {
                inspected.insert((*path).to_owned());
            }
            Action::Write { path, .. } => {
                // A successful write exposes the new state in its bounded result. The replay
                // uses that same evidence to make an immediate exact read unnecessary.
                inspected.insert((*path).to_owned());
                observations.clear();
            }
            Action::Verify => observations.clear(),
            _ => {}
        }
        planner_supplied_read = matches!(action, Action::Search { .. });
    }
    retained
}

fn record(patterns: &mut Vec<TrajectoryPattern>, kind: &str, example: String) {
    if let Some(pattern) = patterns.iter_mut().find(|pattern| pattern.kind == kind) {
        pattern.count = pattern.count.saturating_add(1);
        if pattern.examples.len() < MAX_PATTERN_EXAMPLES && !pattern.examples.contains(&example) {
            pattern.examples.push(example);
        }
        return;
    }
    if patterns.len() == MAX_PATTERNS {
        return;
    }
    patterns.push(TrajectoryPattern { kind: kind.to_owned(), count: 1, examples: vec![example] });
}

fn action_label(action: &Action) -> String {
    match action {
        Action::List { path } | Action::Read { path } => (*path).to_owned(),
        Action::Search { query, path } => format!("{query} @ {path}"),
        Action::Write { path, .. } => (*path).to_owned(),
        Action::Shell { program, args } => format!("{} {}", program, args.join(" ")),
        Action::Verify => "verification".to_owned(),
        Action::Finish => "finish".to_owned(),
    }
}

fn action_fingerprint(action: &Action) -> String {
    match action {
        Action::List { path } => format!("list:{path}"),
        Action::Read { path } => format!("read:{path}"),
        Action::Search { query, path } => format!("search:{query}:{path}"),
        Action::Shell { program, args } => format!("shell:{program}:{}", args.join("\0")),
        Action::Write { .. } | Action::Verify | Action::Finish => String::new(),
    }
}

fn is_repeatable_observation(action: &Action) -> bool {
    match action {
        Action::List { .. } | Action::Read { .. } | Action::Search { .. } => true,
        Action::Shell { program, args } => {
            let command = aether_core::analyze_command(
                program,
                &args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(),
                ".",
            );
            command.class.is_read_only() && !command.uncertain && !command.mutates_workspace()
        }
        Action::Write { .. } | Action::Verify | Action::Finish => false,
    }
}

#[derive(Clone, Copy)]
enum ActionKind {
    List,
    Read,
    Search,
    ShellGitRead,
    Other,
}

fn action_kind(action: &Action) -> ActionKind {
    match action {
        Action::List { .. } => ActionKind::List,
        Action::Read { .. } => ActionKind::Read,
        Action::Search { .. } => ActionKind::Search,
        Action::Shell { program, args }
            if program == &"git"
                && args
                    .first()
                    .is_some_and(|argument| *argument == "status" || *argument == "diff") =>
        {
            ActionKind::ShellGitRead
        }
        _ => ActionKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_read_is_reported_and_reused_only_when_state_is_unchanged() {
        let script = [
            Action::Read { path: "src/lib.rs" },
            Action::Read { path: "src/lib.rs" },
            Action::Write { path: "src/lib.rs", contents: "fixed" },
            Action::Read { path: "src/lib.rs" },
        ];
        let baseline = retained_indices(&script, false);
        let optimized = retained_indices(&script, true);
        let analysis = TrajectoryMetrics::analyze(
            &script,
            &baseline,
            &HashSet::from(["src/lib.rs".to_owned()]),
        );

        assert_eq!(optimized, vec![0, 2, 3]);
        assert_eq!(analysis.reused_observations, 0);
        assert!(
            analysis
                .patterns
                .iter()
                .any(|pattern| { pattern.kind == "repeated_read" && pattern.count == 1 })
        );
    }

    #[test]
    fn read_only_shell_repeats_are_bounded_but_mutating_commands_are_not_reused() {
        let script = [
            Action::Shell { program: "rg", args: &["needle", "src"] },
            Action::Shell { program: "rg", args: &["needle", "src"] },
            Action::Shell { program: "cargo", args: &["fmt"] },
            Action::Shell { program: "cargo", args: &["fmt"] },
        ];
        let optimized = retained_indices(&script, true);

        assert_eq!(optimized, vec![0, 2, 3]);
    }
}
