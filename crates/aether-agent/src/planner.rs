//! Bounded, deterministic planning for high-confidence repository observations.
//!
//! The planner is deliberately smaller than a general search engine.  It consumes repository
//! metadata and already retained policy/context evidence, chooses at most a few exact files, and
//! emits read-only actions.  A separate executor performs the bounded reads so planning itself
//! remains allocation- and filesystem-light when current excerpts already answer the request.

use std::{
    fmt::Write as _,
    fs::{self, File},
    io::{self, Read},
    path::Path,
    time::{Duration, Instant},
};

use aether_core::{
    BoundedText, CommandEffects, ContextSnapshot, InspectedFile, LineRange, MAX_EXCERPT_BYTES,
    ObservedFileState, ToolCallId, ToolResult, WorkspacePath,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;

use crate::{
    RepoMap, RepoMapSnapshot, RepoSelection, RepoSelectionKind, RepoSymbolSelection, Symbol,
    SymbolKind, SymbolMatch, SymbolRelationshipKind, SymbolRelationshipMatch,
};

/// Minimum confidence required before a repository observation is planned automatically.
pub const MIN_PLANNER_CONFIDENCE: u8 = 80;
/// Maximum logical exploration depth represented by one collapsed plan.
pub const DEFAULT_MAX_PLANNER_DEPTH: usize = 3;
/// Maximum repository candidates considered by one plan.
pub const DEFAULT_MAX_PLANNER_CANDIDATES: usize = 8;
/// Maximum files read by one plan.
pub const DEFAULT_MAX_PLANNER_FILES: usize = 4;
/// Maximum bytes retained by one executed plan.
pub const DEFAULT_MAX_PLANNER_BYTES: usize = 12 * 1024;
/// Maximum physical read actions emitted by one plan.
pub const DEFAULT_MAX_PLANNER_ACTIONS: usize = 4;
/// Maximum execution time for one bounded local plan.
pub const DEFAULT_MAX_PLANNER_EXECUTION_MICROS: u64 = 10_000;
/// Maximum bytes retained for the structured planner observation.
pub const MAX_PLANNER_OBSERVATION_BYTES: usize = 16 * 1024;

/// Hard ceilings for planning and local observation execution.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlannerLimits {
    pub max_depth: usize,
    pub max_candidates: usize,
    pub max_files: usize,
    pub max_bytes: usize,
    pub max_actions: usize,
    pub max_execution_micros: u64,
}

impl Default for PlannerLimits {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_MAX_PLANNER_DEPTH,
            max_candidates: DEFAULT_MAX_PLANNER_CANDIDATES,
            max_files: DEFAULT_MAX_PLANNER_FILES,
            max_bytes: DEFAULT_MAX_PLANNER_BYTES,
            max_actions: DEFAULT_MAX_PLANNER_ACTIONS,
            max_execution_micros: DEFAULT_MAX_PLANNER_EXECUTION_MICROS,
        }
    }
}

impl PlannerLimits {
    fn normalized(self) -> Self {
        Self {
            max_depth: self.max_depth.clamp(1, DEFAULT_MAX_PLANNER_DEPTH),
            max_candidates: self.max_candidates.clamp(1, DEFAULT_MAX_PLANNER_CANDIDATES),
            max_files: self.max_files.clamp(1, DEFAULT_MAX_PLANNER_FILES),
            max_bytes: self.max_bytes.clamp(1, DEFAULT_MAX_PLANNER_BYTES),
            max_actions: self.max_actions.clamp(1, DEFAULT_MAX_PLANNER_ACTIONS),
            max_execution_micros: self
                .max_execution_micros
                .clamp(1, DEFAULT_MAX_PLANNER_EXECUTION_MICROS),
        }
    }

    fn execution_budget(self) -> Duration {
        Duration::from_micros(self.max_execution_micros)
    }
}

/// Read-only repository actions.  There is intentionally no mutation variant.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryAction {
    /// Read one exact file and only the selected line ranges.
    Read { path: String, ranges: Vec<LineRange> },
}

/// The kind of model request that may be eligible for local planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RepositoryRequestKind {
    Search,
    Find,
    List,
    Read,
}

/// One exact read request eligible for bounded local coalescing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryReadTarget {
    pub path: String,
    pub ranges: Vec<LineRange>,
}

/// Inputs to the pure planning step.  All slices are caller-owned bounded evidence.
#[derive(Clone, Copy, Debug)]
pub struct RepositoryPlanRequest<'a> {
    pub kind: RepositoryRequestKind,
    pub query: &'a str,
    pub context: &'a ContextSnapshot,
    pub repo: Option<&'a RepoMapSnapshot>,
    pub selections: &'a [RepoSelection],
    pub symbols: &'a [SymbolMatch],
    pub relationships: &'a [SymbolRelationshipMatch],
    pub command_effects: Option<&'a CommandEffects>,
}

impl<'a> RepositoryPlanRequest<'a> {
    #[must_use]
    pub fn new(kind: RepositoryRequestKind, query: &'a str, context: &'a ContextSnapshot) -> Self {
        Self {
            kind,
            query,
            context,
            repo: None,
            selections: &[],
            symbols: &[],
            relationships: &[],
            command_effects: None,
        }
    }

