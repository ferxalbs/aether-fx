use serde::{Deserialize, Serialize};

use crate::{AgentEvent, OpaqueContinuation, SessionId, TurnId};

/// Version of the append-friendly local JSONL schema.
pub const SESSION_SCHEMA_VERSION: u32 = 1;

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
    /// Session header.
    Started,
    /// User input.
    UserPrompt { prompt: String },
    /// A rendered-safe agent event.
    AgentEvent { event: AgentEvent },
    /// Opaque backend continuation needed by a later step.
    Continuation { continuation: Option<OpaqueContinuation> },
    /// Explicitly closed session.
    Finished,
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
