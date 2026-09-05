use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use aether_core::{
    ContextSnapshot, MAX_SESSION_FILE_BYTES, MAX_SESSION_LINE_BYTES, MAX_SESSION_LINES,
    MAX_STORED_TURNS, MIN_SUPPORTED_SESSION_SCHEMA_VERSION, OpaqueContinuation,
    SESSION_SCHEMA_VERSION, SessionId, SessionLine, SessionRecord, TranscriptCell, TurnId,
    TurnSnapshot, payload_contains_secrets, persistable_continuation,
};
use thiserror::Error;

use crate::context::ContextEngine;
use crate::session_fs;

/// Session persistence failures with operation context.
#[derive(Debug, Error)]
pub enum SessionStoreError {
    /// A contained session entry was not present.
    #[error("session not found")]
    NotFound,
    /// Filesystem operation failed.
    #[error("session {operation} failed: {message}")]
    Io { operation: String, message: String },
    /// A JSONL line was invalid, too large, or used an unsupported schema.
    #[error("session record invalid: {0}")]
    Invalid(String),
}

pub(crate) const SESSION_TEMP_PREFIX: &str = ".aether-fx-session";

/// Append-friendly JSONL session store in private OS application-state storage.
#[derive(Clone, Debug)]
pub struct SessionStore {
    workspace: PathBuf,
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
    /// Sanitized Rainy continuation, if still present and not invalidated.
    pub continuation: Option<OpaqueContinuation>,
    /// Working context after stale-file refresh.
    pub context: ContextSnapshot,
    /// Safe semantic transcript restored for the interactive UI.
    pub transcript: Vec<TranscriptCell>,
    /// Whether the live workspace differed from persisted hashes/paths.
    pub workspace_changed: bool,
    /// Human-readable resume notes.
    pub warnings: Vec<String>,
}

/// Bounded, non-secret metadata used by the local session picker.
#[derive(Clone, Debug)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub model: Option<String>,
    pub last_turn: Option<TurnId>,
    pub workspace: Option<String>,
    pub modified: std::time::SystemTime,
}

impl SessionStore {
    /// Resolve the private AETHER Fx application-state root without creating it.
    pub fn state_root() -> Result<PathBuf, SessionStoreError> {
        session_fs::state_root()
    }

    /// Resolve the non-secret user configuration path outside the workspace.
    pub fn config_path(workspace: impl AsRef<Path>) -> Result<PathBuf, SessionStoreError> {
        session_fs::config_path(workspace, false)
    }

    /// Prepare the private state root and resolve the non-secret configuration path.
    pub fn prepare_config_path(workspace: impl AsRef<Path>) -> Result<PathBuf, SessionStoreError> {
        session_fs::config_path(workspace, true)
    }

    /// Return the canonical workspace path used for state binding and execution.
    pub fn canonical_workspace(workspace: impl AsRef<Path>) -> Result<PathBuf, SessionStoreError> {
        session_fs::canonical_workspace(workspace)
    }

    /// Derive the stable, platform-native workspace bucket identifier.
    pub fn workspace_id(workspace: impl AsRef<Path>) -> Result<String, SessionStoreError> {
        session_fs::workspace_id(workspace)
    }