    #[must_use]
    pub fn with_repo(mut self, repo: &'a RepoMapSnapshot) -> Self {
        self.repo = Some(repo);
        self
    }

    #[must_use]
    pub fn with_selections(mut self, selections: &'a [RepoSelection]) -> Self {
        self.selections = selections;
        self
    }

    #[must_use]
    pub fn with_symbols(mut self, symbols: &'a [SymbolMatch]) -> Self {
        self.symbols = symbols;
        self
    }

    #[must_use]
    pub fn with_relationships(mut self, relationships: &'a [SymbolRelationshipMatch]) -> Self {
        self.relationships = relationships;
        self
    }

    #[must_use]
    pub fn with_command_effects(mut self, effects: &'a CommandEffects) -> Self {
        self.command_effects = Some(effects);
        self
    }
}

/// One file retained in the compact observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryFileObservation {
    pub path: String,
    pub score: u16,
    pub inspected: bool,
    pub stale: bool,
}

/// One exact symbol or relationship retained in the compact observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositorySymbolObservation {
    pub path: String,
    pub name: Option<String>,
    pub kind: String,
    pub relationship: Option<String>,
    pub line: Option<usize>,
    pub end_line: Option<usize>,
    pub score: u16,
}

/// One bounded current excerpt retained in the observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryExcerpt {
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: BoundedText,
    pub content_hash: Option<String>,
    pub truncated: bool,
}

/// One compact structured repository observation.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepositoryObservation {
    pub files: Vec<RepositoryFileObservation>,
    pub symbols: Vec<RepositorySymbolObservation>,
    pub excerpts: Vec<RepositoryExcerpt>,
    pub confidence: u8,
    pub ambiguity: Option<BoundedText>,
    pub bytes_read: usize,
    pub files_read: usize,
    pub cache_hit: bool,
    pub collapsed: bool,
}

impl RepositoryObservation {
    /// Convert the observation into a normal successful tool result for context folding.
    pub fn into_tool_result(self, call_id: ToolCallId) -> ToolResult {
        let matches = self
            .symbols
            .iter()
            .map(|symbol| {
                json!({
                    "path": symbol.path,
                    "line": symbol.line,
                    "kind": symbol.relationship.as_deref().unwrap_or(symbol.kind.as_str()),
                    "text": symbol.name.as_deref().unwrap_or("repository symbol")
                })
            })
            .collect::<Vec<_>>();
        let files = self
            .excerpts
            .iter()
            .map(|excerpt| {
                let lines = excerpt
                    .text
                    .as_str()
                    .lines()
                    .enumerate()
                    .map(|(offset, text)| {
                        json!({
                            "number": excerpt.start_line.saturating_add(offset),
                            "text": text
                        })
                    })
                    .collect::<Vec<_>>();
                json!({
                    "path": excerpt.path,
                    "binary": false,
                    "truncated": excerpt.truncated,
                    "bytes_read": excerpt.text.as_str().len(),
                    "content_hash": excerpt.content_hash,
                    "lines": lines
                })
            })
            .collect::<Vec<_>>();
        let mut model_output = String::new();
        let _ = writeln!(
            &mut model_output,
            "repository observation: confidence={} files={} read={}B cache_hit={}",
            self.confidence,
            self.files.len(),
            self.bytes_read,
            self.cache_hit
        );
        for symbol in &self.symbols {
            let _ = writeln!(
                &mut model_output,
                "symbol: {} {} {}",
                symbol.path,
                symbol.line.map_or_else(|| "-".to_owned(), |line| line.to_string()),
                symbol.name.as_deref().unwrap_or("repository symbol")
            );
        }
        for excerpt in &self.excerpts {
            let _ = writeln!(
                &mut model_output,
                "file: {}:{}-{}",
                excerpt.path, excerpt.start_line, excerpt.end_line
            );
            model_output.push_str(excerpt.text.as_str());
            if !excerpt.text.as_str().ends_with('\n') {
                model_output.push('\n');
            }
        }
        if let Some(ambiguity) = &self.ambiguity {
            let _ = writeln!(&mut model_output, "ambiguity: {}", ambiguity.as_str());
        }
        // The excerpts are represented once in `files.lines`; retaining them a second time in
        // the planner metadata needlessly inflates the next model request.
        let metadata = Self {
            files: self.files,
            symbols: self.symbols,
            excerpts: Vec::new(),
            confidence: self.confidence,
            ambiguity: self.ambiguity,
            bytes_read: self.bytes_read,
            files_read: self.files_read,
            cache_hit: self.cache_hit,
            collapsed: self.collapsed,
        };
        let value = json!({
            "repository_observation": metadata,
            "matches": matches,
            "files": files,
            "truncated": false
        });
        ToolResult {
            call_id,
            ok: true,
            output: BoundedText::new(&model_output, MAX_PLANNER_OBSERVATION_BYTES),
            data: Some(value),
            error: None,
        }
    }
}

