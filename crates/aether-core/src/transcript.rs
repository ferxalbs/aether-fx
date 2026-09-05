use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

use crate::{AppointmentId, BoundedText, TurnId, payload_contains_secrets};

/// Maximum semantic transcript cells retained in a session snapshot.
pub const MAX_TRANSCRIPT_CELLS: usize = 256;
/// Maximum serialized bytes retained for one semantic transcript snapshot.
pub const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
/// Maximum bytes retained for one persisted transcript text field.
pub const MAX_TRANSCRIPT_TEXT_BYTES: usize = 16 * 1024;

/// Safe, semantic transcript content that may be restored into the TUI.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TranscriptCell {
    /// Submitted user prompt.
    User { text: BoundedText },
    /// Rendered assistant response text.
    Assistant { text: BoundedText },
    /// Tool identity and terminal outcome without raw output or arguments.
    Tool { name: BoundedText, ok: Option<bool> },
    /// A bounded warning shown in the transcript.
    Warning { message: BoundedText },
    /// A bounded error shown in the transcript.
    Error { message: BoundedText },
    /// A compact completed-turn summary.
    TurnSummary { turn_id: TurnId, summary: BoundedText },
    /// A confirmed local appointment summary without private notes or attendees.
    AppointmentConfirmed {
        id: AppointmentId,
        title: BoundedText,
        starts_at: String,
        /// Explicit offset retained so resumed transcript cards keep local-time meaning.
        #[serde(default)]
        utc_offset_minutes: i32,
        duration_minutes: u16,
    },
}

/// Sanitize, bound, and secret-filter transcript cells before persistence.
pub fn sanitize_transcript(cells: impl IntoIterator<Item = TranscriptCell>) -> Vec<TranscriptCell> {
    let mut safe = cells.into_iter().filter_map(sanitize_cell).collect::<Vec<_>>();
    if safe.len() > MAX_TRANSCRIPT_CELLS {
        let keep_from = safe.len().saturating_sub(MAX_TRANSCRIPT_CELLS);
        safe = safe.split_off(keep_from);
    }
    let mut retained = Vec::with_capacity(safe.len());
    let mut bytes: usize = 0;
    for cell in safe.drain(..).rev() {
        let size = serde_json::to_vec(&cell).map(|value| value.len()).unwrap_or(usize::MAX);
        if size == usize::MAX || bytes.saturating_add(size) > MAX_TRANSCRIPT_BYTES {
            continue;
        }
        bytes = bytes.saturating_add(size);
        retained.push(cell);
    }
    retained.reverse();
    retained
}

fn sanitize_cell(cell: TranscriptCell) -> Option<TranscriptCell> {
    let cell = match cell {
        TranscriptCell::User { text } => {
            TranscriptCell::User { text: clean_text(text.as_str(), true) }
        }
        TranscriptCell::Assistant { text } => {
            TranscriptCell::Assistant { text: clean_text(text.as_str(), true) }
        }
        TranscriptCell::Tool { name, ok } => {
            TranscriptCell::Tool { name: clean_text(name.as_str(), false), ok }
        }
        TranscriptCell::Warning { message } => {
            TranscriptCell::Warning { message: clean_text(message.as_str(), true) }
        }
        TranscriptCell::Error { message } => {
            TranscriptCell::Error { message: clean_text(message.as_str(), true) }
        }
        TranscriptCell::TurnSummary { turn_id, summary } => {
            TranscriptCell::TurnSummary { turn_id, summary: clean_text(summary.as_str(), true) }
        }
        TranscriptCell::AppointmentConfirmed {
            id,
            title,
            starts_at,
            utc_offset_minutes,
            duration_minutes,
        } => {
            AppointmentId::new(id.as_str().to_owned()).ok()?;
            if !(-1439..=1439).contains(&utc_offset_minutes) {
                return None;
            }
            if !(1..=1440).contains(&duration_minutes) {
                return None;
            }
            let parsed = OffsetDateTime::parse(&starts_at, &Rfc3339).ok()?;
            let canonical = parsed.to_offset(UtcOffset::UTC).format(&Rfc3339).ok()?;
            if canonical != starts_at {
                return None;
            }
            let title = clean_text(title.as_str(), false);
            if title.as_str().trim().is_empty() {
                return None;
            }
            TranscriptCell::AppointmentConfirmed {
                id,
                title,
                starts_at: starts_at
                    .chars()
                    .filter(|character| !character.is_control())
                    .take(64)
                    .collect(),
                utc_offset_minutes,
                duration_minutes,
            }
        }
    };
    let value = serde_json::to_value(&cell).ok()?;
    (!payload_contains_secrets(&value)).then_some(cell)
}

fn clean_text(value: &str, allow_newlines: bool) -> BoundedText {
    let cleaned = value
        .chars()
        .filter(|character| {
            !character.is_control() || (allow_newlines && matches!(character, '\n' | '\t'))
        })
        .collect::<String>();
    BoundedText::new(cleaned, MAX_TRANSCRIPT_TEXT_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_like_cells_are_not_persisted() {
        let cells = sanitize_transcript([
            TranscriptCell::User { text: BoundedText::new("RAINY_API_KEY=do-not-store", 1024) },
            TranscriptCell::Assistant { text: BoundedText::new("safe", 1024) },
        ]);
        assert_eq!(cells.len(), 1);
        assert!(matches!(cells[0], TranscriptCell::Assistant { .. }));
    }

    #[test]
    fn transcript_retains_the_newest_bounded_cells() {
        let cells = (0..(MAX_TRANSCRIPT_CELLS + 4))
            .map(|index| TranscriptCell::User {
                text: BoundedText::new(format!("prompt-{index}"), MAX_TRANSCRIPT_TEXT_BYTES),
            })
            .collect::<Vec<_>>();
        let safe = sanitize_transcript(cells);
        assert_eq!(safe.len(), MAX_TRANSCRIPT_CELLS);
        assert!(matches!(&safe[0], TranscriptCell::User { text } if text.as_str() == "prompt-4"));
        assert!(
            matches!(&safe[MAX_TRANSCRIPT_CELLS - 1], TranscriptCell::User { text } if text.as_str() == "prompt-259")
        );
    }

    #[test]
    fn corrupt_transcript_cells_are_skipped_by_session_deserialization() {
        let snapshot = crate::TurnSnapshot {
            turn_id: crate::TurnId::new("turn-1").unwrap(),
            steps: 1,
            cancelled: false,
            context: crate::ContextSnapshot::new("/tmp/project", None),
            continuation: None,
            transcript: Vec::new(),
        };
        let mut value = serde_json::to_value(snapshot).unwrap();
        value["transcript"] = serde_json::json!([
            {"type": "unknown"},
            {"type": "warning", "message": {"text": "safe", "limit": {"truncated": false, "original_bytes": 4, "retained_bytes": 4}}}
        ]);
        let parsed = serde_json::from_value::<crate::TurnSnapshot>(value).unwrap();
        assert_eq!(parsed.transcript.len(), 1);
    }
}
