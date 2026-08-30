//! Deterministic, bounded selection of stored context for a model turn.

use std::cmp::Reverse;

use crate::symbol_index::SymbolRelationshipKind;
use aether_core::WorkflowPhase;

/// The origin of a context candidate.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ContextKind {
    ModifiedPath,
    UserModifiedPath,
    Excerpt,
    InspectedFile,
    ToolSummary,
    ScopedInstructions,
    RepositoryMetadata,
    GitMetadata,
}

/// A borrowed item from the larger stored context.
#[derive(Clone, Copy, Debug)]
pub struct ContextCandidate<'a> {
    pub kind: ContextKind,
    pub path: Option<&'a str>,
    pub content: &'a str,
    pub start_line: usize,
    pub end_line: usize,
    pub recency: usize,
    pub modified: bool,
    pub stale: bool,
    /// Workflow phase in which this candidate is being considered.
    pub phase: WorkflowPhase,
    /// Whether this item changed since the previous bounded context packet.
    pub delta: bool,
    /// Stable content fingerprint used for delta-aware selection.
    pub fingerprint: [u8; 16],
    /// Whether this item belongs to an active work subgoal or target scope.
    pub targeted_subgoal: bool,
    /// Whether this item corresponds to an unresolved diagnostic or failure path.
    pub diagnostic_path: bool,
}

/// A symbol hit supplied by a targeted repository lookup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SymbolHint<'a> {
    pub path: &'a str,
    pub name: &'a str,
}

/// A bounded relationship hit supplied by repository symbol navigation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RelationshipHint<'a> {
    pub path: &'a str,
    pub kind: SymbolRelationshipKind,
    pub score: usize,
    pub ambiguous: bool,
}

impl<'a> ContextCandidate<'a> {
    #[must_use]
    pub fn new(kind: ContextKind, path: Option<&'a str>, content: &'a str) -> Self {
        Self {
            kind,
            path,
            content,
            start_line: 0,
            end_line: 0,
            recency: 0,
            modified: false,
            stale: false,
            phase: WorkflowPhase::Discover,
            delta: true,
            fingerprint: [0; 16],
            targeted_subgoal: false,
            diagnostic_path: false,
        }
    }

    fn byte_len(self) -> usize {
        self.path.map_or(0, str::len).saturating_add(self.content.len()).saturating_add(32)
    }
}

/// One ranked item selected for model-visible context.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelectedContextItem {
    pub index: usize,
    pub score: i32,
    pub bytes: usize,
    pub truncated: bool,
}

/// Result of relevance selection, in deterministic rank order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedContext {
    pub items: Vec<SelectedContextItem>,
    pub used_bytes: usize,
}