/// A deterministic plan and its immediate evidence/ambiguity decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryActionPlan {
    pub actions: Vec<RepositoryAction>,
    pub observation: RepositoryObservation,
    pub logical_depth: usize,
    pub aborted: bool,
    pub abort_reason: Option<BoundedText>,
}

impl RepositoryActionPlan {
    #[must_use]
    pub fn is_usable(&self) -> bool {
        !self.aborted
            && (self.observation.cache_hit
                || !self.actions.is_empty()
                || !self.observation.files.is_empty())
    }

    #[must_use]
    pub fn is_ambiguous(&self) -> bool {
        self.abort_reason.is_some()
    }
}

/// Failures from bounded local observation execution.
#[derive(Debug, Error)]
pub enum PlannerExecutionError {
    #[error("repository planner cancelled")]
    Cancelled,
    #[error("repository planner execution budget exceeded")]
    TimeBudget,
    #[error("repository planner path is invalid: {0}")]
    InvalidPath(String),
    #[error("repository planner could not read a file: {0}")]
    Io(#[from] io::Error),
}

/// Pure planner plus a bounded executor for its read-only actions.
#[derive(Clone, Copy, Debug)]
pub struct RepositoryActionPlanner {
    limits: PlannerLimits,
}

impl Default for RepositoryActionPlanner {
    fn default() -> Self {
        Self::new(PlannerLimits::default())
    }
}

impl RepositoryActionPlanner {
    #[must_use]
    pub fn new(limits: PlannerLimits) -> Self {
        Self { limits: limits.normalized() }
    }

    #[must_use]
    pub fn limits(&self) -> PlannerLimits {
        self.limits
    }

    /// Plan exact read-only requests without requiring a model search round-trip.
    #[must_use]
    pub fn plan_read_targets(
        &self,
        context: &ContextSnapshot,
        targets: &[RepositoryReadTarget],
    ) -> RepositoryActionPlan {
        let mut plan = empty_plan(self.limits.max_depth);
        if context.workspace_changed || targets.is_empty() {
            return abort_plan(
                plan,
                if context.workspace_changed {
                    "workspace changed; exact reads require fresh evidence"
                } else {
                    "exact read request is empty"
                },
            );
        }
        let mut seen = Vec::new();
        for target in targets.iter().take(self.limits.max_candidates) {
            let Ok(relative) = WorkspacePath::new(&target.path) else {
                return abort_plan(plan, "exact read path is outside the workspace");
            };
            let path = relative.display();
            if path.is_empty() || seen.iter().any(|existing: &String| existing == &path) {
                continue;
            }
            if seen.len() >= self.limits.max_files {
                break;
            }
            let ranges = normalize_ranges(&target.ranges);
            let inspected = context.inspected.iter().find(|file| file.path == path);
            plan.observation.files.push(RepositoryFileObservation {
                path: path.clone(),
                score: 100,
                inspected: inspected.is_some_and(is_current_file),
                stale: inspected.is_some_and(|file| file.stale),
            });
            if inspected.is_some_and(|file| ranges_covered(file, &ranges))
                && let Some(excerpt) = current_excerpt(context, &path, &ranges)
            {
                push_excerpt(&mut plan.observation, excerpt, self.limits.max_files);
                seen.push(path);
                continue;
            }
            if plan.actions.len() < self.limits.max_actions {
                plan.actions.push(RepositoryAction::Read { path: path.clone(), ranges });
            }
            seen.push(path);
        }
        if plan.actions.is_empty() && !plan.observation.cache_hit {
            return abort_plan(plan, "no bounded exact read survived selection");
        }
        plan.observation.collapsed = true;
        plan.aborted = false;
        plan
    }

    /// Merge compatible read-only plans into one bounded physical execution plan.
    #[must_use]
    pub fn coalesce(&self, plans: &[RepositoryActionPlan]) -> Option<RepositoryActionPlan> {
        if plans.is_empty() || plans.iter().any(|plan| plan.aborted) {
            return None;
        }
        let mut merged = empty_plan(self.limits.max_depth);
        merged.aborted = false;
        merged.observation.collapsed = true;
        for plan in plans {
            merged.observation.confidence =
                merged.observation.confidence.max(plan.observation.confidence);
            merged.observation.cache_hit |= plan.observation.cache_hit;
            for file in &plan.observation.files {
                if !merged.observation.files.iter().any(|existing| existing.path == file.path) {
                    merged.observation.files.push(file.clone());
                }
            }
            for symbol in &plan.observation.symbols {
                if !merged.observation.symbols.iter().any(|existing| {
                    existing.path == symbol.path
                        && existing.name == symbol.name
                        && existing.relationship == symbol.relationship
                }) {
                    merged.observation.symbols.push(symbol.clone());
                }
            }
            for excerpt in &plan.observation.excerpts {
                push_excerpt(&mut merged.observation, excerpt.clone(), self.limits.max_files);
            }
            merged.observation.bytes_read =
                merged.observation.bytes_read.saturating_add(plan.observation.bytes_read);
            merged.observation.files_read =
                merged.observation.files_read.saturating_add(plan.observation.files_read);
            for action in &plan.actions {
                if merged.actions.len() >= self.limits.max_actions {
                    break;
                }
                if !merged.actions.iter().any(|existing| existing == action) {
                    merged.actions.push(action.clone());
                }
            }
        }
        merged.observation.files.truncate(self.limits.max_candidates);
        merged.observation.symbols.truncate(self.limits.max_candidates);
        if merged.actions.is_empty() && !merged.observation.cache_hit {
            return None;
        }
        Some(merged)
    }

