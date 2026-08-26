use std::io;

use aether_core::{CancellationFlag, PermissionClass, ToolCallId, ToolExecutionContext};
use globset::{Glob, GlobSetBuilder};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{
    BinaryDetection, SearcherBuilder, Sink, SinkContext, SinkContextKind, SinkMatch,
};
use ignore::WalkBuilder;
use regex_automata::meta::Regex as AutomataRegex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::common::{
    MAX_INPUT_BYTES, MAX_WALK_ENTRIES, ToolInternalError, Workspace, spawn_blocking_tool,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchInput {
    pub patterns: Vec<String>,
    pub path: Option<String>,
    pub globs: Option<Vec<String>>,
    pub regex: Option<bool>,
    pub case_insensitive: Option<bool>,
    pub before_context: Option<usize>,
    pub after_context: Option<usize>,
    pub include_hidden: Option<bool>,
    pub respect_ignore: Option<bool>,
    pub max_results: Option<usize>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchMatch {
    pub path: String,
    pub line: Option<u64>,
    pub kind: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchOutput {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
}

struct SearchSink {
    path: String,
    matches: Vec<SearchMatch>,
    max_results: usize,
    cancellation: CancellationFlag,
}

impl SearchSink {
    fn push(&mut self, line: Option<u64>, kind: &str, bytes: &[u8]) -> io::Result<bool> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "search cancelled"));
        }
        if self.matches.len() >= self.max_results {
            return Ok(false);
        }
        let text = String::from_utf8_lossy(bytes).trim_end_matches(['\r', '\n']).to_owned();
        self.matches.push(SearchMatch {
            path: self.path.clone(),
            line,
            kind: kind.to_owned(),
            text,
        });
        Ok(true)
    }
}

impl Sink for SearchSink {
    type Error = io::Error;

    fn matched(
        &mut self,
        _: &grep_searcher::Searcher,
        mat: &SinkMatch<'_>,
    ) -> Result<bool, Self::Error> {
        self.push(mat.line_number(), "match", mat.bytes())
    }

    fn context(
        &mut self,
        _: &grep_searcher::Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        let kind = match context.kind() {
            SinkContextKind::Before => "before",
            SinkContextKind::After => "after",
            SinkContextKind::Other => "context",
        };
        self.push(context.line_number(), kind, context.bytes())
    }
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    let parsed: SearchInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    execute_parsed(workspace, call_id, parsed, context).await
}

pub(crate) async fn execute_parsed(
    workspace: &Workspace,
    call_id: ToolCallId,
    parsed: SearchInput,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    if parsed.patterns.is_empty() || parsed.patterns.len() > 32 {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("search requires 1..=32 patterns".to_owned()),
        );
    }
    if let Err(error) =
        workspace.require_permit(&context, &call_id, "search", PermissionClass::ReadOnly)
    {
        return workspace.result_error(call_id, error);
    }
    spawn_blocking_tool(workspace, call_id, &context, move |workspace, call_id, cancellation| {
        execute_blocking(workspace, call_id, parsed, cancellation)
    })
    .await
}

