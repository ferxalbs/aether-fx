use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use aether_core::{
    BoundedText, ContextSnapshot, MAX_PROMPT_BYTES, MAX_SESSION_FILE_BYTES, MAX_SESSION_LINE_BYTES,
    MAX_SESSION_LINES, MAX_STORED_TURNS, OpaqueContinuation, SESSION_SCHEMA_VERSION, SessionId,
    SessionLine, SessionRecord, TurnId, payload_contains_secrets, persistable_continuation,
};
use thiserror::Error;

use crate::context::ContextEngine;

/// Session persistence failures with operation context.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// Filesystem operation failed.
    #[error("session {operation} failed: {message}")]
    Io { operation: String, message: String },
    /// A JSONL line was invalid, too large, or used an unsupported schema.
    #[error("session record invalid: {0}")]
    Invalid(String),
}

/// Append-friendly local JSONL session store.
#[derive(Clone, Debug)]
pub struct SessionStore {
    path: PathBuf,
    session_id: SessionId,
    next_sequence: u64,
}

/// Restored local session used by `aether resume`.
#[derive(Clone, Debug)]
pub struct ResumableSession {
    /// Restored session identity.
    pub session_id: SessionId,
    /// Live workspace root after validation.
    pub workspace_root: PathBuf,
    /// Model stored with the session, if any.
    pub model: Option<String>,
    /// Sanitized Rainy continuation, if still present.
    pub continuation: Option<OpaqueContinuation>,
    /// Working context after stale-file refresh.
    pub context: ContextSnapshot,
    /// Whether the live workspace differed from persisted hashes/paths.
    pub workspace_changed: bool,
    /// Human-readable resume notes.
    pub warnings: Vec<String>,
}

impl SessionStore {
    /// Return `{workspace}/.aether/sessions`.
    pub fn directory_for(workspace: impl AsRef<Path>) -> PathBuf {
        workspace.as_ref().join(".aether").join("sessions")
    }

    /// Return the JSONL path for a session id inside a workspace.
    pub fn path_for(workspace: impl AsRef<Path>, session_id: &SessionId) -> PathBuf {
        Self::directory_for(workspace).join(format!("{}.jsonl", session_id.as_str()))
    }

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