    /// Plan from bounded indexed/context evidence without touching the filesystem.
    #[must_use]
    pub fn plan(&self, request: RepositoryPlanRequest<'_>) -> RepositoryActionPlan {
        let mut plan = empty_plan(self.limits.max_depth);
        if request.kind != RepositoryRequestKind::Search {
            return abort_plan(plan, "request is not a high-confidence search exploration");
        }
        if request.query.trim().is_empty() {
            return abort_plan(plan, "search query is empty");
        }
        if request.context.workspace_changed {
            return abort_plan(plan, "workspace changed; cached repository evidence is stale");
        }
        if request.context.workflow.decision.low_value_observations >= 3 {
            return abort_plan(plan, "policy reports repeated low-value observations");
        }
        if request
            .command_effects
            .is_some_and(|effects| effects.uncertain || !effects.class.is_read_only())
        {
            return abort_plan(plan, "command intelligence is uncertain or not read-only");
        }

        let terms = query_terms(request.query);
        if terms.is_empty() {
            return abort_plan(plan, "search query has no bounded identifier terms");
        }

        let mut candidates = Vec::with_capacity(self.limits.max_candidates);
        let mut add_candidate = |candidate: Candidate| {
            if candidate.path.is_empty() {
                return;
            }
            if let Some(existing) = candidates
                .iter_mut()
                .find(|existing: &&mut Candidate| existing.path == candidate.path)
            {
                existing.score = existing.score.max(candidate.score);
                existing.ambiguous |= candidate.ambiguous;
                existing.related |= candidate.related;
                if existing.symbol.is_none() {
                    existing.symbol = candidate.symbol;
                }
                if existing.line.is_none() {
                    existing.line = candidate.line;
                }
                return;
            }
            if candidates.len() >= self.limits.max_candidates {
                return;
            }
            candidates.push(candidate);
        };

        for symbol in request.symbols.iter().take(self.limits.max_candidates) {
            add_candidate(Candidate::from_symbol(symbol, None));
        }
        for relationship in request.relationships.iter().take(self.limits.max_candidates) {
            add_candidate(Candidate::from_relationship(relationship));
        }
        for selection in request.selections.iter().take(self.limits.max_candidates) {
            add_candidate(Candidate::from_selection(selection));
        }
        for candidate in &request.context.workflow.decision.candidate_files {
            let detail = request
                .context
                .workflow
                .decision
                .evidence
                .iter()
                .rev()
                .find(|evidence| evidence.path.as_str() == candidate.path.as_str())
                .map(|evidence| evidence.detail.as_str());
            add_candidate(Candidate::from_decision(candidate, detail));
        }
        if let Some(repo) = request.repo {
            for selection in repo.select(request.query, self.limits.max_candidates) {
                add_candidate(Candidate::from_selection(&selection));
            }
        }

        candidates.sort_unstable_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.line.cmp(&right.line))
        });
        let Some(top) = candidates.first().cloned() else {
            return abort_plan(plan, "no indexed candidate matched the exploration request");
        };
        if !candidate_matches_terms(&top, &terms) {
            return abort_plan(plan, "indexed evidence does not match the search request");
        }
        let second = candidates.iter().find(|candidate| candidate.path != top.path);
        let confidence = confidence(top.score, second.map(|candidate| candidate.score));
        plan.observation.confidence = confidence;

        if top.ambiguous {
            return abort_plan_with_confidence(
                plan,
                confidence,
                "top repository relationship is explicitly ambiguous",
            );
        }
        if second.is_some_and(|candidate| candidate.score.saturating_add(16) >= top.score) {
            return abort_plan_with_confidence(
                plan,
                confidence,
                "top repository candidates are too close to resolve safely",
            );
        }
        if confidence < MIN_PLANNER_CONFIDENCE {
            return abort_plan_with_confidence(
                plan,
                confidence,
                "repository evidence is below the strict planning threshold",
            );
        }

        let current = current_context_paths(request.context);
        let mut selected = Vec::new();
        for candidate in candidates.iter().take(self.limits.max_candidates) {
            let related = candidate.path != top.path
                && (candidate.related
                    || is_test_path(&candidate.path)
                    || same_stem(&candidate.path, &top.path));
            if candidate.path != top.path && !related {
                continue;
            }
            if selected.len() >= self.limits.max_files {
                break;
            }
            selected.push(candidate.clone());
        }
        if selected.is_empty() {
            return abort_plan_with_confidence(
                plan,
                confidence,
                "no bounded file survived selection",
            );
        }

        for candidate in selected {
            let inspected = current.iter().find(|file| file.path == candidate.path);
            let ranges = candidate_ranges(&candidate, inspected);
            let file_observation = RepositoryFileObservation {
                path: candidate.path.clone(),
                score: candidate.score.min(u16::MAX as usize) as u16,
                inspected: inspected.is_some_and(is_current_file),
                stale: inspected.is_some_and(|file| file.stale),
            };
            plan.observation.files.push(file_observation);
            if let Some(symbol) = candidate.symbol.as_ref() {
                plan.observation.symbols.push(symbol.clone());
            }
            if inspected.is_some_and(|file| ranges_covered(file, &ranges)) {
                if let Some(excerpt) = current_excerpt(request.context, &candidate.path, &ranges) {
                    push_excerpt(&mut plan.observation, excerpt, self.limits.max_files);
                }
                plan.observation.cache_hit = true;
                continue;
            }
            if plan.actions.len() >= self.limits.max_actions {
                break;
            }
            plan.actions.push(RepositoryAction::Read { path: candidate.path, ranges });
        }

        plan.logical_depth = self.limits.max_depth;
        plan.observation.collapsed = true;
        plan.aborted = false;
        plan
    }

    /// Execute a plan with containment, bytes, file, time, and cancellation bounds.
    pub fn execute(
        &self,
        plan: &RepositoryActionPlan,
        repo: &RepoMap,
        context: &ContextSnapshot,
        cancellation: &crate::CancellationToken,
    ) -> Result<RepositoryObservation, PlannerExecutionError> {
        if plan.aborted {
            return Ok(plan.observation.clone());
        }
        if plan.actions.is_empty() {
            return Ok(plan.observation.clone());
        }
        let started = Instant::now();
        let mut observation = plan.observation.clone();
        let mut bytes_left = self.limits.max_bytes.saturating_sub(observation.bytes_read);
        let mut files_read = 0;
        for action in plan.actions.iter().take(self.limits.max_actions) {
            check_budget(started, self.limits.execution_budget(), cancellation)?;
            let RepositoryAction::Read { path, ranges } = action;
            if files_read >= self.limits.max_files || bytes_left == 0 {
                break;
            }
            if let Some(cached) = current_excerpt(context, path, ranges) {
                push_excerpt(&mut observation, cached, self.limits.max_files);
                continue;
            }
            let excerpt = read_excerpt(
                repo.root(),
                path,
                ranges,
                bytes_left,
                started,
                self.limits.execution_budget(),
                cancellation,
            )?;
            bytes_left = bytes_left.saturating_sub(excerpt.text.as_str().len());
            observation.bytes_read =
                observation.bytes_read.saturating_add(excerpt.text.as_str().len());
            observation.excerpts.push(excerpt);
            files_read += 1;
            observation.files_read = files_read;
        }
        observation.excerpts.truncate(self.limits.max_files);
        Ok(observation)
    }
}

