//! Bounded, serializable state shared by deterministic coding-policy decisions.

use serde::{Deserialize, Serialize};

use crate::{BoundedText, EvidenceProvenance};

/// Maximum retained evidence records for one coding task.
pub const MAX_DECISION_EVIDENCE: usize = 32;
/// Maximum retained unresolved questions for one coding task.
pub const MAX_DECISION_QUESTIONS: usize = 8;
/// Maximum candidate files retained for deterministic ranking.
pub const MAX_DECISION_CANDIDATES: usize = 16;
/// Maximum modified paths retained as the minimal affected scope.
pub const MAX_DECISION_SCOPE: usize = 16;
/// Maximum bytes retained by one decision-state text field.
pub const MAX_DECISION_FIELD_BYTES: usize = 192;

/// The local evidence classes used by the policy. These are intentionally lexical and
/// observational; they do not claim semantic understanding of the repository.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionEvidenceKind {
    #[default]
    Discovery,
    Inspection,
    Symbol,
    Relationship,
    Mutation,
    Verification,
}

impl DecisionEvidenceKind {
    /// Stable compact spelling for model-visible state.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Discovery => "discovery",
            Self::Inspection => "inspection",
            Self::Symbol => "symbol",
            Self::Relationship => "relationship",
            Self::Mutation => "mutation",
            Self::Verification => "verification",
        }
    }
}

/// The bounded actions that the runtime can recommend to the model.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionAction {
    #[default]
    DiscoverTarget,
    InspectCandidate,
    ResolveQuestion,
    MutateMinimal,
    VerifyFocused,
    VerifyBroader,
    Finish,
    Escalate,
}

impl DecisionAction {
    /// Stable compact spelling for model-visible state and deterministic tie-breaking.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DiscoverTarget => "discover_target",
            Self::InspectCandidate => "inspect_candidate",
            Self::ResolveQuestion => "resolve_question",
            Self::MutateMinimal => "mutate_minimal",
            Self::VerifyFocused => "verify_focused",
            Self::VerifyBroader => "verify_broader",
            Self::Finish => "finish",
            Self::Escalate => "escalate",
        }
    }
}

/// One compact piece of evidence supporting the current decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionEvidence {
    pub kind: DecisionEvidenceKind,
    /// Origin of the evidence. This is never treated as user authorization by itself.
    #[serde(default = "default_evidence_provenance")]
    pub provenance: EvidenceProvenance,
    pub path: BoundedText,
    pub detail: BoundedText,
    pub weight: u16,
    pub revision: u32,
}

fn default_evidence_provenance() -> EvidenceProvenance {
    EvidenceProvenance::Repository
}

/// One unresolved, actionable question that prevents a stronger decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionQuestion {
    pub key: BoundedText,
    pub question: BoundedText,
}

/// One candidate file with deterministic evidence and scope ranking.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionCandidate {
    pub path: BoundedText,
    pub score: u16,
    pub evidence: u16,
    pub inspected: bool,
    pub modified: bool,
    pub stale: bool,
}

/// Compact state used to select the next local coding action.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecisionState {
    pub evidence: Vec<DecisionEvidence>,
    pub unresolved_questions: Vec<DecisionQuestion>,
    pub candidate_files: Vec<DecisionCandidate>,
    pub modified_scope: Vec<String>,
    pub verification_scope: BoundedText,
    /// Deterministic 0..=100 confidence in the evidence needed for the next mutation.
    pub progress_confidence: u8,
    pub next_action: DecisionAction,
    pub next_target: Option<BoundedText>,
    pub next_reason: BoundedText,
    #[serde(default)]
    pub low_value_observations: u8,
}

impl DecisionState {
    /// Return a fresh decision state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or strengthen one bounded evidence record.
    pub fn record_evidence(
        &mut self,
        kind: DecisionEvidenceKind,
        path: impl AsRef<str>,
        detail: impl AsRef<str>,
        weight: u16,
        revision: u32,
    ) {
        self.record_evidence_with_provenance(
            EvidenceProvenance::Repository,
            kind,
            path,
            detail,
            weight,
            revision,
        );
    }

