//! Bounded operational memory for one repository task.
//!
//! WorkingSet is a projection over persisted context, not another planner. It keeps identifiers,
//! path roles, freshness, and compact blockers close to the active decision while leaving full
//! source excerpts in ContextSnapshot and hydrating them only when selected evidence requires it.

use std::fmt::Write as _;

use aether_core::{
    BoundedText, ContextSnapshot, DecisionEvidenceKind, MAX_DECISION_FIELD_BYTES, TurnPhase,
};

/// Maximum path-role entries retained by one active working set.
pub const MAX_WORKING_SET_ITEMS: usize = 24;
/// Maximum compact blockers retained in the model-facing projection.
pub const MAX_WORKING_SET_BLOCKERS: usize = 8;
/// Maximum bytes rendered for a working-set projection.
pub const MAX_WORKING_SET_RENDER_BYTES: usize = 4 * 1024;

/// Coarse role of a path in the active repository task.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum WorkingSetRole {
    Target,
    Changed,
    UserOwned,
    Diagnostic,
    Definition,
    Relationship,
    Test,
    Evidence,
}

impl WorkingSetRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Target => "target",
            Self::Changed => "changed",
            Self::UserOwned => "user_owned",
            Self::Diagnostic => "diagnostic",
            Self::Definition => "definition",
            Self::Relationship => "relationship",
            Self::Test => "test",
            Self::Evidence => "evidence",
        }
    }
}

/// One bounded path reference retained by the active working set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkingSetItem {
    pub path: String,
    pub role: WorkingSetRole,
    pub priority: u16,
    pub fresh: bool,
}

/// Compact operational memory derived from the current ContextSnapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WorkingSet {
    pub objective: String,
    pub active_subgoal: Option<String>,
    pub pending_criteria: Vec<String>,
    pub items: Vec<WorkingSetItem>,
    pub blockers: Vec<String>,
    pub stale_paths: Vec<String>,
    pub revision: u32,
    pub phase: TurnPhase,
}