#[derive(Clone, Debug)]
struct Candidate {
    path: String,
    score: usize,
    ambiguous: bool,
    related: bool,
    symbol: Option<RepositorySymbolObservation>,
    line: Option<usize>,
}

impl Candidate {
    fn from_symbol(hit: &SymbolMatch, relationship: Option<&str>) -> Self {
        Self {
            path: hit.path.to_string_lossy().into_owned(),
            score: hit.score,
            ambiguous: false,
            related: relationship.is_some(),
            line: Some(hit.symbol.start_line),
            symbol: Some(symbol_observation(&hit.path, Some(&hit.symbol), relationship, hit.score)),
        }
    }

    fn from_relationship(hit: &SymbolRelationshipMatch) -> Self {
        let relationship = relationship_name(hit.kind);
        Self {
            path: hit.path.to_string_lossy().into_owned(),
            score: hit.score,
            ambiguous: hit.ambiguous,
            related: !matches!(hit.kind, SymbolRelationshipKind::Definition),
            line: hit.symbol.as_ref().map(|symbol| symbol.start_line),
            symbol: Some(symbol_observation(
                &hit.path,
                hit.symbol.as_ref(),
                Some(relationship),
                hit.score,
            )),
        }
    }

    fn from_selection(selection: &RepoSelection) -> Self {
        let path = selection.path.to_string_lossy().into_owned();
        Self {
            path: path.clone(),
            score: selection.score,
            ambiguous: false,
            related: matches!(selection.kind, RepoSelectionKind::Test),
            line: selection.symbol.as_ref().map(|symbol| symbol.line),
            symbol: selection
                .symbol
                .as_ref()
                .map(|symbol| selection_symbol_observation(&path, symbol, selection.score)),
        }
    }

    fn from_decision(candidate: &aether_core::DecisionCandidate, detail: Option<&str>) -> Self {
        let (name, line) = detail.and_then(parse_symbol_detail).unzip();
        Self {
            path: candidate.path.as_str().to_owned(),
            score: usize::from(candidate.score),
            ambiguous: false,
            related: is_test_path(candidate.path.as_str()),
            line,
            symbol: name.map(|name| RepositorySymbolObservation {
                path: candidate.path.as_str().to_owned(),
                name: Some(name),
                kind: "indexed".to_owned(),
                relationship: None,
                line,
                end_line: None,
                score: candidate.score,
            }),
        }
    }
}

fn empty_plan(depth: usize) -> RepositoryActionPlan {
    RepositoryActionPlan {
        actions: Vec::new(),
        observation: RepositoryObservation::default(),
        logical_depth: depth,
        aborted: true,
        abort_reason: None,
    }
}

