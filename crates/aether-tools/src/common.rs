use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use aether_core::{
    CoreError, DEFAULT_MAX_OUTPUT_BYTES, PermissionClass, PermissionEngine, PermissionRequest,
    ToolCallId, ToolResult, WorkspaceRoot,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

pub(crate) const MAX_WALK_ENTRIES: usize = 100_000;
pub(crate) const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_PROCESS_READ_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum ToolInternalError {
    #[error("{0}")]
    Core(#[from] CoreError),
    #[error("I/O operation failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid input: {0}")]
    Input(String),
    #[error("invalid pattern: {0}")]
    Pattern(String),
    #[error("operation timed out")]
    Timeout,
}

pub(crate) type ToolResultInternal<T> = Result<T, ToolInternalError>;

pub(crate) struct ProcessEntry {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
}

#[derive(Clone)]
pub struct Workspace {
    root: WorkspaceRoot,
    canonical_root: Arc<PathBuf>,
    permissions: PermissionEngine,
    max_output_bytes: usize,
    processes: Arc<Mutex<HashMap<u64, ProcessEntry>>>,
    next_process_id: Arc<AtomicU64>,
}

impl Workspace {
    pub fn new(path: impl AsRef<Path>) -> ToolResultInternal<Self> {
        let canonical_root = fs::canonicalize(path.as_ref())?;
        if !canonical_root.is_dir() {
            return Err(ToolInternalError::Input("workspace root must be a directory".to_owned()));
        }
        Ok(Self {
            root: WorkspaceRoot::new(canonical_root.clone())?,
            canonical_root: Arc::new(canonical_root),
            permissions: PermissionEngine::new(Default::default()),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
            processes: Arc::new(Mutex::new(HashMap::new())),
            next_process_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn with_policy(mut self, permissions: PermissionEngine) -> Self {
        self.permissions = permissions;
        self
    }

    pub fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes.max(1);
        self
    }

    pub fn root(&self) -> &Path {
        self.canonical_root.as_path()
    }

    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    pub(crate) fn authorize(
        &self,
        class: PermissionClass,
        operation: &str,
        target: Option<String>,
        details: Value,
    ) -> ToolResultInternal<()> {
        self.permissions.authorize(&PermissionRequest {
            class,
            operation: operation.to_owned(),
            target,
            details,
        })?;
        Ok(())
    }

    pub(crate) fn resolve_existing(&self, path: &str) -> ToolResultInternal<(String, PathBuf)> {
        let (relative, lexical) = self.root.resolve(path)?;
        let canonical = fs::canonicalize(&lexical)?;
        self.ensure_contained(&canonical, path)?;
        Ok((relative.display(), canonical))
    }

    pub(crate) fn resolve_for_write(&self, path: &str) -> ToolResultInternal<(String, PathBuf)> {
        let (relative, lexical) = self.root.resolve(path)?;
        if lexical.exists() {
            let canonical = fs::canonicalize(&lexical)?;
            self.ensure_contained(&canonical, path)?;
        } else {
            let parent = lexical.parent().ok_or_else(|| {
                ToolInternalError::Input("write path has no parent directory".to_owned())
            })?;
            let canonical_parent = fs::canonicalize(parent)?;
            self.ensure_contained(&canonical_parent, path)?;
        }
        Ok((relative.display(), lexical))
    }

    pub(crate) fn verify_entry(&self, path: &Path, display: &str) -> ToolResultInternal<()> {
        let canonical = fs::canonicalize(path)?;
        self.ensure_contained(&canonical, display)
    }

    fn ensure_contained(&self, candidate: &Path, display: &str) -> ToolResultInternal<()> {
        if candidate.starts_with(self.canonical_root.as_path()) {
            Ok(())
        } else {
            Err(ToolInternalError::Core(CoreError::PathEscape { path: display.to_owned() }))
        }
    }

    pub(crate) fn parse<T: DeserializeOwned>(&self, input: &Value) -> ToolResultInternal<T> {
        serde_json::from_value(input.clone())
            .map_err(|error| ToolInternalError::Input(error.to_string()))
    }

    pub(crate) fn allocate_process_id(&self) -> u64 {
        self.next_process_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn processes(&self) -> &Arc<Mutex<HashMap<u64, ProcessEntry>>> {
        &self.processes
    }

    pub(crate) fn result_error(&self, call_id: ToolCallId, error: ToolInternalError) -> ToolResult {
        let (code, retryable) = match &error {
            ToolInternalError::Core(CoreError::PermissionRequired { .. }) => {
                ("permission_required", false)
            }
            ToolInternalError::Core(CoreError::PathEscape { .. }) => ("path_escape", false),
            ToolInternalError::Core(CoreError::Cancelled) => ("cancelled", true),
            ToolInternalError::Core(CoreError::Unsupported { .. }) => ("unsupported", false),
            ToolInternalError::Core(CoreError::FeatureUnavailable { .. }) => {
                ("feature_unavailable", false)
            }
            ToolInternalError::Timeout => ("timeout", true),
            ToolInternalError::Input(_) | ToolInternalError::Pattern(_) => ("invalid_input", false),
            ToolInternalError::Io(_) => ("io", false),
            ToolInternalError::Core(_) => ("operation_failed", false),
        };
        ToolResult::failure(call_id, code, error.to_string(), retryable, self.max_output_bytes)
    }
}

pub(crate) fn bounded_limit(requested: Option<usize>, workspace_limit: usize) -> usize {
    requested.unwrap_or(workspace_limit).clamp(1, workspace_limit.min(MAX_INPUT_BYTES))
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub(crate) fn hash_file(path: &Path) -> ToolResultInternal<String> {
    let bytes = fs::read(path)?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ToolInternalError::Input(
            "precondition file exceeds the bounded input limit".to_owned(),
        ));
    }
    Ok(hash_bytes(&bytes))
}

pub(crate) fn temporary_path(path: &Path) -> PathBuf {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    path.with_file_name(format!(".aether-{file_name}-{id}.tmp"))
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> ToolResultInternal<()> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ToolInternalError::Input("write exceeds the bounded input limit".to_owned()));
    }
    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = fs::OpenOptions::new().create_new(true).write(true).open(&temporary)?;
        use std::io::Write;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        #[cfg(windows)]
        {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