impl WorkingSet {
    /// Build a stable, high-signal projection without reading the workspace.
    #[must_use]
    pub fn from_snapshot(snapshot: &ContextSnapshot) -> Self {
        let workflow = &snapshot.workflow;
        let mut working_set = Self {
            objective: workflow.work.objective.as_str().to_owned(),
            active_subgoal: workflow.work.active_subgoal.as_ref().map(|id| id.as_str().to_owned()),
            pending_criteria: workflow
                .work
                .acceptance_criteria
                .iter()
                .filter(|criterion| !criterion.satisfied)
                .take(8)
                .map(|criterion| criterion.key.as_str().to_owned())
                .collect(),
            blockers: workflow
                .unresolved_failures
                .iter()
                .take(MAX_WORKING_SET_BLOCKERS)
                .map(|failure| failure.key.as_str().to_owned())
                .collect(),
            stale_paths: snapshot
                .inspected
                .iter()
                .filter(|file| file.stale)
                .take(MAX_WORKING_SET_ITEMS)
                .map(|file| file.path.clone())
                .collect(),
            revision: workflow.workspace_revision,
            phase: workflow.director.phase,
            items: Vec::new(),
        };

        for candidate in workflow.decision.candidate_files.iter().take(16) {
            working_set.add_item(
                candidate.path.as_str(),
                if candidate.evidence > 0 {
                    WorkingSetRole::Target
                } else {
                    WorkingSetRole::Evidence
                },
                candidate.score,
                candidate.inspected && !candidate.stale,
            );
        }
        for path in &snapshot.modified {
            working_set.add_item(path, WorkingSetRole::Changed, 100, true);
        }
        for path in &snapshot.user_modified {
            working_set.add_item(path, WorkingSetRole::UserOwned, 96, true);
        }
        for mutation in workflow.work.mutations.iter().rev().take(8) {
            for path in &mutation.paths {
                working_set.add_item(
                    path.as_str(),
                    WorkingSetRole::Changed,
                    92,
                    mutation.revision == workflow.workspace_revision,
                );
            }
        }
        for evidence in workflow.decision.evidence.iter().rev().take(24) {
            let role = match evidence.kind {
                DecisionEvidenceKind::Symbol => WorkingSetRole::Definition,
                DecisionEvidenceKind::Relationship => WorkingSetRole::Relationship,
                DecisionEvidenceKind::Verification => WorkingSetRole::Diagnostic,
                _ => WorkingSetRole::Evidence,
            };
            if !evidence.path.as_str().is_empty() {
                working_set.add_item(evidence.path.as_str(), role, evidence.weight, true);
            }
        }
        for evidence in workflow.work.evidence.iter().rev().take(16) {
            if !evidence.path.as_str().is_empty() {
                working_set.add_item(
                    evidence.path.as_str(),
                    WorkingSetRole::Evidence,
                    20,
                    evidence.valid,
                );
            }
        }
        for blocker in workflow.work.blockers.iter().take(MAX_WORKING_SET_BLOCKERS) {
            if working_set.blockers.len() == MAX_WORKING_SET_BLOCKERS {
                break;
            }
            let key = blocker.key.as_str().to_owned();
            if !working_set.blockers.iter().any(|existing| existing == &key) {
                working_set.blockers.push(key);
            }
        }
        working_set.items.sort_unstable_by(|left, right| {
            right
                .priority
                .cmp(&left.priority)
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.role.cmp(&right.role))
        });
        working_set.items.truncate(MAX_WORKING_SET_ITEMS);
        working_set
    }

    /// Render only bounded identifiers and freshness metadata for model context.
    #[must_use]
    pub fn render(&self) -> String {
        let mut output = String::new();
        let _ = write!(
            output,
            "phase={} rev={} objective={}",
            self.phase.as_str(),
            self.revision,
            bounded(&self.objective),
        );
        if let Some(active) = &self.active_subgoal {
            let _ = write!(output, " active={}", bounded(active));
        }
        if !self.pending_criteria.is_empty() {
            output.push_str(" pending_criteria=");
            output.push_str(
                &self
                    .pending_criteria
                    .iter()
                    .map(|item| bounded(item))
                    .collect::<Vec<_>>()
                    .join(","),
            );
        }
        if !self.blockers.is_empty() {
            output.push_str(" blockers=");
            output.push_str(
                &self.blockers.iter().map(|item| bounded(item)).collect::<Vec<_>>().join(","),
            );
        }
        if !self.stale_paths.is_empty() {
            output.push_str(" stale=");
            output.push_str(
                &self.stale_paths.iter().map(|item| bounded(item)).collect::<Vec<_>>().join(","),
            );
        }
        output.push_str(" items=");
        for (index, item) in self.items.iter().enumerate() {
            if index > 0 {
                output.push(';');
            }
            let _ = write!(
                output,
                "{}:{}:{}",
                bounded(&item.path),
                item.role.as_str(),
                if item.fresh { "fresh" } else { "stale" },
            );
        }
        BoundedText::new(output, MAX_WORKING_SET_RENDER_BYTES).into_string()
    }

    fn add_item(&mut self, path: &str, role: WorkingSetRole, priority: u16, fresh: bool) {
        let path = BoundedText::new(path, MAX_DECISION_FIELD_BYTES);
        if path.as_str().is_empty() || path.is_truncated() {
            return;
        }
        if let Some(existing) = self
            .items
            .iter_mut()
            .find(|existing| existing.path == path.as_str() && existing.role == role)
        {
            existing.priority = existing.priority.max(priority);
            existing.fresh &= fresh;
            return;
        }
        self.items.push(WorkingSetItem { path: path.into_string(), role, priority, fresh });
        if self.items.len() > MAX_WORKING_SET_ITEMS.saturating_mul(2) {
            self.items.sort_unstable_by_key(|item| std::cmp::Reverse(item.priority));
            self.items.truncate(MAX_WORKING_SET_ITEMS);
        }
    }
}

fn bounded(value: &str) -> String {
    BoundedText::new(value, MAX_DECISION_FIELD_BYTES).into_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::{DecisionEvidenceKind, EvidenceProvenance};

    #[test]
    fn multi_file_working_set_prioritizes_changes_and_relationships() {
        let mut snapshot = ContextSnapshot::new("/workspace", None);
        snapshot.workflow.work.objective = BoundedText::new("migrate API", 512);
        snapshot.modified = vec!["crates/app/src/lib.rs".to_owned()];
        snapshot.workflow.decision.record_evidence_with_provenance(
            EvidenceProvenance::Repository,
            DecisionEvidenceKind::Relationship,
            "crates/codec/src/lib.rs",
            "caller",
            80,
            0,
        );
        let working_set = WorkingSet::from_snapshot(&snapshot);
        assert!(working_set.items.iter().any(|item| item.role == WorkingSetRole::Changed));
        assert!(working_set.items.iter().any(|item| {
            item.path == "crates/codec/src/lib.rs" && item.role == WorkingSetRole::Relationship
        }));
        assert!(working_set.render().contains("crates/app/src/lib.rs"));
    }
}