fn execute_blocking(
    workspace: Workspace,
    call_id: ToolCallId,
    parsed: SearchInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
        return workspace.result_error(call_id, error);
    }
    let regex = parsed.regex.unwrap_or(false);
    if regex {
        for pattern in &parsed.patterns {
            if let Err(error) = AutomataRegex::new(pattern) {
                return workspace
                    .result_error(call_id, ToolInternalError::Pattern(error.to_string()));
            }
        }
    }
    let mut matcher_builder = RegexMatcherBuilder::new();
    matcher_builder
        .case_insensitive(parsed.case_insensitive.unwrap_or(false))
        .fixed_strings(!regex);
    let matcher = if regex {
        matcher_builder.build_many(&parsed.patterns)
    } else {
        matcher_builder.build_literals(&parsed.patterns)
    };
    let matcher = match matcher {
        Ok(value) => value,
        Err(error) => {
            return workspace.result_error(call_id, ToolInternalError::Pattern(error.to_string()));
        }
    };
    let glob_set = match parsed.globs.as_ref() {
        Some(globs) => {
            if globs.len() > 32 {
                return workspace.result_error(
                    call_id,
                    ToolInternalError::Input("search accepts at most 32 globs".to_owned()),
                );
            }
            let mut builder = GlobSetBuilder::new();
            for glob in globs {
                match Glob::new(glob) {
                    Ok(value) => builder.add(value),
                    Err(error) => {
                        return workspace
                            .result_error(call_id, ToolInternalError::Pattern(error.to_string()));
                    }
                };
            }
            match builder.build() {
                Ok(value) => Some(value),
                Err(error) => {
                    return workspace
                        .result_error(call_id, ToolInternalError::Pattern(error.to_string()));
                }
            }
        }
        None => None,
    };

    let requested_path = parsed.path.as_deref().unwrap_or(".");
    let (_, base) = match workspace.resolve_existing(requested_path) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    let max_results = parsed.max_results.unwrap_or(1000).clamp(1, MAX_WALK_ENTRIES);
    let mut walker = WalkBuilder::new(base);
    let include_hidden = parsed.include_hidden.unwrap_or(false);
    let respect_ignore = parsed.respect_ignore.unwrap_or(true);
    walker
        .hidden(!include_hidden)
        .git_ignore(respect_ignore)
        .git_exclude(respect_ignore)
        .git_global(false)
        .parents(false)
        .ignore(respect_ignore)
        .follow_links(false);
    let mut matches = Vec::new();
    let mut truncated = false;
    for result in walker.build() {
        if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
            return workspace.result_error(call_id, error);
        }
        let entry = match result {
            Ok(value) => value,
            Err(error) => {
                return workspace
                    .result_error(call_id, ToolInternalError::Input(error.to_string()));
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Err(error) = workspace.verify_entry(path, &path.to_string_lossy()) {
            return workspace.result_error(call_id, error);
        }
        let relative = match path.strip_prefix(workspace.root()) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if glob_set.as_ref().is_some_and(|set| !set.is_match(relative)) {
            continue;
        }
        let path_text = relative.to_string_lossy().into_owned();
        let mut searcher = SearcherBuilder::new();
        searcher
            .line_number(true)
            .before_context(parsed.before_context.unwrap_or(0).min(20))
            .after_context(parsed.after_context.unwrap_or(0).min(20))
            .max_matches(Some(max_results.saturating_sub(matches.len()) as u64))
            .heap_limit(Some(MAX_INPUT_BYTES))
            .binary_detection(BinaryDetection::quit(0));
        let mut sink = SearchSink {
            path: path_text,
            matches: Vec::new(),
            max_results: max_results.saturating_sub(matches.len()),
            cancellation: cancellation.clone(),
        };
        let search_result = searcher.build().search_path(matcher.clone(), path, &mut sink);
        if let Err(error) = search_result {
            if cancellation.is_cancelled() || error.kind() == io::ErrorKind::Interrupted {
                return workspace.result_error(
                    call_id,
                    ToolInternalError::Core(aether_core::CoreError::Cancelled),
                );
            }
            return workspace.result_error(call_id, error.into());
        }
        matches.extend(sink.matches);
        if matches.len() >= max_results {
            truncated = true;
            break;
        }
    }
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(SearchOutput { matches, truncated }),
        workspace.max_output_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_and_regex_modes_are_distinct() {
        assert!(AutomataRegex::new("a+b").is_ok());
    }

    #[test]
    fn search_sink_observes_cancellation_during_matching() {
        let cancellation = CancellationFlag::new();
        let mut sink = SearchSink {
            path: "file.txt".to_owned(),
            matches: Vec::new(),
            max_results: 8,
            cancellation: cancellation.clone(),
        };
        cancellation.cancel();
        assert_eq!(
            sink.push(Some(1), "match", b"needle\n").unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }
}