    /// Append a record, fsync it, and compact the file when bounds are exceeded.
    pub fn append(
        &mut self,
        turn_id: Option<TurnId>,
        record: SessionRecord,
    ) -> Result<SessionLine, SessionStoreError> {
        let record = sanitize_record(record)?;
        let line = SessionLine::new(self.next_sequence, self.session_id.clone(), turn_id, record);
        let encoded = serde_json::to_string(&line)
            .map_err(|error| SessionStoreError::Invalid(error.to_string()))?;
        if encoded.len() > MAX_SESSION_LINE_BYTES {
            return Err(SessionStoreError::Invalid("session record exceeds line limit".to_owned()));
        }
        if payload_contains_secrets(
            &serde_json::from_str::<serde_json::Value>(&encoded)
                .map_err(|error| SessionStoreError::Invalid(error.to_string()))?,
        ) {
            return Err(SessionStoreError::Invalid(
                "session record refused because it contains a secret".to_owned(),
            ));
        }
        let current_len = fs::metadata(&self.path).map(|metadata| metadata.len()).unwrap_or(0);
        if current_len.saturating_add(encoded.len() as u64).saturating_add(1)
            > MAX_SESSION_FILE_BYTES as u64
        {
            self.compact()?;
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
        file.sync_all().map_err(|error| SessionStoreError::Io {
            operation: "sync session record".to_owned(),
            message: error.to_string(),
        })?;
        self.next_sequence = self.next_sequence.saturating_add(1);
        let line_count = Self::replay(&self.path)?.len();
        if line_count > MAX_SESSION_LINES {
            self.compact()?;
        }
        Ok(line)
    }

    /// Read and validate bounded session lines for replay.
    pub fn replay(path: impl AsRef<Path>) -> Result<Vec<SessionLine>, SessionStoreError> {
        let path = path.as_ref();
        let metadata = fs::metadata(path).map_err(|error| SessionStoreError::Io {
            operation: "stat session file".to_owned(),
            message: error.to_string(),
        })?;
        if metadata.len() > MAX_SESSION_FILE_BYTES as u64 {
            return Err(SessionStoreError::Invalid("session file exceeds size limit".to_owned()));
        }
        let file = File::open(path).map_err(|error| SessionStoreError::Io {
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
            let ended_with_newline = buffer.ends_with('\n');
            let trimmed = buffer.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLine>(trimmed) {
                Ok(line) => {
                    if line.schema_version != SESSION_SCHEMA_VERSION {
                        return Err(SessionStoreError::Invalid(format!(
                            "unsupported session schema {}",
                            line.schema_version
                        )));
                    }
                    lines.push(line);
                }
                Err(_) if !ended_with_newline => {
                    // A truncated last line is a crash window; drop it rather than fail resume.
                    break;
                }
                Err(error) => {
                    return Err(SessionStoreError::Invalid(error.to_string()));
                }
            }
            if lines.len() > MAX_SESSION_LINES {
                return Err(SessionStoreError::Invalid(
                    "session line count exceeds limit".to_owned(),
                ));
            }
        }
        Ok(lines)
    }

    /// Reconstruct bounded resume state from a JSONL file and the live workspace.
    pub fn restore(
        path: impl AsRef<Path>,
        live_workspace: impl AsRef<Path>,
    ) -> Result<ResumableSession, SessionStoreError> {
        let lines = Self::replay(path)?;
        if lines.is_empty() {
            return Err(SessionStoreError::Invalid("session file is empty".to_owned()));
        }
        let session_id = lines[0].session_id.clone();
        if lines.iter().any(|line| line.session_id != session_id) {
            return Err(SessionStoreError::Invalid("session id mismatch".to_owned()));
        }
        let mut workspace_root = None;
        let mut model = None;
        let mut continuation = None;
        let mut context = None;
        let mut prompts = Vec::new();
        for line in &lines {
            match &line.record {
                SessionRecord::Started { workspace_root: stored, model: stored_model } => {
                    workspace_root = Some(stored.clone());
                    model.clone_from(stored_model);
                }
                SessionRecord::UserPrompt { prompt } => {
                    prompts.push(prompt.clone());
                    if prompts.len() > MAX_STORED_TURNS {
                        prompts.remove(0);
                    }
                }
                SessionRecord::Continuation { continuation: stored } => {
                    continuation = stored.as_ref().and_then(persistable_continuation);
                }
                SessionRecord::ContextSnapshot { context: stored } => {
                    context = Some((**stored).clone());
                }
                SessionRecord::TurnCompleted { .. } | SessionRecord::Finished => {}
            }
        }
        let live_workspace =
            fs::canonicalize(live_workspace.as_ref()).map_err(|error| SessionStoreError::Io {
                operation: "canonicalize live workspace".to_owned(),
                message: error.to_string(),
            })?;
        if !live_workspace.is_dir() {
            return Err(SessionStoreError::Invalid("workspace is not a directory".to_owned()));
        }
        let mut warnings = Vec::new();
        let mut workspace_changed = false;
        if let Some(stored) = workspace_root.as_deref() {
            match fs::canonicalize(stored) {
                Ok(canonical) if canonical != live_workspace => {
                    workspace_changed = true;
                    warnings.push(format!(
                        "stored workspace {} differs from live workspace {}",
                        canonical.display(),
                        live_workspace.display()
                    ));
                }
                Ok(_) => {}
                Err(_) => {
                    workspace_changed = true;
                    warnings.push("stored workspace path is no longer valid".to_owned());
                }
            }
        } else {
            return Err(SessionStoreError::Invalid(
                "session is missing a workspace root".to_owned(),
            ));
        }
        let mut engine = ContextEngine::restore(context.unwrap_or_else(|| {
            ContextSnapshot::new(live_workspace.display().to_string(), model.clone())
        }));
        if let Some(prompt) = prompts.last()
            && engine.snapshot().current_task.as_str().is_empty()
        {
            engine.set_task(prompt);
        }
        engine.set_model(model.clone());
        engine.set_continuation(continuation.clone());
        let stale = engine.refresh_against_workspace(&live_workspace);
        if !stale.is_empty() {
            workspace_changed = true;
            warnings.push(format!("stale file state detected: {}", stale.join(", ")));
        }
        if workspace_changed {
            engine.set_workspace_changed(true);
        }
        let context = engine.into_snapshot();
        Ok(ResumableSession {
            session_id,
            workspace_root: live_workspace,
            model: context.model.clone(),
            continuation: context.continuation.clone(),
            workspace_changed,
            context,
            warnings,
        })
    }

    /// Return the backing path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn compact(&mut self) -> Result<(), SessionStoreError> {
        let lines = Self::replay(&self.path).unwrap_or_default();
        let mut started = None;
        let mut continuation = None;
        let mut context = None;
        let mut turns: Vec<SessionLine> = Vec::new();
        for line in lines {
            match &line.record {
                SessionRecord::Started { .. } => started = Some(line),
                SessionRecord::Continuation { .. } => continuation = Some(line),
                SessionRecord::ContextSnapshot { .. } => context = Some(line),
                SessionRecord::UserPrompt { .. } | SessionRecord::TurnCompleted { .. } => {
                    turns.push(line);
                    let overflow = turns.len().saturating_sub(MAX_STORED_TURNS.saturating_mul(2));
                    if overflow > 0 {
                        turns.drain(0..overflow);
                    }
                }
                SessionRecord::Finished => {}
            }
        }
        let mut kept = Vec::new();
        if let Some(started) = started {
            kept.push(started);
        }
        kept.extend(turns);
        if let Some(context) = context {
            kept.push(context);
        }
        if let Some(continuation) = continuation {
            kept.push(continuation);
        }
        for (index, line) in kept.iter_mut().enumerate() {
            line.sequence = (index as u64).saturating_add(1);
            line.schema_version = SESSION_SCHEMA_VERSION;
        }
        let encoded = kept
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| SessionStoreError::Invalid(error.to_string()))?
            .join("\n");
        let body = if encoded.is_empty() { String::new() } else { format!("{encoded}\n") };
        if body.len() > MAX_SESSION_FILE_BYTES {
            return Err(SessionStoreError::Invalid(
                "compacted session still exceeds size limit".to_owned(),
            ));
        }
        atomic_replace_file(&self.path, body.as_bytes())?;
        self.next_sequence = kept.last().map(|line| line.sequence.saturating_add(1)).unwrap_or(1);
        Ok(())
    }
}

