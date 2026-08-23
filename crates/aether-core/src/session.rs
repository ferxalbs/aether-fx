use serde::{Deserialize, Serialize};

use crate::{
    BoundedText, ContextSnapshot, OpaqueContinuation, SessionId, TurnId, persistable_continuation,
};

/// Version of the append-friendly local JSONL schema.
pub const SESSION_SCHEMA_VERSION: u32 = 2;

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

/// Append-only local session records.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionRecord {
    /// Session header with workspace and model identity.
    Started { workspace_root: String, model: Option<String> },
    /// Bounded user input.
    UserPrompt { prompt: String },
    /// Compact completed-turn metadata. Raw tool output is not stored.
    TurnCompleted { steps: u16, cancelled: bool, text: BoundedText },
    /// Opaque backend continuation needed by a later step.
    Continuation { continuation: Option<OpaqueContinuation> },
    /// Bounded working-context snapshot.
    ContextSnapshot { context: Box<ContextSnapshot> },
    /// Explicitly closed session.
    Finished,
}

impl SessionRecord {
    /// Persist only opaque continuation identity keys.
    pub fn continuation(continuation: Option<OpaqueContinuation>) -> Self {
        Self::Continuation {
            continuation: continuation.as_ref().and_then(persistable_continuation),
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
            SessionRecord::UserPrompt { prompt: "hello".to_owned() },
        );
        let encoded = serde_json::to_string(&line).unwrap();
        assert_eq!(serde_json::from_str::<SessionLine>(&encoded).unwrap(), line);
    }
}