fn abort_plan(mut plan: RepositoryActionPlan, reason: &str) -> RepositoryActionPlan {
    plan.abort_reason = Some(BoundedText::new(reason, aether_core::MAX_DECISION_FIELD_BYTES));
    plan.aborted = true;
    plan
}

fn abort_plan_with_confidence(
    mut plan: RepositoryActionPlan,
    confidence: u8,
    reason: &str,
) -> RepositoryActionPlan {
    plan.observation.confidence = confidence;
    abort_plan(plan, reason)
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|term| term.len() > 1)
        .take(16)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn candidate_matches_terms(candidate: &Candidate, terms: &[String]) -> bool {
    let path = candidate.path.to_ascii_lowercase();
    let symbol = candidate
        .symbol
        .as_ref()
        .and_then(|symbol| symbol.name.as_deref())
        .map(str::to_ascii_lowercase);
    terms.iter().any(|term| {
        path.contains(term) || symbol.as_deref().is_some_and(|name| name.contains(term))
    })
}

fn confidence(top: usize, second: Option<usize>) -> u8 {
    let base: u8 = match top {
        120.. => 94,
        100.. => 90,
        84.. => 82,
        68.. => 68,
        _ => 40,
    };
    let gap = second.map_or(10, |score| top.saturating_sub(score).min(10) as u8);
    base.saturating_add(gap).min(100)
}

fn relationship_name(kind: SymbolRelationshipKind) -> &'static str {
    match kind {
        SymbolRelationshipKind::Definition => "definition",
        SymbolRelationshipKind::Caller => "caller",
        SymbolRelationshipKind::Implementation => "implementation",
        SymbolRelationshipKind::Test => "test",
        SymbolRelationshipKind::Dependency => "dependency",
        SymbolRelationshipKind::Import => "import",
        SymbolRelationshipKind::Module => "module",
    }
}

fn symbol_kind_name(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Function => "function",
        SymbolKind::Method => "method",
        SymbolKind::Struct => "struct",
        SymbolKind::Enum => "enum",
        SymbolKind::Trait => "trait",
        SymbolKind::TypeAlias => "type_alias",
        SymbolKind::Module => "module",
        SymbolKind::Test => "test",
        SymbolKind::Import => "import",
    }
}

fn symbol_observation(
    path: &Path,
    symbol: Option<&Symbol>,
    relationship: Option<&str>,
    score: usize,
) -> RepositorySymbolObservation {
    RepositorySymbolObservation {
        path: path.to_string_lossy().into_owned(),
        name: symbol.map(|symbol| symbol.name.clone()),
        kind: symbol.map_or_else(
            || "relationship".to_owned(),
            |symbol| symbol_kind_name(symbol.kind).to_owned(),
        ),
        relationship: relationship.map(str::to_owned),
        line: symbol.map(|symbol| symbol.start_line),
        end_line: symbol.map(|symbol| symbol.end_line),
        score: score.min(u16::MAX as usize) as u16,
    }
}

fn selection_symbol_observation(
    path: &str,
    symbol: &RepoSymbolSelection,
    score: usize,
) -> RepositorySymbolObservation {
    RepositorySymbolObservation {
        path: path.to_owned(),
        name: Some(symbol.name.clone()),
        kind: symbol_kind_name(symbol.kind).to_owned(),
        relationship: None,
        line: Some(symbol.line),
        end_line: None,
        score: score.min(u16::MAX as usize) as u16,
    }
}

fn parse_symbol_detail(detail: &str) -> Option<(String, usize)> {
    let (name, line) = detail.rsplit_once(" at line ")?;
    Some((name.to_owned(), line.parse().ok()?))
}

fn current_context_paths(context: &ContextSnapshot) -> Vec<InspectedFile> {
    context
        .inspected
        .iter()
        .filter(|file| file.last_state == ObservedFileState::Present)
        .cloned()
        .collect()
}

fn is_current_file(file: &InspectedFile) -> bool {
    file.last_state == ObservedFileState::Present && !file.stale
}

fn candidate_ranges(candidate: &Candidate, inspected: Option<&InspectedFile>) -> Vec<LineRange> {
    if let Some(line) = candidate.line {
        return vec![LineRange { start: line.max(1), end: line.max(1).saturating_add(80) }];
    }
    if let Some(file) = inspected
        && let Some(range) = file.ranges.first().copied()
    {
        return vec![range];
    }
    vec![LineRange { start: 1, end: 96 }]
}

fn normalize_ranges(ranges: &[LineRange]) -> Vec<LineRange> {
    let mut normalized = ranges
        .iter()
        .take(16)
        .filter_map(|range| {
            (range.start > 0 && range.end >= range.start).then_some(LineRange {
                start: range.start,
                end: range.end.min(range.start.saturating_add(256)),
            })
        })
        .collect::<Vec<_>>();
    if normalized.is_empty() {
        normalized.push(LineRange { start: 1, end: usize::MAX });
    }
    normalized
}

