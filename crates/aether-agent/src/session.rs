use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aether_core::{SESSION_SCHEMA_VERSION, SessionId, SessionLine, SessionRecord, TurnId};
use thiserror::Error;

const MAX_SESSION_LINE_BYTES: usize = 256 * 1024;
const MAX_SESSION_LINES: usize = 100_000;

/// Session persistence failures with operation context.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// Filesystem operation failed.
    #[error("session {operation} failed: {message}")]
    Io { operation: String, message: String },
    /// A JSONL line was invalid or too large.
    #[error("session record invalid: {0}")]
    Invalid(String),
}

/// Append-friendly local JSONL session store.
pub struct SessionStore {
    path: PathBuf,
    session_id: SessionId,
    next_sequence: u64,
}

impl SessionStore {
    /// Open or create a session file and recover its next sequence number.
    pub fn open(
        path: impl Into<PathBuf>,
        session_id: SessionId,
    ) -> Result<Self, SessionStoreError> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| SessionStoreError::Io {
                operation: "create session directory".to_owned(),
                message: error.to_string(),
            })?;
        }
        let mut next_sequence = 1;
        if path.exists() {
            for line in Self::replay(&path)? {
                next_sequence = next_sequence.max(line.sequence.saturating_add(1));
            }
        }
        Ok(Self { path, session_id, next_sequence })
    }

    /// Append a record and flush it to the OS.
    pub fn append(
        &mut self,
        turn_id: Option<TurnId>,
        record: SessionRecord,
    ) -> Result<SessionLine, SessionStoreError> {
        let line = SessionLine::new(self.next_sequence, self.session_id.clone(), turn_id, record);
        let encoded = serde_json::to_string(&line)
            .map_err(|error| SessionStoreError::Invalid(error.to_string()))?;
        if encoded.len() > MAX_SESSION_LINE_BYTES {
            return Err(SessionStoreError::Invalid("session record exceeds line limit".to_owned()));
        }
        let mut file =
            OpenOptions::new().create(true).append(true).open(&self.path).map_err(|error| {
                SessionStoreError::Io {
                    operation: "open session file".to_owned(),
                    message: error.to_string(),
                }
            })?;
        file.write_all(encoded.as_bytes()).map_err(|error| SessionStoreError::Io {
            operation: "append session record".to_owned(),
            message: error.to_string(),
        })?;
        file.write_all(b"\n").map_err(|error| SessionStoreError::Io {
            operation: "terminate session record".to_owned(),
            message: error.to_string(),
        })?;
        file.flush().map_err(|error| SessionStoreError::Io {
            operation: "flush session record".to_owned(),
            message: error.to_string(),
        })?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        Ok(line)
    }

    /// Read and validate all bounded session lines for replay.
    pub fn replay(path: impl AsRef<Path>) -> Result<Vec<SessionLine>, SessionStoreError> {
        let file = File::open(path.as_ref()).map_err(|error| SessionStoreError::Io {
            operation: "read session file".to_owned(),
            message: error.to_string(),
        })?;
        let mut reader = BufReader::new(file);
        let mut lines = Vec::new();
        let mut buffer = String::new();
        loop {
            buffer.clear();
            let read = reader.read_line(&mut buffer).map_err(|error| SessionStoreError::Io {
                operation: "read session line".to_owned(),
                message: error.to_string(),
            })?;
            if read == 0 {
                break;
            }
            if read > MAX_SESSION_LINE_BYTES {
                return Err(SessionStoreError::Invalid("session line exceeds limit".to_owned()));
            }
            let line: SessionLine = serde_json::from_str(buffer.trim_end())
                .map_err(|error| SessionStoreError::Invalid(error.to_string()))?;
            if line.schema_version != SESSION_SCHEMA_VERSION {
                return Err(SessionStoreError::Invalid(format!(
                    "unsupported session schema {}",
                    line.schema_version
                )));
            }
            lines.push(line);
            if lines.len() > MAX_SESSION_LINES {
                return Err(SessionStoreError::Invalid(
                    "session line count exceeds limit".to_owned(),
                ));
            }
        }
        Ok(lines)
    }

    /// Return the backing path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}