fn sanitize_record(record: SessionRecord) -> Result<SessionRecord, SessionStoreError> {
    let record = match record {
        SessionRecord::UserPrompt { prompt } => SessionRecord::UserPrompt {
            prompt: BoundedText::new(prompt, MAX_PROMPT_BYTES).into_string(),
        },
        SessionRecord::Continuation { continuation } => SessionRecord::Continuation {
            continuation: continuation.as_ref().and_then(persistable_continuation),
        },
        SessionRecord::ContextSnapshot { context } => {
            if payload_contains_secrets(
                &serde_json::to_value(&context).unwrap_or(serde_json::Value::Null),
            ) {
                return Err(SessionStoreError::Invalid(
                    "context snapshot refused because it contains a secret".to_owned(),
                ));
            }
            let mut context = context;
            context.continuation = context.continuation.as_ref().and_then(persistable_continuation);
            SessionRecord::ContextSnapshot { context }
        }
        other => other,
    };
    Ok(record)
}

fn atomic_replace_file(path: &Path, bytes: &[u8]) -> Result<(), SessionStoreError> {
    let directory = path.parent().unwrap_or(Path::new("."));
    let temporary = directory.join(format!(
        ".aether-session-{}-{}.tmp",
        std::process::id(),
        path.file_name().and_then(|name| name.to_str()).unwrap_or("session")
    ));
    let mut file =
        OpenOptions::new().create_new(true).write(true).open(&temporary).map_err(|error| {
            SessionStoreError::Io {
                operation: "create compacted session".to_owned(),
                message: error.to_string(),
            }
        })?;
    let result = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(SessionStoreError::Io {
            operation: "write compacted session".to_owned(),
            message: error.to_string(),
        });
    }
    drop(file);
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        SessionStoreError::Io {
            operation: "install compacted session".to_owned(),
            message: error.to_string(),
        }
    })
}

/// Inputs for one bounded JSONL turn persist.
pub struct PersistTurn<'a> {
    /// Turn identity.
    pub turn_id: &'a TurnId,
    /// Bounded user prompt.
    pub prompt: &'a str,
    /// Semantic model steps used.
    pub steps: u16,
    /// Whether the turn ended cancelled.
    pub cancelled: bool,
    /// Bounded assistant text.
    pub text: BoundedText,
    /// Working context after the turn.
    pub context: &'a ContextSnapshot,
    /// Sanitized Rainy continuation.
    pub continuation: Option<OpaqueContinuation>,
}