    /// Add evidence with an explicit origin. Provenance affects diagnostics and trust decisions;
    /// it never grants user authorization without a separate permission decision.
    pub fn record_evidence_with_provenance(
        &mut self,
        provenance: EvidenceProvenance,
        kind: DecisionEvidenceKind,
        path: impl AsRef<str>,
        detail: impl AsRef<str>,
        weight: u16,
        revision: u32,
    ) {
        let path = BoundedText::new(path, MAX_DECISION_FIELD_BYTES);
        let detail = BoundedText::new(detail, MAX_DECISION_FIELD_BYTES);
        if path.as_str().is_empty() && detail.as_str().is_empty() {
            return;
        }
        if let Some(existing) = self.evidence.iter_mut().find(|existing| {
            existing.kind == kind
                && existing.provenance == provenance
                && existing.path == path
                && existing.detail == detail
                && existing.revision == revision
        }) {
            existing.weight = existing.weight.max(weight);
            return;
        }
        if self.evidence.len() == MAX_DECISION_EVIDENCE {
            self.evidence.remove(0);
        }
        self.evidence.push(DecisionEvidence { kind, provenance, path, detail, weight, revision });
        self.progress_confidence =
            self.progress_confidence.saturating_add((weight.min(10)) as u8).min(100);
    }

    /// Add a question unless an equivalent key is already unresolved.
    pub fn record_question(&mut self, key: impl AsRef<str>, question: impl AsRef<str>) {
        let key = BoundedText::new(key, MAX_DECISION_FIELD_BYTES);
        let question = BoundedText::new(question, MAX_DECISION_FIELD_BYTES);
        if key.as_str().is_empty() || question.as_str().is_empty() {
            return;
        }
        if let Some(existing) =
            self.unresolved_questions.iter_mut().find(|existing| existing.key == key)
        {
            existing.question = question;
            return;
        }
        if self.unresolved_questions.len() == MAX_DECISION_QUESTIONS {
            self.unresolved_questions.remove(0);
        }
        self.unresolved_questions.push(DecisionQuestion { key, question });
    }

    /// Resolve one question by its stable key.
    pub fn clear_question(&mut self, key: &str) {
        self.unresolved_questions.retain(|question| question.key.as_str() != key);
    }

    /// Insert or update a candidate and retain the highest-scoring bounded set.
    pub fn upsert_candidate(
        &mut self,
        path: impl AsRef<str>,
        score: usize,
        evidence: usize,
        inspected: bool,
        modified: bool,
        stale: bool,
    ) {
        let path = BoundedText::new(path, MAX_DECISION_FIELD_BYTES);
        if path.as_str().is_empty() {
            return;
        }
        let score = u16::try_from(score.min(u16::MAX as usize)).unwrap_or(u16::MAX);
        let evidence = u16::try_from(evidence.min(u16::MAX as usize)).unwrap_or(u16::MAX);
        if let Some(existing) =
            self.candidate_files.iter_mut().find(|candidate| candidate.path == path)
        {
            existing.score = existing.score.max(score);
            existing.evidence = existing.evidence.max(evidence);
            existing.inspected |= inspected;
            existing.modified |= modified;
            existing.stale = stale;
        } else {
            self.candidate_files.push(DecisionCandidate {
                path,
                score,
                evidence,
                inspected,
                modified,
                stale,
            });
        }
        self.sort_candidates();
    }

    /// Record one path in the minimal affected scope.
    pub fn record_modified_scope(&mut self, path: &str) {
        let path = BoundedText::new(path, MAX_DECISION_FIELD_BYTES);
        if path.is_truncated() || path.as_str().is_empty() {
            return;
        }
        if self.modified_scope.iter().any(|existing| existing == path.as_str()) {
            return;
        }
        self.modified_scope.push(path.into_string());
        if self.modified_scope.len() > MAX_DECISION_SCOPE {
            self.modified_scope.remove(0);
        }
    }

    /// Mark one observation as valuable or low-value for no-progress detection.
    pub fn note_observation_value(&mut self, advanced: bool) {
        if advanced {
            self.low_value_observations = 0;
            self.progress_confidence = self.progress_confidence.saturating_add(2).min(100);
        } else {
            self.low_value_observations = self.low_value_observations.saturating_add(1);
        }
    }