    /// Discover contained session files without exposing their contents.
    pub fn summaries(
        workspace: impl AsRef<Path>,
    ) -> Result<(Vec<SessionSummary>, Vec<String>), SessionStoreError> {
        let entries = session_fs::session_entries(workspace.as_ref())?;
        let mut summaries = Vec::new();
        let mut issues = Vec::new();
        for entry in entries {
            let session_id = entry.session_id;
            let summary =
                match Self::summary_from_file(entry.file, session_id.clone(), entry.modified) {
                    Ok(summary) => summary,
                    Err(error) => {
                        issues.push(format!("{session_id}: {error}"));
                        continue;
                    }
                };
            summaries.push(summary);
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.modified));
        Ok((summaries, issues))
    }

    fn summary_from_file(
        mut file: File,
        session_id: SessionId,
        modified: std::time::SystemTime,
    ) -> Result<SessionSummary, SessionStoreError> {
        let length = file
            .metadata()
            .map_err(|error| SessionStoreError::Io {
                operation: "stat session file".to_owned(),
                message: error.to_string(),
            })?
            .len();
        if length > MAX_SESSION_FILE_BYTES as u64 {
            return Err(SessionStoreError::Invalid("session file exceeds size limit".to_owned()));
        }
        let mut header_text = String::new();
        let header_bytes =
            BufReader::new(&mut file).read_line(&mut header_text).map_err(|error| {
                SessionStoreError::Io {
                    operation: "read session header".to_owned(),
                    message: error.to_string(),
                }
            })?;
        if header_bytes == 0 {
            return Err(SessionStoreError::Invalid("session file is empty".to_owned()));
        }
        if header_bytes > MAX_SESSION_LINE_BYTES {
            return Err(SessionStoreError::Invalid("session line exceeds limit".to_owned()));
        }
        let header = Self::parse_summary_line(header_text.trim_end(), &session_id)?;
        let SessionRecord::Started { workspace_root, model } = header.record else {
            return Err(SessionStoreError::Invalid("session header is missing".to_owned()));
        };

        const TAIL_BYTES: usize = MAX_SESSION_LINE_BYTES * 2 + 2;
        let tail_length = usize::try_from(length.min(TAIL_BYTES as u64)).unwrap_or(TAIL_BYTES);
        file.seek(SeekFrom::End(-(tail_length as i64))).map_err(|error| SessionStoreError::Io {
            operation: "seek session tail".to_owned(),
            message: error.to_string(),
        })?;
        let mut tail = vec![0; tail_length];
        file.read_exact(&mut tail).map_err(|error| SessionStoreError::Io {
            operation: "read session tail".to_owned(),
            message: error.to_string(),
        })?;
        let mut tail_start = 0;
        if length > tail_length as u64 {
            tail_start =
                tail.iter().position(|byte| *byte == b'\n').map_or(tail.len(), |index| index + 1);
        }
        let mut tail_end = tail.len();
        if !tail.ends_with(b"\n") {
            tail_end = tail.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);
        }
        let tail = &tail[tail_start.min(tail_end)..tail_end];
        let tail = std::str::from_utf8(tail)
            .map_err(|error| SessionStoreError::Invalid(error.to_string()))?;
        let lines: Vec<&str> = tail.split_terminator('\n').collect();
        let mut last_turn = None;
        let mut latest_model = None;
        for text in lines.into_iter().rev() {
            if text.trim().is_empty() {
                continue;
            }
            let line = Self::parse_summary_line(text.trim_end(), &session_id)?;
            if let SessionRecord::TurnSnapshot { snapshot }
            | SessionRecord::Checkpoint { snapshot } = line.record
            {
                last_turn = Some(snapshot.turn_id);
                latest_model = snapshot.context.model;
                break;
            }
        }
        Ok(SessionSummary {
            session_id,
            model: latest_model.or(model),
            last_turn,
            workspace: Some(workspace_root),
            modified,
        })
    }

    fn parse_summary_line(
        text: &str,
        session_id: &SessionId,
    ) -> Result<SessionLine, SessionStoreError> {
        if text.len() > MAX_SESSION_LINE_BYTES {
            return Err(SessionStoreError::Invalid("session line exceeds limit".to_owned()));
        }
        let line: SessionLine = serde_json::from_str(text)
            .map_err(|error| SessionStoreError::Invalid(error.to_string()))?;
        if !supported_schema(line.schema_version) {
            return Err(SessionStoreError::Invalid(format!(
                "unsupported session schema {}",
                line.schema_version
            )));
        }
        if &line.session_id != session_id {
            return Err(SessionStoreError::Invalid("session id mismatch".to_owned()));
        }
        Ok(line)
    }

    /// Restore newest valid resumable session, skipping bad candidates.
    pub fn restore_latest(
        workspace: impl AsRef<Path>,
    ) -> Result<(Option<ResumableSession>, Vec<String>), SessionStoreError> {
        let workspace = workspace.as_ref();
        let (summaries, mut issues) = Self::summaries(workspace)?;
        for summary in summaries {
            match Self::restore(workspace, summary.session_id.clone()) {
                Ok(restored) => return Ok((Some(restored), issues)),
                Err(error) => issues.push(format!("{}: {error}", summary.session_id)),
            }
        }
        Ok((None, issues))
    }
    /// Return the resolved OS-state session directory for a workspace.
    pub fn directory_for(workspace: impl AsRef<Path>) -> Result<PathBuf, SessionStoreError> {
        session_fs::session_directory(workspace)
    }

    /// Return the resolved private workspace state directory.
    pub fn workspace_directory(workspace: impl AsRef<Path>) -> Result<PathBuf, SessionStoreError> {
        session_fs::workspace_directory(workspace)
    }

    /// Open or create a session file inside a workspace and recover its next sequence.
    pub fn open(
        workspace: impl AsRef<Path>,
        session_id: SessionId,
    ) -> Result<Self, SessionStoreError> {
        let layout = session_fs::SessionLayout::prepare(workspace.as_ref(), &session_id)?;
        let mut next_sequence = 1;
        if let Some(file) = layout.try_open_existing()? {
            for line in Self::replay_reader(file)? {
                next_sequence = next_sequence.max(line.sequence.saturating_add(1));
            }
        }
        Ok(Self { workspace: layout.workspace, path: layout.path, session_id, next_sequence })
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
        let current_len = fs::symlink_metadata(&self.path)
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if current_len.saturating_add(encoded.len() as u64).saturating_add(1)
            > MAX_SESSION_FILE_BYTES as u64
        {
            self.compact()?;
        }
        let layout = session_fs::SessionLayout::prepare(&self.workspace, &self.session_id)?;
        let mut file = layout.open_jsonl(true)?;
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
        let layout = session_fs::SessionLayout::prepare(&self.workspace, &self.session_id)?;
        let line_count = Self::replay_reader(layout.open_jsonl(false)?)?.len();
        if line_count > MAX_SESSION_LINES {
            self.compact()?;
        }
        Ok(line)
    }

    /// Read bounded records only through this store's contained descriptor.
    pub fn read(&self) -> Result<Vec<SessionLine>, SessionStoreError> {
        let layout = session_fs::SessionLayout::prepare(&self.workspace, &self.session_id)?;
        Self::replay_reader(layout.open_jsonl(false)?)
    }

    fn replay_reader(file: File) -> Result<Vec<SessionLine>, SessionStoreError> {
        let metadata = file.metadata().map_err(|error| SessionStoreError::Io {
            operation: "stat session file".to_owned(),
            message: error.to_string(),
        })?;
        if metadata.len() > MAX_SESSION_FILE_BYTES as u64 {
            return Err(SessionStoreError::Invalid("session file exceeds size limit".to_owned()));
        }
        Self::parse_lines(BufReader::new(file))
    }

    fn parse_lines(mut reader: BufReader<File>) -> Result<Vec<SessionLine>, SessionStoreError> {
        let mut lines = Vec::new();
        let mut buffer = Vec::new();
        loop {
            buffer.clear();
            let read =
                reader.read_until(b'\n', &mut buffer).map_err(|error| SessionStoreError::Io {
                    operation: "read session line".to_owned(),
                    message: error.to_string(),
                })?;
            if read == 0 {
                break;
            }
            if read > MAX_SESSION_LINE_BYTES {
                return Err(SessionStoreError::Invalid("session line exceeds limit".to_owned()));
            }
            let ended_with_newline = buffer.ends_with(b"\n");
            let text = match std::str::from_utf8(&buffer) {
                Ok(text) => text,
                Err(_) if !ended_with_newline => break,
                Err(error) => return Err(SessionStoreError::Invalid(error.to_string())),
            };
            let trimmed = text.trim_end();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionLine>(trimmed) {
                Ok(line) => {
                    if !supported_schema(line.schema_version) {
                        return Err(SessionStoreError::Invalid(format!(
                            "unsupported session schema {}",
                            line.schema_version
                        )));
                    }
                    lines.push(line);
                }
                Err(_) if !ended_with_newline => break,
                Err(error) => return Err(SessionStoreError::Invalid(error.to_string())),
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
        live_workspace: impl AsRef<Path>,
        session_id: SessionId,
    ) -> Result<ResumableSession, SessionStoreError> {
        let layout = session_fs::SessionLayout::existing(live_workspace.as_ref(), &session_id)?;
        let file = layout.open_jsonl(false)?;
        let lines = Self::replay_reader(file)?;
        if lines.is_empty() {
            return Err(SessionStoreError::Invalid("session file is empty".to_owned()));
        }
        let stored_session_id = lines[0].session_id.clone();
        if stored_session_id != session_id
            || lines.iter().any(|line| line.session_id != stored_session_id)
        {
            return Err(SessionStoreError::Invalid("session id mismatch".to_owned()));
        }
        let mut workspace_root = None;
        let mut model = None;
        let mut continuation = None;
        let mut context = None;
        let mut transcript = None;
        for line in &lines {
            match &line.record {
                SessionRecord::Started { workspace_root: stored, model: stored_model } => {
                    workspace_root = Some(stored.clone());
                    model.clone_from(stored_model);
                }
                SessionRecord::TurnSnapshot { snapshot }
                | SessionRecord::Checkpoint { snapshot } => {
                    context = Some(snapshot.context.clone());
                    continuation =
                        snapshot.continuation.as_ref().and_then(persistable_continuation);
                    if !snapshot.transcript.is_empty() {
                        transcript = Some(snapshot.transcript.clone());
                    }
                    if snapshot.context.model.is_some() {
                        model.clone_from(&snapshot.context.model);
                    }
                }
                SessionRecord::Finished => {}
            }
        }
        let live_workspace = layout.workspace.clone();
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
        let context = context.ok_or_else(|| {
            SessionStoreError::Invalid("session has no committed turn".to_owned())
        })?;
        let mut engine = ContextEngine::restore(context);
        engine.set_model(model.clone());
        engine.set_continuation(continuation.clone());
        let stale = engine.refresh_against_workspace(&live_workspace);
        if !stale.is_empty() {
            workspace_changed = true;
            warnings.push(format!("stale file state detected: {}", stale.join(", ")));
        }
        if workspace_changed {
            engine.set_workspace_changed(true);
            engine.set_continuation(None);
            continuation = None;
            warnings
                .push("Rainy continuation invalidated because the workspace changed".to_owned());
        }
        let context = engine.into_snapshot();
        Ok(ResumableSession {
            session_id: stored_session_id,
            workspace_root: live_workspace,
            model: context.model.clone(),
            continuation,
            workspace_changed,
            transcript: transcript.unwrap_or_default(),
            context,
            warnings,
        })
    }

    /// Return the backing path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn compact(&mut self) -> Result<(), SessionStoreError> {
        let layout = session_fs::SessionLayout::prepare(&self.workspace, &self.session_id)?;
        let file = layout.open_jsonl(false)?;
        let lines = Self::replay_reader(file)?;
        let mut started = None;
        let mut turns: Vec<SessionLine> = Vec::new();
        for line in lines {
            match &line.record {
                SessionRecord::Started { .. } => started = Some(line),
                SessionRecord::TurnSnapshot { .. } | SessionRecord::Checkpoint { .. } => {
                    turns.push(line);
                    if turns.len() > MAX_STORED_TURNS {
                        turns.remove(0);
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
        layout.replace_with(body.as_bytes())?;
        self.next_sequence = kept.last().map(|line| line.sequence.saturating_add(1)).unwrap_or(1);
        Ok(())
    }
}

fn supported_schema(version: u32) -> bool {
    (MIN_SUPPORTED_SESSION_SCHEMA_VERSION..=SESSION_SCHEMA_VERSION).contains(&version)
}

fn sanitize_record(record: SessionRecord) -> Result<SessionRecord, SessionStoreError> {
    let record = match record {
        SessionRecord::TurnSnapshot { snapshot } => SessionRecord::turn_snapshot(*snapshot),
        SessionRecord::Checkpoint { snapshot } => SessionRecord::checkpoint(*snapshot),
        other => other,
    };
    Ok(record)
}

/// Inputs for one bounded JSONL turn persist.
pub struct PersistTurn<'a> {
    /// Turn identity.
    pub turn_id: &'a TurnId,
    /// Semantic model steps used.
    pub steps: u16,
    /// Whether the turn ended cancelled.
    pub cancelled: bool,
    /// Working context after the turn.
    pub context: &'a ContextSnapshot,
    /// Sanitized Rainy continuation.
    pub continuation: Option<OpaqueContinuation>,
    /// Safe semantic transcript cells to restore in the TUI.
    pub transcript: &'a [TranscriptCell],
}

/// Persist one completed turn as a single committed JSONL record.
pub fn persist_turn(
    store: &mut SessionStore,
    turn: PersistTurn<'_>,
) -> Result<(), SessionStoreError> {
    store.append(
        Some(turn.turn_id.clone()),
        SessionRecord::turn_snapshot(TurnSnapshot {
            turn_id: turn.turn_id.clone(),
            steps: turn.steps,
            cancelled: turn.cancelled,
            context: turn.context.clone(),
            continuation: turn.continuation,
            transcript: turn.transcript.to_vec(),
        }),
    )?;
    Ok(())
}

/// Inputs for a cancelled or incomplete turn checkpoint.
pub struct PersistCheckpoint<'a> {
    /// Turn identity.
    pub turn_id: &'a TurnId,
    /// Semantic model steps reached before the checkpoint.
    pub steps: u16,
    /// Whether cancellation caused the checkpoint.
    pub cancelled: bool,
    /// Working context after the latest safe observation.
    pub context: &'a ContextSnapshot,
    /// Sanitized provider continuation, if one is still usable.
    pub continuation: Option<OpaqueContinuation>,
    /// Safe semantic transcript cells to restore in the TUI.
    pub transcript: &'a [TranscriptCell],
}

/// Persist resumable work state without presenting it as a completed turn.
pub fn persist_checkpoint(
    store: &mut SessionStore,
    checkpoint: PersistCheckpoint<'_>,
) -> Result<(), SessionStoreError> {
    store.append(
        Some(checkpoint.turn_id.clone()),
        SessionRecord::checkpoint(TurnSnapshot {
            turn_id: checkpoint.turn_id.clone(),
            steps: checkpoint.steps,
            cancelled: checkpoint.cancelled,
            context: checkpoint.context.clone(),
            continuation: checkpoint.continuation,
            transcript: checkpoint.transcript.to_vec(),
        }),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::{
        BoundedText, CompactToolSummary, ContextSnapshot, InspectedFile, LineRange,
        ObservedFileState, SESSION_SCHEMA_VERSION, TranscriptCell, TurnId, WorkflowPhase,
    };

    fn temp_session(label: &str) -> (PathBuf, PathBuf, SessionId) {
        let root = std::env::temp_dir().join(format!(
            "aether-session-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let session = SessionId::new(format!("s-{label}")).unwrap();
        let path = SessionStore::directory_for(&root).unwrap().join(format!("{}.jsonl", session));
        (root, path, session)
    }

    #[test]
    fn workspace_id_is_deterministic_and_workspace_scoped() {
        let (root, _, _) = temp_session("workspace-id-a");
        let (other, _, _) = temp_session("workspace-id-b");
        let canonical = SessionStore::canonical_workspace(&root).unwrap();
        assert_eq!(
            SessionStore::workspace_id(&root).unwrap(),
            SessionStore::workspace_id(&canonical).unwrap()
        );
        assert_ne!(
            SessionStore::workspace_id(&root).unwrap(),
            SessionStore::workspace_id(&other).unwrap()
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(other);
    }

    #[test]
    fn session_creation_stays_outside_workspace() {
        let (root, path, session) = temp_session("state-override");
        let mut store = SessionStore::open(&root, session).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: root.display().to_string(), model: None },
            )
            .unwrap();
        let state_root =
            crate::state::physical_state_root(&crate::state::resolve_state_root().unwrap())
                .unwrap();
        assert!(path.starts_with(state_root));
        assert!(path.is_file());
        assert!(!root.join(".aether").exists());
        assert!(!root.join(".aether-fx").exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn opening_new_session_without_existing_file_succeeds() {
        let (root, path, session) = temp_session("new-empty");
        assert!(!path.exists());
        let mut store = SessionStore::open(&root, session).unwrap();
        assert!(!path.exists());
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: root.display().to_string(), model: None },
            )
            .unwrap();
        assert!(path.is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn workspace_id_hashes_non_utf8_native_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let (root, _, _) = temp_session("workspace-id-native");
        let non_utf8 = root.join(OsString::from_vec(b"non-utf8-\xff".to_vec()));
        if fs::create_dir(&non_utf8).is_err() {
            let _ = fs::remove_dir_all(root);
            return;
        }
        let canonical = fs::canonicalize(&non_utf8).unwrap();
        let expected = blake3::hash(canonical.as_os_str().as_bytes()).to_hex().to_string();
        assert_eq!(SessionStore::workspace_id(&non_utf8).unwrap(), expected);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn workspace_metadata_mismatch_fails_closed() {
        let (root, path, session) = temp_session("metadata-mismatch");
        let _store = SessionStore::open(&root, session).unwrap();
        let metadata_path = path.parent().unwrap().parent().unwrap().join("workspace.json");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
        metadata["workspace"] = serde_json::Value::String("/different/workspace".to_owned());
        fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let error = SessionStore::summaries(&root).unwrap_err().to_string();
        assert!(error.contains("workspace metadata"), "{error}");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn concurrent_workspace_metadata_creation_converges_safely() {
        use std::sync::{Arc, Barrier};

        let root = std::env::temp_dir().join(format!(
            "aether-session-metadata-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let mut workers = Vec::new();
        for index in 0..8 {
            let workspace = root.clone();
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                SessionStore::open(
                    &workspace,
                    SessionId::new(format!("metadata-race-{index}")).unwrap(),
                )
            }));
        }
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
        assert!(SessionStore::summaries(&root).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    fn inspected(path: &str, hash: &str) -> InspectedFile {
        InspectedFile {
            path: path.to_owned(),
            content_hash: Some(hash.to_owned()),
            ranges: vec![LineRange { start: 1, end: 1 }],
            last_state: ObservedFileState::Present,
            stale: false,
        }
    }

    fn write_started_and_turn(
        store: &mut SessionStore,
        workspace: &Path,
        context: ContextSnapshot,
        continuation: Option<OpaqueContinuation>,
    ) {
        store
            .append(
                None,
                SessionRecord::Started {
                    workspace_root: workspace.display().to_string(),
                    model: Some("model-a".to_owned()),
                },
            )
            .unwrap();
        persist_turn(
            store,
            PersistTurn {
                turn_id: &TurnId::new("turn-1").unwrap(),
                steps: 1,
                cancelled: false,
                context: &context,
                continuation,
                transcript: &[],
            },
        )
        .unwrap();
    }

    #[test]
    fn resumed_session_restores_bounded_workflow_state() {
        let (root, _path, session) = temp_session("workflow-resume");
        let source = root.join("src.rs");
        std::fs::write(&source, "fn main() {}\n").unwrap();
        let hash = blake3::hash(&std::fs::read(&source).unwrap()).to_hex().to_string();
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        let mut context =
            ContextSnapshot::new(canonical_root.display().to_string(), Some("model".to_owned()));
        context.inspected.push(inspected("src.rs", &hash));
        context.workflow.record_relevant_file("src.rs");
        context.workflow.record_inspection();
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        write_started_and_turn(&mut store, &root, context, None);

        let restored = SessionStore::restore(&root, session).unwrap();
        assert_eq!(restored.context.workflow.phase, WorkflowPhase::Inspect);
        assert_eq!(restored.context.workflow.relevant_files, vec!["src.rs"]);
        assert_eq!(restored.context.workflow.progress.inspections, 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn incomplete_checkpoint_restores_work_state_without_claiming_completion() {
        let (root, _path, session) = temp_session("checkpoint");
        let mut context =
            ContextSnapshot::new(root.display().to_string(), Some("model".to_owned()));
        context.current_task = aether_core::BoundedText::new(
            "refactor multiple modules and verify",
            aether_core::MAX_TASK_BYTES,
        );
        context.workflow.work.initialize_from_prompt("refactor multiple modules and verify");
        context.workflow.record_relevant_file("src/lib.rs");
        context.workflow.record_inspection();
        let turn_id = TurnId::new("turn-checkpoint").unwrap();
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        store
            .append(
                None,
                SessionRecord::Started {
                    workspace_root: root.display().to_string(),
                    model: Some("model".to_owned()),
                },
            )
            .unwrap();
        persist_checkpoint(
            &mut store,
            PersistCheckpoint {
                turn_id: &turn_id,
                steps: 0,
                cancelled: true,
                context: &context,
                continuation: None,
                transcript: &[],
            },
        )
        .unwrap();
        let restored = SessionStore::restore(&root, session).unwrap();
        assert_eq!(restored.context.workflow.work.mode, aether_core::WorkMode::Structured);
        assert!(restored.context.workflow.work.objective.as_str().contains("refactor"));
        assert_ne!(restored.context.workflow.phase, aether_core::WorkflowPhase::Complete);
        let _ = std::fs::remove_dir_all(root);
    }

    fn padded_started_line(
        session: &SessionId,
        sequence: u64,
        target_len: usize,
        fill: &[u8],
    ) -> (Vec<u8>, usize) {
        let prefix = format!(
            r#"{{"schema_version":3,"sequence":{sequence},"session_id":"{}","turn_id":null,"record":{{"type":"started","workspace_root":"/workspace","model":"model","padding":""#,
            session.as_str()
        )
        .into_bytes();
        let suffix = b"\"}}\n";
        assert!(target_len >= prefix.len() + suffix.len());
        let padding_len = target_len - prefix.len() - suffix.len();
        let mut bytes = prefix.clone();
        while bytes.len() + fill.len() <= prefix.len() + padding_len {
            bytes.extend_from_slice(fill);
        }
        bytes.extend(std::iter::repeat_n(b'a', prefix.len() + padding_len - bytes.len()));
        bytes.extend_from_slice(suffix);
        assert_eq!(bytes.len(), target_len);
        (bytes, prefix.len())
    }

    #[test]
    fn session_write_read_round_trip_persists_continuation() {
        let (root, _path, session) = temp_session("roundtrip");
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        let mut context =
            ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned()));
        context.continuation = Some(OpaqueContinuation(serde_json::json!({
            "previous_response_id": "resp_1",
            "reasoning": "hidden"
        })));
        write_started_and_turn(
            &mut store,
            &root,
            context,
            Some(OpaqueContinuation(serde_json::json!({
                "previous_response_id": "resp_1",
                "reasoning": "hidden"
            }))),
        );
        let lines = SessionStore::open(&root, session.clone()).unwrap().read().unwrap();
        assert_eq!(lines[0].schema_version, SESSION_SCHEMA_VERSION);
        match &lines[1].record {
            SessionRecord::TurnSnapshot { snapshot } => {
                assert_eq!(
                    snapshot.continuation.as_ref().unwrap().0,
                    serde_json::json!({"previous_response_id": "resp_1"})
                );
            }
            other => panic!("unexpected record {other:?}"),
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn latest_transcript_is_restored_with_the_session_context() {
        let (root, _path, session) = temp_session("transcript-resume");
        let context = ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned()));
        let cells = vec![
            TranscriptCell::User { text: BoundedText::new("schedule a design review", 1024) },
            TranscriptCell::Assistant { text: BoundedText::new("I can help with that.", 1024) },
            TranscriptCell::Tool { name: BoundedText::new("read_file", 1024), ok: Some(true) },
        ];
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        store
            .append(
                None,
                SessionRecord::Started {
                    workspace_root: root.display().to_string(),
                    model: Some("model-a".to_owned()),
                },
            )
            .unwrap();
        persist_turn(
            &mut store,
            PersistTurn {
                turn_id: &TurnId::new("turn-transcript").unwrap(),
                steps: 1,
                cancelled: false,
                context: &context,
                continuation: None,
                transcript: &cells,
            },
        )
        .unwrap();
        let restored = SessionStore::restore(&root, session).unwrap();
        assert_eq!(restored.transcript, cells);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn schema_five_snapshot_without_transcript_remains_readable() {
        let (root, path, session) = temp_session("schema-five");
        let _store = SessionStore::open(&root, session.clone()).unwrap();
        let turn_id = TurnId::new("turn-schema-five").unwrap();
        let started = SessionLine::new(
            1,
            session.clone(),
            None,
            SessionRecord::Started {
                workspace_root: root.display().to_string(),
                model: Some("model-a".to_owned()),
            },
        );
        let turn = SessionLine::new(
            2,
            session.clone(),
            Some(turn_id.clone()),
            SessionRecord::turn_snapshot(TurnSnapshot {
                turn_id,
                steps: 1,
                cancelled: false,
                context: ContextSnapshot::new(
                    root.display().to_string(),
                    Some("model-a".to_owned()),
                ),
                continuation: None,
                transcript: Vec::new(),
            }),
        );
        let mut turn_value = serde_json::to_value(turn).unwrap();
        turn_value["schema_version"] = serde_json::json!(5);
        turn_value["record"]["snapshot"].as_object_mut().unwrap().remove("transcript");
        let body = format!(
            "{}\n{}\n",
            serde_json::to_string(&started).unwrap(),
            serde_json::to_string(&turn_value).unwrap()
        );
        fs::write(&path, body).unwrap();
        let restored = SessionStore::restore(&root, session).unwrap();
        assert!(restored.transcript.is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn truncated_final_record_is_ignored_and_previous_turn_remains() {
        let (root, path, session) = temp_session("truncated");
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        let first = ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned()));
        write_started_and_turn(
            &mut store,
            &root,
            first,
            Some(OpaqueContinuation(serde_json::json!({"previous_response_id": "resp_first"}))),
        );
        let committed = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{committed}{{\"truncated\"")).unwrap();
        let restored = SessionStore::restore(&root, session.clone()).unwrap();
        assert_eq!(
            restored.continuation.unwrap().0.get("previous_response_id").unwrap(),
            "resp_first"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn summary_tail_start_inside_utf8_character_finds_latest_turn() {
        const TAIL_BYTES: usize = MAX_SESSION_LINE_BYTES * 2 + 2;

        let (root, path, session) = temp_session("summary-utf8-boundary");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let header = serde_json::to_vec(&SessionLine::new(
            1,
            session.clone(),
            None,
            SessionRecord::Started {
                workspace_root: root.display().to_string(),
                model: Some("model-a".to_owned()),
            },
        ))
        .unwrap();
        let mut header = header;
        header.push(b'\n');
        let turn_id = TurnId::new("turn-utf8-boundary").unwrap();
        let turn = serde_json::to_vec(&SessionLine::new(
            4,
            session.clone(),
            Some(turn_id.clone()),
            SessionRecord::turn_snapshot(TurnSnapshot {
                turn_id: turn_id.clone(),
                steps: 1,
                cancelled: false,
                context: ContextSnapshot::new(
                    root.display().to_string(),
                    Some("model-a".to_owned()),
                ),
                continuation: None,
                transcript: Vec::new(),
            }),
        ))
        .unwrap();
        let mut turn = turn;
        turn.push(b'\n');
        let (boundary, boundary_prefix_len) =
            padded_started_line(&session, 2, MAX_SESSION_LINE_BYTES, "🚀".as_bytes());
        let middle_len = TAIL_BYTES - boundary.len() - turn.len() + boundary_prefix_len + 2;
        assert!(middle_len <= MAX_SESSION_LINE_BYTES);
        let (middle, _) = padded_started_line(&session, 3, middle_len, b"m");

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&header);
        bytes.extend_from_slice(&boundary);
        bytes.extend_from_slice(&middle);
        bytes.extend_from_slice(&turn);
        let tail_offset = bytes.len() - TAIL_BYTES;
        assert_eq!(tail_offset, header.len() + boundary_prefix_len + 2);
        assert!((0x80..=0xbf).contains(&bytes[tail_offset]));
        std::fs::write(&path, bytes).unwrap();

        let (summaries, issues) = SessionStore::summaries(&root).unwrap();
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(summaries[0].session_id, session);
        assert_eq!(summaries[0].last_turn, Some(turn_id));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn summary_ignores_crash_truncated_utf8_after_committed_turn() {
        let (root, path, session) = temp_session("summary-utf8-truncated");
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        write_started_and_turn(
            &mut store,
            &root,
            ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned())),
            None,
        );
        let committed = std::fs::read(&path).unwrap();
        let mut bytes = committed;
        bytes.extend_from_slice(b"{\"partial\":\"");
        bytes.extend_from_slice(&"🚀".as_bytes()[..3]);
        std::fs::write(&path, bytes).unwrap();

        let (summaries, issues) = SessionStore::summaries(&root).unwrap();
        assert!(issues.is_empty(), "{issues:?}");
        assert_eq!(summaries[0].last_turn, Some(TurnId::new("turn-1").unwrap()));
        let (restored, restore_issues) = SessionStore::restore_latest(&root).unwrap();
        assert_eq!(restored.unwrap().session_id, session);
        assert!(restore_issues.is_empty(), "{restore_issues:?}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn session_corruption_handling_skips_truncated_tail_and_rejects_middle() {
        let (first_root, path, session) = temp_session("corrupt");
        let mut store = SessionStore::open(&first_root, session.clone()).unwrap();
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
        let lines = SessionStore::open(&first_root, session.clone()).unwrap().read().unwrap();
        assert_eq!(lines.len(), 1);

        let (root, path, session) = temp_session("corrupt-middle");
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: "/workspace".to_owned(), model: None },
            )
            .unwrap();
        std::fs::write(&path, "{\"schema_version\":3}\nnot-json\n").unwrap();
        assert!(SessionStore::open(&root, session.clone()).is_err());
        let _ = std::fs::remove_dir_all(first_root);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn corrupt_session_compaction_fails_without_overwrite() {
        let (root, path, session) = temp_session("compact-corrupt");
        let mut store = SessionStore::open(&root, session).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: root.display().to_string(), model: None },
            )
            .unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{original}not-json\n")).unwrap();
        let error = store.compact().unwrap_err().to_string();
        assert!(error.contains("session record invalid"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{original}not-json\n"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn schema_version_rejection() {
        let (root, path, session) = temp_session("schema");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            r#"{"schema_version":2,"sequence":1,"session_id":"s-schema","turn_id":null,"record":{"type":"finished"}}
"#,
        )
        .unwrap();
        let error = SessionStore::open(&root, session.clone()).unwrap_err().to_string();
        assert!(error.contains("unsupported session schema 2"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn secret_bearing_and_raw_command_output_are_not_persisted() {
        let (root, path, session) = temp_session("secrets");
        let mut store = SessionStore::open(&root, session).unwrap();
        let mut context = ContextSnapshot::new(root.display().to_string(), None);
        context.current_task = aether_core::BoundedText::new("print RAINY_API_KEY", 1024);
        context.continuation = Some(OpaqueContinuation(serde_json::json!({
            "previous_response_id": "resp_1",
            "RAINY_API_KEY": "must-not-store"
        })));
        context.tool_summaries.push(CompactToolSummary {
            tool: "shell".to_owned(),
            ok: true,
            summary: aether_core::BoundedText::new("export TOKEN=abc\nstdout dump", 1024),
        });
        write_started_and_turn(&mut store, &root, context, None);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(!body.contains("RAINY_API_KEY"));
        assert!(!body.contains("must-not-store"));
        assert!(!body.contains("stdout dump"));
        assert!(!body.contains("print RAINY_API_KEY"));
        assert!(body.contains("\"shell ok\"") || body.contains("shell ok"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn unix_session_dir_and_file_permissions_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let (root, path, session) = temp_session("perms");
        let mut store = SessionStore::open(&root, session).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: root.display().to_string(), model: None },
            )
            .unwrap();
        let sessions = path.parent().unwrap();
        let workspace_state = sessions.parent().unwrap();
        let workspaces = workspace_state.parent().unwrap();
        let state_root = workspaces.parent().unwrap();
        assert_eq!(fs::metadata(state_root).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(workspaces).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(workspace_state).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(sessions).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(
            fs::metadata(workspace_state.join("workspace.json")).unwrap().permissions().mode()
                & 0o777,
            0o600
        );
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unchanged_workspace_preserves_continuation() {
        let (root, _path, session) = temp_session("unchanged");
        std::fs::write(root.join("main.rs"), "old").unwrap();
        let hash = blake3::hash(b"old").to_hex().to_string();
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        let mut context =
            ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned()));
        context.inspected.push(inspected("main.rs", &hash));
        write_started_and_turn(
            &mut store,
            &root,
            context,
            Some(OpaqueContinuation(serde_json::json!({"previous_response_id": "resp_keep"}))),
        );
        let restored = SessionStore::restore(&root, session.clone()).unwrap();
        assert!(!restored.workspace_changed);
        assert_eq!(
            restored.continuation.unwrap().0.get("previous_response_id").unwrap(),
            "resp_keep"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changed_inspected_file_invalidates_continuation() {
        let (root, _path, session) = temp_session("changed-file");
        std::fs::write(root.join("main.rs"), "old").unwrap();
        let hash = blake3::hash(b"old").to_hex().to_string();
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        let mut context =
            ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned()));
        context.inspected.push(inspected("main.rs", &hash));
        write_started_and_turn(
            &mut store,
            &root,
            context,
            Some(OpaqueContinuation(serde_json::json!({"previous_response_id": "resp_stale"}))),
        );
        std::fs::write(root.join("main.rs"), "new").unwrap();
        let restored = SessionStore::restore(&root, session.clone()).unwrap();
        assert!(restored.workspace_changed);
        assert!(restored.context.inspected[0].stale);
        assert!(restored.continuation.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn deleted_inspected_file_invalidates_continuation() {
        let (root, _path, session) = temp_session("deleted-file");
        std::fs::write(root.join("main.rs"), "old").unwrap();
        let hash = blake3::hash(b"old").to_hex().to_string();
        let mut store = SessionStore::open(&root, session.clone()).unwrap();
        let mut context =
            ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned()));
        context.inspected.push(inspected("main.rs", &hash));
        write_started_and_turn(
            &mut store,
            &root,
            context,
            Some(OpaqueContinuation(serde_json::json!({"previous_response_id": "resp_gone"}))),
        );
        std::fs::remove_file(root.join("main.rs")).unwrap();
        let restored = SessionStore::restore(&root, session.clone()).unwrap();
        assert!(restored.workspace_changed);
        assert!(restored.continuation.is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changed_workspace_root_invalidates_continuation() {
        let (live, _path, session) = temp_session("changed-root-live");
        let stored = std::env::temp_dir().join(format!(
            "aether-stored-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&stored).unwrap();
        let mut store = SessionStore::open(&live, session.clone()).unwrap();
        write_started_and_turn(
            &mut store,
            &stored,
            ContextSnapshot::new(stored.display().to_string(), Some("model-a".to_owned())),
            Some(OpaqueContinuation(serde_json::json!({"previous_response_id": "resp_root"}))),
        );
        let restored = SessionStore::restore(&live, session.clone()).unwrap();
        assert!(restored.workspace_changed);
        assert!(restored.continuation.is_none());
        let _ = std::fs::remove_dir_all(live);
        let _ = std::fs::remove_dir_all(stored);
    }

    fn outside_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "aether-outside-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn try_dir_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_dir(target, link).is_ok()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            false
        }
    }

    fn try_file_symlink(target: &Path, link: &Path) -> bool {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(target, link).is_ok()
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file(target, link).is_ok()
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = (target, link);
            false
        }
    }

    fn state_bucket(root: &Path) -> PathBuf {
        SessionStore::state_root()
            .unwrap()
            .join("workspaces")
            .join(SessionStore::workspace_id(root).unwrap())
    }

    #[test]
    fn workspace_bucket_symlink_escape_is_rejected() {
        let (root, _, session) = temp_session("aether-link");
        let outside = outside_dir("aether-link");
        std::fs::write(outside.join("escaped.txt"), "outside").unwrap();
        let bucket = state_bucket(&root);
        std::fs::create_dir_all(bucket.parent().unwrap()).unwrap();
        if !try_dir_symlink(&outside, &bucket) {
            let _ = std::fs::remove_dir_all(root);
            let _ = std::fs::remove_dir_all(outside);
            return;
        }
        let error = SessionStore::open(&root, session).unwrap_err().to_string();
        assert!(
            error.contains("symbolic link")
                || error.contains("reparse")
                || error.contains("session directory")
        );
        assert_eq!(std::fs::read_to_string(outside.join("escaped.txt")).unwrap(), "outside");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn sessions_symlink_escape_is_rejected() {
        let (root, _, session) = temp_session("sessions-link");
        let outside = outside_dir("sessions-link");
        let bucket = state_bucket(&root);
        std::fs::create_dir_all(&bucket).unwrap();
        let sessions = bucket.join("sessions");
        if !try_dir_symlink(&outside, &sessions) {
            let _ = std::fs::remove_dir_all(root);
            let _ = std::fs::remove_dir_all(outside);
            return;
        }
        let error = SessionStore::open(&root, session).unwrap_err().to_string();
        assert!(
            error.contains("symbolic link")
                || error.contains("reparse")
                || error.contains("session directory")
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn session_jsonl_symlink_escape_is_rejected() {
        let (root, path, session) = temp_session("jsonl-link");
        let outside = outside_dir("jsonl-link");
        let target = outside.join("escaped.jsonl");
        std::fs::write(&target, "outside\n").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        if !try_file_symlink(&target, &path) {
            let _ = std::fs::remove_dir_all(root);
            let _ = std::fs::remove_dir_all(outside);
            return;
        }
        let error = SessionStore::open(&root, session).unwrap_err().to_string();
        assert!(
            error.contains("symbolic link")
                || error.contains("reparse")
                || error.contains("session file")
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside\n");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[test]
    fn summaries_reject_state_and_sessions_directory_links() {
        for nested in [false, true] {
            let (root, _, _) = temp_session("summary-dir-link");
            let outside = outside_dir("summary-dir-link");
            let bucket = state_bucket(&root);
            if nested {
                std::fs::create_dir_all(&bucket).unwrap();
            } else {
                std::fs::create_dir_all(bucket.parent().unwrap()).unwrap();
            }
            let link = if nested { bucket.join("sessions") } else { bucket };
            if !try_dir_symlink(&outside, &link) {
                let _ = std::fs::remove_dir_all(root);
                let _ = std::fs::remove_dir_all(outside);
                continue;
            }
            let error = SessionStore::summaries(&root).unwrap_err().to_string();
            assert!(error.contains("symbolic link") || error.contains("reparse"));
            let _ = std::fs::remove_dir_all(root);
            let _ = std::fs::remove_dir_all(outside);
        }
    }

    #[test]
    fn summaries_are_newest_first_and_scale_to_one_hundred() {
        let (root, _, _) = temp_session("summary-hundred");
        let mut expected_newest = None;
        for number in 0..100 {
            let session = SessionId::new(format!("summary-{number:03}")).unwrap();
            let mut store = SessionStore::open(&root, session.clone()).unwrap();
            write_started_and_turn(
                &mut store,
                &root,
                ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned())),
                None,
            );
            expected_newest = Some(session);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let (summaries, issues) = SessionStore::summaries(&root).unwrap();
        assert!(issues.is_empty());
        assert_eq!(summaries.len(), 100);
        assert_eq!(summaries[0].session_id, expected_newest.unwrap());
        assert!(summaries.windows(2).all(|pair| pair[0].modified >= pair[1].modified));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn restore_latest_skips_corrupt_empty_and_unsupported_candidates() {
        for invalid in ["not-json\n", "", "{\"schema_version\":2}\n"] {
            let (root, _, _) = temp_session("latest-skip");
            let older = SessionId::new("older-valid").unwrap();
            let mut store = SessionStore::open(&root, older.clone()).unwrap();
            write_started_and_turn(
                &mut store,
                &root,
                ContextSnapshot::new(root.display().to_string(), Some("model-a".to_owned())),
                None,
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
            let newest = SessionId::new("newest-invalid").unwrap();
            let path =
                SessionStore::directory_for(&root).unwrap().join(format!("{}.jsonl", newest));
            std::fs::write(path, invalid).unwrap();

            let (restored, issues) = SessionStore::restore_latest(&root).unwrap();
            assert_eq!(restored.unwrap().session_id, older);
            assert!(issues.iter().any(|issue| issue.contains("newest-invalid")));
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn compaction_temp_symlink_does_not_escape() {
        let (root, path, session) = temp_session("compact-temp");
        let outside = outside_dir("compact-temp");
        let target = outside.join("escaped.tmp");
        std::fs::write(&target, "outside").unwrap();
        let mut store = SessionStore::open(&root, session).unwrap();
        store
            .append(
                None,
                SessionRecord::Started { workspace_root: root.display().to_string(), model: None },
            )
            .unwrap();
        let temp = path.parent().unwrap().join(format!(
            "{SESSION_TEMP_PREFIX}-{}-{}.tmp",
            std::process::id(),
            path.file_name().unwrap().to_string_lossy()
        ));
        if !try_file_symlink(&target, &temp) {
            let _ = std::fs::remove_dir_all(root);
            let _ = std::fs::remove_dir_all(outside);
            return;
        }
        let original = std::fs::read_to_string(&path).unwrap();
        assert!(store.compact().is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside");
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }
}