/// Persist one completed turn using spawn-friendly synchronous writes.
pub fn persist_turn(
    store: &mut SessionStore,
    turn: PersistTurn<'_>,
) -> Result<(), SessionStoreError> {
    store.append(
        Some(turn.turn_id.clone()),
        SessionRecord::UserPrompt { prompt: turn.prompt.to_owned() },
    )?;
    store.append(
        Some(turn.turn_id.clone()),
        SessionRecord::TurnCompleted {
            steps: turn.steps,
            cancelled: turn.cancelled,
            text: turn.text,
        },
    )?;
    store.append(
        Some(turn.turn_id.clone()),
        SessionRecord::ContextSnapshot { context: Box::new(turn.context.clone()) },
    )?;
    store.append(Some(turn.turn_id.clone()), SessionRecord::continuation(turn.continuation))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::{ContextSnapshot, SESSION_SCHEMA_VERSION};

    fn temp_session(label: &str) -> (PathBuf, SessionId) {
        let root = std::env::temp_dir().join(format!(
            "aether-session-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let session = SessionId::new(format!("s-{label}")).unwrap();
        let path = SessionStore::path_for(&root, &session);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        (path, session)
    }

    #[test]
    fn session_write_read_round_trip_persists_continuation() {
        let (path, session) = temp_session("roundtrip");
        let mut store = SessionStore::open(&path, session.clone()).unwrap();
        store
            .append(
                None,
                SessionRecord::Started {
                    workspace_root: "/workspace".to_owned(),
                    model: Some("model-a".to_owned()),
                },
            )
            .unwrap();
        store
            .append(
                None,
                SessionRecord::continuation(Some(OpaqueContinuation(serde_json::json!({
                    "previous_response_id": "resp_1",
                    "reasoning": "hidden"
                })))),
            )
            .unwrap();
        let lines = SessionStore::replay(&path).unwrap();
        assert_eq!(lines[0].schema_version, SESSION_SCHEMA_VERSION);
        match &lines[1].record {
            SessionRecord::Continuation { continuation } => {
                assert_eq!(
                    continuation.as_ref().unwrap().0,
                    serde_json::json!({"previous_response_id": "resp_1"})
                );
            }
            other => panic!("unexpected record {other:?}"),
        }
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn session_corruption_handling_skips_truncated_tail_and_rejects_middle() {
        let (path, session) = temp_session("corrupt");
        let mut store = SessionStore::open(&path, session.clone()).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: "/workspace".to_owned(), model: None },
            )
            .unwrap();
        std::fs::write(
            &path,
            format!("{}{{\"truncated\"", std::fs::read_to_string(&path).unwrap()),
        )
        .unwrap();
        let lines = SessionStore::replay(&path).unwrap();
        assert_eq!(lines.len(), 1);

        let (path, session) = temp_session("corrupt-middle");
        let mut store = SessionStore::open(&path, session).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: "/workspace".to_owned(), model: None },
            )
            .unwrap();
        std::fs::write(&path, "{\"schema_version\":2}\nnot-json\n").unwrap();
        assert!(SessionStore::replay(&path).is_err());
    }

    #[test]
    fn schema_version_rejection() {
        let (path, _) = temp_session("schema");
        std::fs::write(
            &path,
            r#"{"schema_version":99,"sequence":1,"session_id":"s-schema","turn_id":null,"record":{"type":"finished"}}
"#,
        )
        .unwrap();
        let error = SessionStore::replay(&path).unwrap_err().to_string();
        assert!(error.contains("unsupported session schema 99"));
    }

    #[test]
    fn no_secrets_persisted_in_session_records() {
        let (path, session) = temp_session("secrets");
        let mut store = SessionStore::open(&path, session).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: "/workspace".to_owned(), model: None },
            )
            .unwrap();
        let mut context = ContextSnapshot::new("/workspace", None);
        context.continuation = Some(OpaqueContinuation(serde_json::json!({
            "previous_response_id": "resp_1",
            "RAINY_API_KEY": "must-not-store"
        })));
        let error = store
            .append(None, SessionRecord::ContextSnapshot { context: Box::new(context) })
            .unwrap_err()
            .to_string();
        assert!(error.contains("secret"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("RAINY_API_KEY"));
        assert!(!body.contains("must-not-store"));
    }

    #[test]
    fn resume_detects_workspace_and_stale_file_changes() {
        let root = std::env::temp_dir().join(format!(
            "aether-resume-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "old").unwrap();
        let session = SessionId::new("s-resume").unwrap();
        let path = SessionStore::path_for(&root, &session);
        let mut store = SessionStore::open(&path, session).unwrap();
        store
            .append(
                None,
                SessionRecord::Started {
                    workspace_root: root.display().to_string(),
                    model: Some("model-a".to_owned()),
                },
            )
            .unwrap();
        let mut context =
            ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned()));
        context.inspected.push(aether_core::InspectedFile {
            path: "main.rs".to_owned(),
            content_hash: Some(blake3::hash(b"old").to_hex().to_string()),
            ranges: vec![aether_core::LineRange { start: 1, end: 1 }],
            last_state: aether_core::ObservedFileState::Present,
            stale: false,
        });
        context.continuation = Some(OpaqueContinuation(serde_json::json!({
            "previous_response_id": "resp_resume"
        })));
        store.append(None, SessionRecord::ContextSnapshot { context: Box::new(context) }).unwrap();
        store
            .append(
                None,
                SessionRecord::continuation(Some(OpaqueContinuation(serde_json::json!({
                    "previous_response_id": "resp_resume"
                })))),
            )
            .unwrap();
        std::fs::write(root.join("main.rs"), "new").unwrap();
        let restored = SessionStore::restore(&path, &root).unwrap();
        assert!(restored.workspace_changed);
        assert!(restored.context.inspected[0].stale);
        assert_eq!(
            restored.continuation.unwrap().0.get("previous_response_id").unwrap(),
            "resp_resume"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