    /// Set the compact model-facing recommendation.
    pub fn set_next(
        &mut self,
        action: DecisionAction,
        target: Option<impl AsRef<str>>,
        reason: impl AsRef<str>,
    ) {
        self.next_action = action;
        self.next_target = target.map(|target| BoundedText::new(target, MAX_DECISION_FIELD_BYTES));
        self.next_reason = BoundedText::new(reason, MAX_DECISION_FIELD_BYTES);
    }

    /// Clear workspace-derived evidence after drift.
    pub fn reset_for_workspace_change(&mut self) {
        self.evidence.clear();
        self.unresolved_questions.clear();
        self.candidate_files.clear();
        self.modified_scope.clear();
        self.verification_scope = BoundedText::default();
        self.progress_confidence = 0;
        self.low_value_observations = 0;
        self.set_next(
            DecisionAction::DiscoverTarget,
            None::<&str>,
            "workspace changed; rediscover targets",
        );
    }

    /// Enforce every persisted bound after restoring a session.
    pub fn enforce_bounds(&mut self) {
        self.evidence.truncate(MAX_DECISION_EVIDENCE);
        self.unresolved_questions.truncate(MAX_DECISION_QUESTIONS);
        self.candidate_files.truncate(MAX_DECISION_CANDIDATES);
        self.modified_scope.truncate(MAX_DECISION_SCOPE);
        for evidence in &mut self.evidence {
            evidence.path = BoundedText::new(evidence.path.as_str(), MAX_DECISION_FIELD_BYTES);
            evidence.detail = BoundedText::new(evidence.detail.as_str(), MAX_DECISION_FIELD_BYTES);
        }
        for question in &mut self.unresolved_questions {
            question.key = BoundedText::new(question.key.as_str(), MAX_DECISION_FIELD_BYTES);
            question.question =
                BoundedText::new(question.question.as_str(), MAX_DECISION_FIELD_BYTES);
        }
        for candidate in &mut self.candidate_files {
            candidate.path = BoundedText::new(candidate.path.as_str(), MAX_DECISION_FIELD_BYTES);
        }
        for path in &mut self.modified_scope {
            *path = BoundedText::new(path.as_str(), MAX_DECISION_FIELD_BYTES).into_string();
        }
        self.modified_scope.retain(|path| !path.is_empty());
        self.verification_scope =
            BoundedText::new(self.verification_scope.as_str(), MAX_DECISION_FIELD_BYTES);
        self.next_target = self
            .next_target
            .as_ref()
            .map(|target| BoundedText::new(target.as_str(), MAX_DECISION_FIELD_BYTES));
        self.next_reason = BoundedText::new(self.next_reason.as_str(), MAX_DECISION_FIELD_BYTES);
        self.progress_confidence = self.progress_confidence.min(100);
        self.sort_candidates();
    }

    fn sort_candidates(&mut self) {
        self.candidate_files.sort_unstable_by(|left, right| {
            right.score.cmp(&left.score).then_with(|| left.path.as_str().cmp(right.path.as_str()))
        });
        self.candidate_files.truncate(MAX_DECISION_CANDIDATES);
    }
}

#[cfg(test)]
mod tests {
    use crate::{PermissionClass, PreparedAction, ToolCallId, ToolInvocation};

    use super::*;

    #[test]
    fn evidence_origin_is_retained_without_granting_user_authority() {
        let mut state = DecisionState::new();
        state.record_evidence_with_provenance(
            EvidenceProvenance::ToolOutput,
            DecisionEvidenceKind::Inspection,
            "src/lib.rs",
            "read completed",
            42,
            0,
        );
        assert_eq!(state.evidence[0].provenance, EvidenceProvenance::ToolOutput);

        let action = PreparedAction::fallback(
            ToolInvocation {
                call_id: ToolCallId::new("model-write").expect("call id"),
                name: "write".to_owned(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            },
            PermissionClass::WorkspaceWrite,
        );
        assert_eq!(action.provenance(), EvidenceProvenance::Model);
        assert!(action.requirements.user_authorization);
        assert!(action.requirements.current_workspace_evidence);
    }
}
