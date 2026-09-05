use serde::{Deserialize, Serialize};

use crate::{
    ContextSnapshot, OpaqueContinuation, SessionId, TranscriptCell, TurnId,
    persistable_continuation, sanitize_transcript,
};

/// Version of the append-friendly local JSONL schema.
pub const SESSION_SCHEMA_VERSION: u32 = 6;
/// The immediately previous session schema remains readable after adding workflow defaults.
pub const MIN_SUPPORTED_SESSION_SCHEMA_VERSION: u32 = 3;

/// One versioned line in a local session JSONL file.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct SessionLine {
    /// Schema version for forward-compatible replay.
    pub schema_version: u32,
    /// Monotonic line number.
    pub sequence: u64,
    /// Session identity.
    pub session_id: SessionId,
    /// Turn identity when the record belongs to a turn.
    pub turn_id: Option<TurnId>,
    /// Append-only record payload.
    pub record: SessionRecord,
}

/// One committed, crash-atomic turn record.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TurnSnapshot {
    /// Turn identity for this committed record.
    pub turn_id: TurnId,
    /// Semantic model steps used.
    pub steps: u16,
    /// Whether the turn ended cancelled.
    pub cancelled: bool,
    /// Minimized context metadata safe to persist.
    pub context: ContextSnapshot,
    /// Sanitized Rainy continuation identity, if any.
    pub continuation: Option<OpaqueContinuation>,
    /// Safe semantic transcript cells restored by the interactive TUI.
    #[serde(
        default,
        deserialize_with = "deserialize_transcript",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub transcript: Vec<TranscriptCell>,
}

fn deserialize_transcript<'de, D>(deserializer: D) -> Result<Vec<TranscriptCell>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    Ok(values
        .into_iter()
        .filter_map(|value| serde_json::from_value::<TranscriptCell>(value).ok())
        .collect())
}

/// Append-only local session records.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    /// Session header with workspace and model identity.
    Started { workspace_root: String, model: Option<String> },
    /// One complete committed turn. Partial writes of this record are ignored on replay.
    TurnSnapshot { snapshot: Box<TurnSnapshot> },
    /// A durable checkpoint for a cancelled or incomplete turn. It is resumable but not a claim
    /// that the requested task completed.
    Checkpoint { snapshot: Box<TurnSnapshot> },
    /// Explicitly closed session.
    Finished,
}

impl SessionRecord {
    /// Construct a committed turn after applying persistable-context minimization.
    pub fn turn_snapshot(snapshot: TurnSnapshot) -> Self {
        Self::snapshot(snapshot, false)
    }

    /// Construct a resumable checkpoint after applying the same persistence minimization.
    pub fn checkpoint(snapshot: TurnSnapshot) -> Self {
        Self::snapshot(snapshot, true)
    }

    fn snapshot(mut snapshot: TurnSnapshot, checkpoint: bool) -> Self {
        snapshot.context = snapshot.context.persistable();
        snapshot.continuation = snapshot.continuation.as_ref().and_then(persistable_continuation);
        snapshot.transcript = sanitize_transcript(snapshot.transcript);
        if checkpoint {
            Self::Checkpoint { snapshot: Box::new(snapshot) }
        } else {
            Self::TurnSnapshot { snapshot: Box::new(snapshot) }
        }
    }
}

impl SessionLine {
    /// Construct a schema-versioned line.
    pub fn new(
        sequence: u64,
        session_id: SessionId,
        turn_id: Option<TurnId>,
        record: SessionRecord,
    ) -> Self {
        Self { schema_version: SESSION_SCHEMA_VERSION, sequence, session_id, turn_id, record }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_lines_round_trip() {
        let session = SessionId::new("session-1").unwrap();
        let line = SessionLine::new(
            1,
            session,
            None,
            SessionRecord::Started {
                workspace_root: "/workspace".to_owned(),
                model: Some("model-a".to_owned()),
            },
        );
        let encoded = serde_json::to_string(&line).unwrap();
        assert_eq!(serde_json::from_str::<SessionLine>(&encoded).unwrap(), line);
    }
}