/// Rank and select context without semantic indexes or external state.
#[must_use]
pub fn select_context(
    task: &str,
    candidates: &[ContextCandidate<'_>],
    byte_budget: usize,
    item_limit: usize,
) -> SelectedContext {
    select_context_with_symbols(task, candidates, byte_budget, item_limit, &[])
}

/// Rank context with bounded symbol hits from relevant source files.
#[must_use]
pub fn select_context_with_symbols(
    task: &str,
    candidates: &[ContextCandidate<'_>],
    byte_budget: usize,
    item_limit: usize,
    symbols: &[SymbolHint<'_>],
) -> SelectedContext {
    select_context_with_relationships(task, candidates, byte_budget, item_limit, symbols, &[])
}

/// Rank context with direct symbol hits and bounded lexical relationship hits.
#[must_use]
pub fn select_context_with_relationships(
    task: &str,
    candidates: &[ContextCandidate<'_>],
    byte_budget: usize,
    item_limit: usize,
    symbols: &[SymbolHint<'_>],
    relationships: &[RelationshipHint<'_>],
) -> SelectedContext {
    if byte_budget == 0 || item_limit == 0 {
        return SelectedContext { items: Vec::new(), used_bytes: 0 };
    }
    let terms: Vec<&str> = task
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|term| term.len() > 1)
        .take(32)
        .collect();
    let newest = candidates.iter().map(|candidate| candidate.recency).max().unwrap_or(0);
    let mut rank = |(index, candidate): (usize, &ContextCandidate<'_>)| {
        (
            Reverse(
                score_candidate(*candidate, &terms, newest)
                    + phase_bonus(*candidate)
                    + delta_bonus(*candidate)
                    + symbol_bonus(*candidate, &terms, symbols)
                    + relationship_hint_bonus(*candidate, relationships)
                    + relationship_bonus(*candidate, candidates),
            ),
            index,
        )
    };
    let mut ranked: Vec<(Reverse<i32>, usize)> =
        if candidates.iter().all(|candidate| candidate.delta) {
            candidates.iter().enumerate().map(&mut rank).collect()
        } else {
            candidates
                .iter()
                .enumerate()
                .filter(|(_, candidate)| candidate.delta)
                .map(&mut rank)
                .collect()
        };
    let ranked_limit = item_limit.saturating_mul(4).min(ranked.len());
    if ranked_limit < ranked.len() {
        ranked.select_nth_unstable_by_key(ranked_limit, |(score, index)| (*score, *index));
        ranked.truncate(ranked_limit);
    }
    ranked.sort_unstable_by_key(|(score, index)| (*score, *index));

    let mut items = Vec::with_capacity(item_limit.min(candidates.len()));
    let mut used_bytes = 0usize;
    for (Reverse(score), index) in ranked {
        if items.len() == item_limit || used_bytes == byte_budget {
            break;
        }
        let candidate = candidates[index];
        if is_duplicate(candidate, candidates, &items) {
            continue;
        }
        let available = byte_budget - used_bytes;
        let bytes = candidate.byte_len().min(available);
        if bytes == 0 {
            break;
        }
        let truncated = bytes < candidate.byte_len();
        items.push(SelectedContextItem { index, score, bytes, truncated });
        used_bytes += bytes;
    }
    SelectedContext { items, used_bytes }
}

fn relationship_hint_bonus(
    candidate: ContextCandidate<'_>,
    relationships: &[RelationshipHint<'_>],
) -> i32 {
    let Some(path) = candidate.path else { return 0 };
    relationships
        .iter()
        .take(128)
        .filter(|relationship| relationship.path == path)
        .map(|relationship| {
            let role_bonus = match relationship.kind {
                SymbolRelationshipKind::Definition => 56,
                SymbolRelationshipKind::Implementation => 50,
                SymbolRelationshipKind::Caller => 44,
                SymbolRelationshipKind::Test => 48,
                SymbolRelationshipKind::Dependency => 32,
                SymbolRelationshipKind::Import => 24,
                SymbolRelationshipKind::Module => 20,
            };
            let evidence_bonus = (relationship.score.min(128) / 8) as i32;
            let ambiguity_penalty = if relationship.ambiguous { 10 } else { 0 };
            role_bonus + evidence_bonus - ambiguity_penalty
        })
        .sum()
}

fn symbol_bonus(
    candidate: ContextCandidate<'_>,
    terms: &[&str],
    symbols: &[SymbolHint<'_>],
) -> i32 {
    let Some(path) = candidate.path else { return 0 };
    symbols
        .iter()
        .take(128)
        .filter(|symbol| symbol.path == path)
        .map(|symbol| {
            terms
                .iter()
                .filter(|term| {
                    symbol.name.eq_ignore_ascii_case(term)
                        || contains_ascii_case_insensitive(symbol.name, term)
                })
                .count() as i32
                * 40
        })
        .sum()
}

fn relationship_bonus(candidate: ContextCandidate<'_>, candidates: &[ContextCandidate<'_>]) -> i32 {
    let Some(path) = candidate.path else { return 0 };
    if !is_test_path(path) {
        return 0;
    }
    candidates.iter().any(|other| {
        other.modified
            && other.path.is_some_and(|other_path| {
                !is_test_path(other_path) && related_path_stem(path, other_path)
            })
    }) as i32
        * 45
}

fn is_test_path(path: &str) -> bool {
    if !path.contains("test") {
        return false;
    }
    path.split(['/', '\\']).any(|part| part == "tests" || part == "test")
        || path.rsplit('/').next().is_some_and(|name| name.contains("_test"))
}

fn related_path_stem(left: &str, right: &str) -> bool {
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

fn score_candidate(candidate: ContextCandidate<'_>, terms: &[&str], newest: usize) -> i32 {
    let mut score = match candidate.kind {
        ContextKind::ModifiedPath => 90,
        ContextKind::UserModifiedPath => 94,
        ContextKind::Excerpt => 55,
        ContextKind::InspectedFile => 35,
        ContextKind::ToolSummary => 25,
        ContextKind::ScopedInstructions => 52,
        ContextKind::RepositoryMetadata => 15,
        ContextKind::GitMetadata => 10,
    };
    if candidate.modified {
        score += 80;
    }
    if candidate.targeted_subgoal {
        score += 65;
    }
    if candidate.diagnostic_path {
        score += 75;
    }
    if candidate.stale {
        score -= 220;
    } else if matches!(candidate.kind, ContextKind::Excerpt | ContextKind::InspectedFile) {
        score += 25;
    }
    score += 24i32.saturating_sub(newest.saturating_sub(candidate.recency).min(24) as i32);
    for term in terms {
        let path_matches = candidate.path.is_some_and(|path| path.contains(term));
        if path_matches {
            score += 40;
            if candidate.path.is_some_and(|path| symbol_matches(path, term)) {
                score += 25;
            }
        }
        if !path_matches && candidate.content.contains(term) {
            score += 10;
        }
    }
    score
}

fn delta_bonus(candidate: ContextCandidate<'_>) -> i32 {
    if candidate.delta { 48 } else { -320 }
}

fn phase_bonus(candidate: ContextCandidate<'_>) -> i32 {
    let mut bonus = match candidate.phase {
        WorkflowPhase::Discover => match candidate.kind {
            ContextKind::RepositoryMetadata
            | ContextKind::ScopedInstructions
            | ContextKind::GitMetadata
            | ContextKind::ModifiedPath
            | ContextKind::UserModifiedPath
            | ContextKind::ToolSummary => 32,
            ContextKind::Excerpt | ContextKind::InspectedFile => 8,
        },
        WorkflowPhase::Inspect => match candidate.kind {
            ContextKind::Excerpt | ContextKind::InspectedFile => 42,
            ContextKind::RepositoryMetadata
            | ContextKind::ScopedInstructions
            | ContextKind::ToolSummary => 16,
            ContextKind::ModifiedPath
            | ContextKind::UserModifiedPath
            | ContextKind::GitMetadata => 8,
        },
        WorkflowPhase::Modify => match candidate.kind {
            ContextKind::ModifiedPath | ContextKind::UserModifiedPath => 48,
            ContextKind::Excerpt | ContextKind::InspectedFile => 32,
            ContextKind::ToolSummary => 20,
            ContextKind::RepositoryMetadata
            | ContextKind::ScopedInstructions
            | ContextKind::GitMetadata => 4,
        },
        WorkflowPhase::Verify => match candidate.kind {
            ContextKind::ModifiedPath | ContextKind::Excerpt | ContextKind::InspectedFile => 38,
            ContextKind::ToolSummary | ContextKind::GitMetadata => 34,
            ContextKind::UserModifiedPath => 28,
            ContextKind::RepositoryMetadata | ContextKind::ScopedInstructions => 4,
        },
        WorkflowPhase::Complete => match candidate.kind {
            ContextKind::ToolSummary | ContextKind::GitMetadata => 30,
            ContextKind::ModifiedPath | ContextKind::Excerpt | ContextKind::InspectedFile => 24,
            ContextKind::UserModifiedPath => 20,
            ContextKind::RepositoryMetadata | ContextKind::ScopedInstructions => 4,
        },
    };
    if candidate.diagnostic_path && candidate.phase == WorkflowPhase::Inspect {
        bonus += 30;
    }
    if candidate.targeted_subgoal && candidate.phase == WorkflowPhase::Modify {
        bonus += 25;
    }
    bonus
}

fn symbol_matches(path: &str, term: &str) -> bool {
    path.split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| part.eq_ignore_ascii_case(term))
}

fn contains_ascii_case_insensitive(value: &str, needle: &str) -> bool {
    needle.len() <= value.len()
        && value
            .as_bytes()
            .windows(needle.len())
            .any(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn is_duplicate(
    candidate: ContextCandidate<'_>,
    candidates: &[ContextCandidate<'_>],
    selected: &[SelectedContextItem],
) -> bool {
    selected.iter().any(|item| {
        let existing = candidates[item.index];
        if candidate.path != existing.path {
            return false;
        }
        if candidate.kind == ContextKind::Excerpt && existing.kind == ContextKind::Excerpt {
            return candidate.start_line <= existing.end_line
                && existing.start_line <= candidate.end_line;
        }
        candidate.kind == existing.kind && candidate.content == existing.content
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate<'a>(kind: ContextKind, path: &'a str, content: &'a str) -> ContextCandidate<'a> {
        ContextCandidate::new(kind, Some(path), content)
    }

    #[test]
    fn lexical_path_and_symbol_matches_rank_relevant_source_first() {
        let candidates = [
            candidate(ContextKind::Excerpt, "src/cache.rs", "unrelated"),
            candidate(ContextKind::Excerpt, "src/context_selector.rs", "fn select_context"),
        ];
        let selected = select_context("fix context selector", &candidates, 4096, 2);
        assert_eq!(selected.items[0].index, 1);
    }

    #[test]
    fn targeted_symbol_hits_promote_the_relevant_inspected_file() {
        let candidates = [
            candidate(ContextKind::InspectedFile, "src/unrelated.rs", "unrelated"),
            candidate(ContextKind::InspectedFile, "src/target.rs", "source"),
        ];
        let symbols = [SymbolHint { path: "src/target.rs", name: "open_session" }];
        let selected = select_context_with_symbols("open_session", &candidates, 4096, 1, &symbols);
        assert_eq!(selected.items[0].index, 1);
    }

    #[test]
    fn relationship_hits_promote_tests_and_callers_without_resolving_ambiguity() {
        let candidates = [
            candidate(ContextKind::InspectedFile, "src/unrelated.rs", "unrelated"),
            candidate(ContextKind::InspectedFile, "src/caller.rs", "caller"),
            candidate(ContextKind::InspectedFile, "tests/target_test.rs", "test"),
        ];
        let relationships = [
            RelationshipHint {
                path: "src/caller.rs",
                kind: SymbolRelationshipKind::Caller,
                score: 92,
                ambiguous: true,
            },
            RelationshipHint {
                path: "tests/target_test.rs",
                kind: SymbolRelationshipKind::Test,
                score: 84,
                ambiguous: false,
            },
        ];
        let selected =
            select_context_with_relationships("target", &candidates, 4096, 2, &[], &relationships);
        assert_eq!(selected.items[0].index, 2);
        assert_eq!(selected.items[1].index, 1);
    }

    #[test]
    fn modified_source_promotes_paired_test_by_shared_symbol() {
        let source = {
            let mut value = candidate(ContextKind::ModifiedPath, "src/context.rs", "");
            value.modified = true;
            value
        };
        let paired = candidate(ContextKind::Excerpt, "tests/context_test.rs", "assert behavior");
        let unrelated = candidate(ContextKind::Excerpt, "tests/output_test.rs", "context context");
        let selected = select_context("behavior", &[source, unrelated, paired], 4096, 3);
        assert!(
            selected.items.iter().position(|item| item.index == 2).unwrap()
                < selected.items.iter().position(|item| item.index == 1).unwrap()
        );
    }

    #[test]
    fn stale_content_loses_to_current_content() {
        let mut stale = candidate(ContextKind::Excerpt, "src/context.rs", "selector selector");
        stale.stale = true;
        let current = candidate(ContextKind::InspectedFile, "src/context.rs", "current");
        let selected = select_context("selector", &[stale, current], 4096, 2);
        assert_eq!(selected.items[0].index, 1);
    }

    #[test]
    fn overlapping_excerpts_and_repeated_observations_are_deduplicated() {
        let mut first = candidate(ContextKind::Excerpt, "src/lib.rs", "one");
        first.start_line = 1;
        first.end_line = 20;
        let mut overlap = candidate(ContextKind::Excerpt, "src/lib.rs", "two");
        overlap.start_line = 10;
        overlap.end_line = 30;
        let repeated = candidate(ContextKind::ToolSummary, "", "read ok");
        let selected = select_context("lib", &[first, overlap, repeated, repeated], 4096, 8);
        assert_eq!(selected.items.len(), 2);
    }

    #[test]
    fn byte_and_item_budgets_truncate_selection() {
        let content = "x".repeat(256);
        let candidates = [candidate(ContextKind::Excerpt, "src/lib.rs", &content)];
        let selected = select_context("lib", &candidates, 64, 1);
        assert_eq!(selected.used_bytes, 64);
        assert!(selected.items[0].truncated);
    }

    #[test]
    fn equal_scores_have_deterministic_input_order() {
        let candidates = [
            candidate(ContextKind::ToolSummary, "", "same"),
            candidate(ContextKind::ToolSummary, "", "different"),
        ];
        let first = select_context("none", &candidates, 4096, 2);
        let second = select_context("none", &candidates, 4096, 2);
        assert_eq!(first, second);
        assert_eq!(first.items.iter().map(|item| item.index).collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn workflow_phase_and_delta_state_change_selection() {
        let mut modified = candidate(ContextKind::ModifiedPath, "src/lib.rs", "");
        modified.modified = true;
        modified.phase = WorkflowPhase::Verify;
        let mut metadata = candidate(ContextKind::RepositoryMetadata, "Cargo.toml", "package");
        metadata.phase = WorkflowPhase::Verify;
        let selected = select_context("verify", &[metadata, modified], 4096, 1);
        assert_eq!(selected.items[0].index, 1);

        metadata.delta = false;
        modified.delta = false;
        let empty = select_context("verify", &[metadata, modified], 4096, 1);
        assert!(empty.items.is_empty());
    }

    #[test]
    fn delta_selection_keeps_new_low_ranked_evidence_visible() {
        let mut unchanged = [
            candidate(ContextKind::ModifiedPath, "src/a.rs", ""),
            candidate(ContextKind::ModifiedPath, "src/b.rs", ""),
            candidate(ContextKind::ModifiedPath, "src/c.rs", ""),
            candidate(ContextKind::ModifiedPath, "src/d.rs", ""),
        ];
        for item in &mut unchanged {
            item.delta = false;
        }
        let mut new_metadata = candidate(ContextKind::RepositoryMetadata, "Cargo.toml", "new");
        new_metadata.delta = true;
        let selected = select_context(
            "unrelated",
            &[unchanged[0], unchanged[1], unchanged[2], unchanged[3], new_metadata],
            4096,
            1,
        );
        assert_eq!(selected.items[0].index, 4);
    }
}