fn ranges_covered(file: &InspectedFile, ranges: &[LineRange]) -> bool {
    is_current_file(file)
        && ranges.iter().all(|needed| {
            file.ranges
                .iter()
                .any(|observed| observed.start <= needed.start && observed.end >= needed.end)
        })
}

fn current_excerpt(
    context: &ContextSnapshot,
    path: &str,
    ranges: &[LineRange],
) -> Option<RepositoryExcerpt> {
    let file = context.inspected.iter().find(|file| file.path == path && is_current_file(file))?;
    let excerpt = context.excerpts.iter().rev().find(|excerpt| {
        excerpt.path == path
            && !file.stale
            && ranges
                .iter()
                .all(|needed| excerpt.start_line <= needed.start && excerpt.end_line >= needed.end)
    })?;
    Some(RepositoryExcerpt {
        path: excerpt.path.clone(),
        start_line: excerpt.start_line,
        end_line: excerpt.end_line,
        text: excerpt.text.clone(),
        content_hash: excerpt.content_hash.clone(),
        truncated: false,
    })
}

fn push_excerpt(observation: &mut RepositoryObservation, excerpt: RepositoryExcerpt, max: usize) {
    if observation.excerpts.iter().any(|existing| {
        existing.path == excerpt.path
            && existing.start_line == excerpt.start_line
            && existing.end_line == excerpt.end_line
    }) {
        return;
    }
    let retained_bytes =
        observation.excerpts.iter().map(|existing| existing.text.as_str().len()).sum::<usize>();
    let remaining = MAX_PLANNER_OBSERVATION_BYTES.saturating_sub(retained_bytes);
    if remaining == 0 {
        return;
    }
    let original_bytes = excerpt.text.as_str().len();
    let mut excerpt = excerpt;
    excerpt.text = BoundedText::new(excerpt.text.as_str(), remaining);
    excerpt.truncated |= excerpt.text.as_str().len() < original_bytes;
    if excerpt.text.as_str().is_empty() {
        return;
    }
    observation.excerpts.push(excerpt);
    observation.excerpts.truncate(max);
    observation.cache_hit = true;
}

fn same_stem(left: &str, right: &str) -> bool {
    path_stem(left).eq_ignore_ascii_case(path_stem(right))
}

fn path_stem(path: &str) -> &str {
    path.rsplit('/')
        .next()
        .unwrap_or(path)
        .split('.')
        .next()
        .unwrap_or(path)
        .trim_end_matches("_test")
}

fn is_test_path(path: &str) -> bool {
    path.split('/').any(|part| part == "tests" || part == "test")
        || path.rsplit('/').next().is_some_and(|name| name.contains("_test"))
}

fn check_budget(
    started: Instant,
    budget: Duration,
    cancellation: &crate::CancellationToken,
) -> Result<(), PlannerExecutionError> {
    if cancellation.is_cancelled() {
        return Err(PlannerExecutionError::Cancelled);
    }
    if started.elapsed() > budget {
        return Err(PlannerExecutionError::TimeBudget);
    }
    Ok(())
}

fn read_excerpt(
    root: &Path,
    path: &str,
    ranges: &[LineRange],
    max_bytes: usize,
    started: Instant,
    budget: Duration,
    cancellation: &crate::CancellationToken,
) -> Result<RepositoryExcerpt, PlannerExecutionError> {
    let relative = WorkspacePath::new(path)
        .map_err(|_| PlannerExecutionError::InvalidPath(path.to_owned()))?;
    let full = root.join(relative.as_path());
    let canonical_root = fs::canonicalize(root)?;
    let canonical = fs::canonicalize(&full)?;
    if !canonical.starts_with(&canonical_root) {
        return Err(PlannerExecutionError::InvalidPath(path.to_owned()));
    }
    let metadata = fs::symlink_metadata(&full)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(PlannerExecutionError::InvalidPath(path.to_owned()));
    }
    check_budget(started, budget, cancellation)?;
    let mut file = File::open(canonical)?;
    let read_limit = max_bytes.min(MAX_PLANNER_OBSERVATION_BYTES);
    let mut bytes = Vec::with_capacity(read_limit.min(8 * 1024));
    let mut limited = (&mut file).take(read_limit as u64 + 1);
    limited.read_to_end(&mut bytes)?;
    let truncated = bytes.len() > read_limit;
    bytes.truncate(read_limit);
    check_budget(started, budget, cancellation)?;
    let text = String::from_utf8_lossy(&bytes);
    let first = ranges.first().map_or(1, |range| range.start.max(1));
    let last = ranges.last().map_or(96, |range| range.end.max(first));
    let selected = text
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let number = index + 1;
            ranges.iter().any(|range| number >= range.start && number <= range.end).then_some(line)
        })
        .collect::<Vec<_>>()
        .join("\n");
    let bounded = BoundedText::new(selected, max_bytes.min(MAX_EXCERPT_BYTES));
    let end_line = if bounded.as_str().is_empty() {
        first
    } else {
        first.saturating_add(bounded.as_str().lines().count().saturating_sub(1))
    };
    Ok(RepositoryExcerpt {
        path: relative.display(),
        start_line: first,
        end_line: end_line.min(last),
        content_hash: Some(blake3::hash(&bytes).to_hex().to_string()),
        text: bounded,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use aether_core::{
        BoundedText, ContextSnapshot, DecisionEvidenceKind, FileExcerpt, InspectedFile,
    };

    fn context() -> ContextSnapshot {
        let mut context = ContextSnapshot::new("/workspace", None);
        context.current_task = BoundedText::new("fix parse_port", 4096);
        context.workflow.decision.upsert_candidate("src/lib.rs", 120, 2, false, false, false);
        context.workflow.decision.record_evidence(
            DecisionEvidenceKind::Symbol,
            "src/lib.rs",
            "parse_port at line 1",
            96,
            0,
        );
        context
    }

    #[test]
    fn exact_symbol_plan_collapses_to_one_read_only_action() {
        let context = context();
        let planner = RepositoryActionPlanner::default();
        let plan = planner.plan(RepositoryPlanRequest::new(
            RepositoryRequestKind::Search,
            "parse_port",
            &context,
        ));
        assert!(plan.is_usable());
        assert_eq!(plan.actions.len(), 1);
        assert!(plan.observation.collapsed);
        assert!(plan.actions.iter().all(|action| matches!(action, RepositoryAction::Read { .. })));
    }

    #[test]
    fn ambiguous_relationships_abort_before_actions() {
        let context = ContextSnapshot::new("/workspace", None);
        let relationships = [SymbolRelationshipMatch {
            path: PathBuf::from("src/a.rs"),
            symbol: None,
            kind: SymbolRelationshipKind::Definition,
            score: 120,
            ambiguous: true,
        }];
        let plan = RepositoryActionPlanner::default().plan(
            RepositoryPlanRequest::new(RepositoryRequestKind::Search, "parse", &context)
                .with_relationships(&relationships),
        );
        assert!(!plan.is_usable());
        assert!(plan.is_ambiguous());
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn current_excerpt_needs_no_new_read() {
        let mut context = context();
        context.inspected.push(InspectedFile {
            path: "src/lib.rs".to_owned(),
            content_hash: Some("hash".to_owned()),
            ranges: vec![LineRange { start: 1, end: 81 }],
            last_state: ObservedFileState::Present,
            stale: false,
        });
        context.excerpts.push(FileExcerpt {
            path: "src/lib.rs".to_owned(),
            start_line: 1,
            end_line: 81,
            text: BoundedText::new("fn parse_port() {}", 4096),
            content_hash: Some("hash".to_owned()),
        });
        let plan = RepositoryActionPlanner::default().plan(RepositoryPlanRequest::new(
            RepositoryRequestKind::Search,
            "parse_port",
            &context,
        ));
        assert!(plan.is_usable());
        assert!(plan.actions.is_empty());
        assert!(plan.observation.cache_hit);
        assert_eq!(plan.observation.excerpts.len(), 1);
    }

    #[test]
    fn exact_read_targets_coalesce_without_duplicate_physical_reads() {
        let context = ContextSnapshot::new("/workspace", None);
        let planner = RepositoryActionPlanner::default();
        let targets = [
            RepositoryReadTarget {
                path: "src/lib.rs".to_owned(),
                ranges: vec![LineRange { start: 1, end: 8 }],
            },
            RepositoryReadTarget {
                path: "src/main.rs".to_owned(),
                ranges: vec![LineRange { start: 1, end: 4 }],
            },
        ];
        let first = planner.plan_read_targets(&context, &targets);
        let second = planner.plan_read_targets(&context, &targets[..1]);
        let merged = planner.coalesce(&[first, second]).expect("compatible reads coalesce");
        assert!(merged.is_usable());
        assert_eq!(merged.actions.len(), 2);
        assert_eq!(merged.observation.files.len(), 2);
        assert!(merged.observation.collapsed);
    }

    #[test]
    fn exact_read_planning_fails_closed_after_workspace_drift() {
        let mut context = ContextSnapshot::new("/workspace", None);
        context.workspace_changed = true;
        let plan = RepositoryActionPlanner::default().plan_read_targets(
            &context,
            &[RepositoryReadTarget { path: "src/lib.rs".to_owned(), ranges: Vec::new() }],
        );
        assert!(!plan.is_usable());
        assert!(plan.aborted);
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn unrelated_indexed_evidence_aborts_before_read() {
        let context = context();
        let plan = RepositoryActionPlanner::default().plan(RepositoryPlanRequest::new(
            RepositoryRequestKind::Search,
            "unrelated_symbol",
            &context,
        ));
        assert!(plan.is_ambiguous());
        assert!(plan.actions.is_empty());
    }

    #[test]
    fn limits_are_normalized_and_mutation_has_no_plan_variant() {
        let planner = RepositoryActionPlanner::new(PlannerLimits {
            max_depth: usize::MAX,
            max_candidates: usize::MAX,
            max_files: usize::MAX,
            max_bytes: usize::MAX,
            max_actions: usize::MAX,
            max_execution_micros: u64::MAX,
        });
        assert_eq!(planner.limits().max_depth, DEFAULT_MAX_PLANNER_DEPTH);
        assert_eq!(planner.limits().max_files, DEFAULT_MAX_PLANNER_FILES);
    }
}
