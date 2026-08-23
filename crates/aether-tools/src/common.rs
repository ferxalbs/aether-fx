use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, RwLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use aether_core::{
    CancellationFlag, CoreError, DEFAULT_MAX_OUTPUT_BYTES, ExecutionPermit, PermissionClass,
    PermissionEngine, PermissionRequest, ToolCallId, ToolExecutionContext, ToolResult,
    WorkspaceRoot,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tokio::{
    process::{Child, ChildStdin},
    sync::{Mutex, Notify, OwnedMutexGuard},
    task::JoinHandle,
};

use crate::platform;

pub(crate) const MAX_WALK_ENTRIES: usize = 100_000;
pub(crate) const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_PROCESS_READ_BYTES: usize = 64 * 1024;
pub(crate) const MAX_PERSISTENT_PROCESSES: usize = 16;
pub(crate) const PROCESS_STREAM_BUFFER_BYTES: usize = 256 * 1024;

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
    #[error("destination already exists: {path}")]
    DestinationExists { path: String },
    #[error("concurrent modification detected: {path}")]
    ConcurrentModification { path: String },
    #[error("process {process_id} termination failed: {error}")]
    ProcessTermination { process_id: u64, error: String },
    #[error("HIGH-SEVERITY rollback incomplete after commit error: {commit}; results: {results:?}")]
    RollbackFailed { commit: String, results: Vec<String> },
}

pub(crate) type ToolResultInternal<T> = Result<T, ToolInternalError>;

/// A bounded byte buffer owned by one persistent-process stream.
#[derive(Debug)]
pub(crate) struct OutputBuffer {
    state: StdMutex<OutputBufferState>,
    notify: Notify,
    capacity: usize,
}

#[derive(Debug)]
struct OutputBufferState {
    bytes: std::collections::VecDeque<u8>,
    dropped_bytes: u64,
    eof: bool,
}

#[derive(Debug)]
pub(crate) struct BufferedRead {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub dropped_bytes: u64,
    pub buffered_bytes: usize,
}

impl OutputBuffer {
    pub(crate) fn new(capacity: usize) -> Self {
        Self {
            state: StdMutex::new(OutputBufferState {
                bytes: std::collections::VecDeque::with_capacity(capacity),
                dropped_bytes: 0,
                eof: false,
            }),
            notify: Notify::new(),
            capacity: capacity.max(1),
        }
    }

    pub(crate) fn push(&self, bytes: &[u8]) {
        if let Ok(mut state) = self.state.lock() {
            if bytes.len() >= self.capacity {
                let retained = &bytes[bytes.len() - self.capacity..];
                let dropped =
                    state.bytes.len().saturating_add(bytes.len()).saturating_sub(retained.len());
                state.bytes.clear();
                state.bytes.extend(retained.iter().copied());
                state.dropped_bytes = state.dropped_bytes.saturating_add(dropped as u64);
            } else {
                let overflow =
                    state.bytes.len().saturating_add(bytes.len()).saturating_sub(self.capacity);
                for _ in 0..overflow {
                    let _ = state.bytes.pop_front();
                }
                state.dropped_bytes = state.dropped_bytes.saturating_add(overflow as u64);
                state.bytes.extend(bytes.iter().copied());
            }
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn mark_eof(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.eof = true;
        }
        self.notify.notify_waiters();
    }

    pub(crate) fn take(&self, limit: usize) -> BufferedRead {
        let Ok(mut state) = self.state.lock() else {
            return BufferedRead {
                bytes: Vec::new(),
                eof: false,
                dropped_bytes: 0,
                buffered_bytes: 0,
            };
        };
        let count = limit.min(state.bytes.len());
        let bytes = state.bytes.drain(..count).collect();
        BufferedRead {
            bytes,
            eof: state.eof && state.bytes.is_empty(),
            dropped_bytes: state.dropped_bytes,
            buffered_bytes: state.bytes.len(),
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.state.lock().map(|state| !state.bytes.is_empty() || state.eof).unwrap_or(true)
    }

    pub(crate) fn notifier(&self) -> &Notify {
        &self.notify
    }
}

/// Independent process state. No registry lock is held while any field is awaited.
#[derive(Debug)]
pub(crate) struct ProcessHandle {
    pub child: Mutex<Child>,
    pub stdin: Mutex<Option<ChildStdin>>,
    pub stdout: Arc<OutputBuffer>,
    pub stderr: Arc<OutputBuffer>,
    pub drains: Mutex<Vec<JoinHandle<()>>>,
    pub state: StdMutex<ProcessState>,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    #[cfg(test)]
    pub test_termination: StdMutex<Option<TestTerminationMode>>,
}

impl ProcessHandle {
    pub(crate) async fn terminate(&self) -> Result<ProcessTermination, ProcessTerminationError> {
        crate::process::terminate_process(self).await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessTermination {
    AlreadyExited { exit_code: Option<i32> },
    Terminated { exit_code: Option<i32> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProcessState {
    Running,
    Exited,
    TerminationRequested,
    TerminationFailed,
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TestTerminationMode {
    Fail,
    Timeout,
}

#[derive(Debug, Error)]
pub(crate) enum ProcessTerminationError {
    #[error("kill request failed: {0}")]
    Kill(#[source] io::Error),
    #[error("wait failed: {0}")]
    Wait(#[source] io::Error),
    #[error("termination wait timed out after {timeout_ms} ms")]
    Timeout { timeout_ms: u64 },
    #[cfg(test)]
    #[error("test termination failure: {0}")]
    Test(String),
}

/// Short-lived registry lock around process handles, never around process I/O.
#[derive(Debug)]
pub(crate) struct ProcessRegistry {
    entries: RwLock<HashMap<u64, Arc<ProcessHandle>>>,
    next_id: AtomicU64,
}

impl Default for ProcessRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessRegistry {
    pub(crate) fn new() -> Self {
        Self { entries: RwLock::new(HashMap::new()), next_id: AtomicU64::new(1) }
    }

    pub(crate) fn allocate_id(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    pub(crate) fn insert(
        &self,
        process_id: u64,
        handle: Arc<ProcessHandle>,
    ) -> ToolResultInternal<()> {
        let Ok(mut entries) = self.entries.write() else {
            return Err(ToolInternalError::Input("process registry lock poisoned".to_owned()));
        };
        if entries.len() >= MAX_PERSISTENT_PROCESSES {
            return Err(ToolInternalError::Core(CoreError::ResourceLimit {
                resource: "persistent processes".to_owned(),
                limit: MAX_PERSISTENT_PROCESSES,
            }));
        }
        entries.insert(process_id, handle);
        Ok(())
    }

    pub(crate) fn get(&self, process_id: u64) -> Option<Arc<ProcessHandle>> {
        self.entries.read().ok()?.get(&process_id).cloned()
    }

    pub(crate) fn remove(&self, process_id: u64) -> Option<Arc<ProcessHandle>> {
        self.entries.write().ok()?.remove(&process_id)
    }

    pub(crate) fn snapshot(&self) -> Vec<(u64, Arc<ProcessHandle>)> {
        self.entries
            .read()
            .map(|entries| entries.iter().map(|(id, handle)| (*id, handle.clone())).collect())
            .unwrap_or_default()
    }
}

/// Per-destination mutation locks shared by write and patch.
#[derive(Debug, Default)]
pub(crate) struct PathMutationCoordinator {
    locks: StdMutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
}

impl PathMutationCoordinator {
    pub(crate) fn acquire(
        self: &Arc<Self>,
        keys: impl IntoIterator<Item = PathBuf>,
    ) -> ToolResultInternal<PathMutationGuard> {
        let mut keys = keys.into_iter().collect::<Vec<_>>();
        keys.sort();
        keys.dedup();

        let locks =
            keys.iter().map(|key| self.lock_for(key)).collect::<ToolResultInternal<Vec<_>>>()?;
        let guards = locks.iter().map(|lock| lock.clone().blocking_lock_owned()).collect();

        Ok(PathMutationGuard { coordinator: Arc::clone(self), keys, locks, guards })
    }

    fn lock_for(&self, key: &Path) -> ToolResultInternal<Arc<Mutex<()>>> {
        let mut locks = self
            .locks
            .lock()
            .map_err(|_| ToolInternalError::Input("path mutation lock poisoned".to_owned()))?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(key).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(Mutex::new(()));
        locks.insert(key.to_owned(), Arc::downgrade(&lock));
        Ok(lock)
    }
}

pub(crate) struct PathMutationGuard {
    coordinator: Arc<PathMutationCoordinator>,
    keys: Vec<PathBuf>,
    locks: Vec<Arc<Mutex<()>>>,
    guards: Vec<OwnedMutexGuard<()>>,
}

impl Drop for PathMutationGuard {
    fn drop(&mut self) {
        let guards = std::mem::take(&mut self.guards);
        drop(guards);

        if let Ok(mut locks) = self.coordinator.locks.lock() {
            for (key, lock) in self.keys.iter().zip(&self.locks) {
                let same_lock = locks
                    .get(key)
                    .and_then(Weak::upgrade)
                    .is_some_and(|current| Arc::ptr_eq(&current, lock));
                if same_lock && Arc::strong_count(lock) == 1 {
                    locks.remove(key);
                }
            }
        }
        self.locks.clear();
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum FileState {
    Missing,
    Present { length: u64, modified: Option<SystemTime>, symlink: bool },
}

pub(crate) fn file_state(path: &Path) -> ToolResultInternal<FileState> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(FileState::Present {
            length: metadata.len(),
            modified: metadata.modified().ok(),
            symlink: metadata.file_type().is_symlink(),
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(FileState::Missing),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone)]
pub struct Workspace {
    root: WorkspaceRoot,
    canonical_root: Arc<PathBuf>,
    permissions: PermissionEngine,
    max_output_bytes: usize,
    processes: Arc<ProcessRegistry>,
    mutations: Arc<PathMutationCoordinator>,
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
            processes: Arc::new(ProcessRegistry::new()),
            mutations: Arc::new(PathMutationCoordinator::default()),
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

    pub(crate) fn direct_context(
        &self,
        request: &PermissionRequest,
    ) -> ToolResultInternal<ToolExecutionContext> {
        self.permissions.authorize(request)?;
        Ok(ToolExecutionContext::new(
            CancellationFlag::new(),
            ExecutionPermit::new(request.call_id.clone(), request.tool.clone(), request.class),
        ))
    }

    pub(crate) fn require_permit(
        &self,
        context: &ToolExecutionContext,
        call_id: &ToolCallId,
        tool: &str,
        class: PermissionClass,
    ) -> ToolResultInternal<()> {
        context.cancellation().check()?;
        context.permit().validate(call_id, tool, class)?;
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

    pub(crate) fn mutation_key(&self, path: &Path) -> ToolResultInternal<PathBuf> {
        let key = match fs::canonicalize(path) {
            Ok(canonical) => canonical,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let parent = path.parent().ok_or_else(|| {
                    ToolInternalError::Input("write path has no parent directory".to_owned())
                })?;
                let file_name = path.file_name().ok_or_else(|| {
                    ToolInternalError::Input("write path has no file name".to_owned())
                })?;
                fs::canonicalize(parent)?.join(file_name)
            }
            Err(error) => return Err(error.into()),
        };
        self.ensure_contained(&key, &path.display().to_string())?;
        Ok(key)
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
        self.processes.allocate_id()
    }

    pub(crate) fn processes(&self) -> &Arc<ProcessRegistry> {
        &self.processes
    }

    pub(crate) fn acquire_mutations(
        &self,
        keys: impl IntoIterator<Item = PathBuf>,
    ) -> ToolResultInternal<PathMutationGuard> {
        self.mutations.acquire(keys)
    }

    /// Terminate all persistent processes owned by this workspace/session.
    pub async fn shutdown_processes(&self) -> crate::process::ProcessShutdownReport {
        let mut report = crate::process::ProcessShutdownReport::default();
        for (process_id, handle) in self.processes.snapshot() {
            report.attempted += 1;
            match handle.terminate().await {
                Ok(_) => {
                    self.processes.remove(process_id);
                    report.terminated += 1;
                }
                Err(error) => report.failures.push(crate::process::ProcessShutdownFailure {
                    process_id,
                    error: error.to_string(),
                }),
            }
        }
        report
    }

    pub(crate) fn result_error(&self, call_id: ToolCallId, error: ToolInternalError) -> ToolResult {
        let (code, retryable) = match &error {
            ToolInternalError::Core(CoreError::PermissionRequired { .. }) => {
                ("permission_required", false)
            }
            ToolInternalError::Core(CoreError::PermissionDenied { .. }) => {
                ("permission_denied", false)
            }
            ToolInternalError::Core(CoreError::PathEscape { .. }) => ("path_escape", false),
            ToolInternalError::Core(CoreError::Cancelled) => ("cancelled", true),
            ToolInternalError::Core(CoreError::ResourceLimit { .. }) => ("resource_limit", false),
            ToolInternalError::Core(CoreError::Unsupported { .. }) => ("unsupported", false),
            ToolInternalError::Core(CoreError::FeatureUnavailable { .. }) => {
                ("feature_unavailable", false)
            }
            ToolInternalError::DestinationExists { .. } => ("already_exists", false),
            ToolInternalError::ConcurrentModification { .. } => ("concurrent_modification", true),
            ToolInternalError::ProcessTermination { .. } => ("termination_failed", true),
            ToolInternalError::RollbackFailed { .. } => ("rollback_failed", false),
            ToolInternalError::Timeout => ("timeout", true),
            ToolInternalError::Input(_) | ToolInternalError::Pattern(_) => ("invalid_input", false),
            ToolInternalError::Io(_) => ("io", false),
            ToolInternalError::Core(_) => ("operation_failed", false),
        };
        ToolResult::failure(call_id, code, error.to_string(), retryable, self.max_output_bytes)
    }
}

/// Run one bounded blocking filesystem/CPU operation without occupying the Tokio control task.
pub(crate) async fn spawn_blocking_tool<F>(
    workspace: &Workspace,
    call_id: ToolCallId,
    context: &ToolExecutionContext,
    operation: F,
) -> ToolResult
where
    F: FnOnce(Workspace, ToolCallId, CancellationFlag) -> ToolResult + Send + 'static,
{
    let task_workspace = workspace.clone();
    let task_call_id = call_id.clone();
    let cancellation = context.cancellation().clone();
    let max_output_bytes = workspace.max_output_bytes();
    match tokio::task::spawn_blocking(move || operation(task_workspace, task_call_id, cancellation))
        .await
    {
        Ok(result) => result,
        Err(error) => ToolResult::failure(
            call_id,
            "operation_failed",
            format!("blocking tool task failed: {error}"),
            false,
            max_output_bytes,
        ),
    }
}

/// Canonicalize one user-supplied path away from the current-thread control task.
pub(crate) async fn resolve_existing_blocking(
    workspace: &Workspace,
    path: String,
) -> ToolResultInternal<(String, PathBuf)> {
    let workspace = workspace.clone();
    tokio::task::spawn_blocking(move || workspace.resolve_existing(&path))
        .await
        .map_err(|error| ToolInternalError::Input(format!("path worker failed: {error}")))?
}

pub(crate) fn bounded_limit(requested: Option<usize>, workspace_limit: usize) -> usize {
    requested.unwrap_or(workspace_limit).clamp(1, workspace_limit.min(MAX_INPUT_BYTES))
}

pub(crate) fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

pub(crate) fn hash_file(
    path: &Path,
    cancellation: &CancellationFlag,
) -> ToolResultInternal<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 32 * 1024];
    let mut total = 0usize;
    loop {
        cancellation.check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total.saturating_add(count);
        if total > MAX_INPUT_BYTES {
            return Err(ToolInternalError::Input(
                "precondition file exceeds the bounded input limit".to_owned(),
            ));
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

pub(crate) fn temporary_path(path: &Path) -> PathBuf {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
    let id = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file");
    path.with_file_name(format!(".aether-{file_name}-{pid}-{id}.tmp"))
}

pub(crate) struct StagedReplacement {
    pub destination: PathBuf,
    pub temporary: PathBuf,
}

impl StagedReplacement {
    pub(crate) fn cleanup(&self) {
        let _ = fs::remove_file(&self.temporary);
    }
}

pub(crate) fn stage_replacement(
    path: &Path,
    bytes: &[u8],
    cancellation: &CancellationFlag,
) -> ToolResultInternal<StagedReplacement> {
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ToolInternalError::Input("write exceeds the bounded input limit".to_owned()));
    }
    cancellation.check()?;
    let permissions = match fs::metadata(path) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let mut temporary = None;
    let mut file = None;
    for _ in 0..32 {
        let candidate = temporary_path(path);
        match fs::OpenOptions::new().create_new(true).write(true).open(&candidate) {
            Ok(opened) => {
                temporary = Some(candidate);
                file = Some(opened);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let Some(temporary) = temporary else {
        return Err(ToolInternalError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a collision-free temporary file",
        )));
    };
    let result = (|| {
        let mut file = file.expect("temporary file exists when entering staging");
        for chunk in bytes.chunks(64 * 1024) {
            cancellation.check()?;
            file.write_all(chunk)?;
        }
        file.flush()?;
        if let Some(permissions) = permissions {
            fs::set_permissions(&temporary, permissions)?;
        }
        file.sync_all()?;
        drop(file);
        cancellation.check()?;
        Ok(StagedReplacement { destination: path.to_owned(), temporary: temporary.clone() })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn install_replacement(
    staged: &StagedReplacement,
    initial_state: &FileState,
    create_only: bool,
    cancellation: &CancellationFlag,
) -> ToolResultInternal<()> {
    // The state check belongs immediately before this function. The OS call below is still
    // atomic, but an arbitrary external process can mutate the namespace after that check.
    cancellation.check()?;
    if create_only || matches!(initial_state, FileState::Missing) {
        return install_without_replacement(staged, create_only, cancellation);
    }

    #[cfg(windows)]
    {
        platform::replace_existing(&staged.destination, &staged.temporary)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(&staged.temporary, &staged.destination)?;
        Ok(())
    }
}

fn install_without_replacement(
    staged: &StagedReplacement,
    create_only: bool,
    cancellation: &CancellationFlag,
) -> ToolResultInternal<()> {
    cancellation.check()?;
    match platform::install_exclusive(&staged.destination, &staged.temporary) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            no_replace_conflict(create_only, &staged.destination)
        }
        Err(error) if should_fallback_to_hard_link(&error) => {
            match fs::hard_link(&staged.temporary, &staged.destination) {
                Ok(()) => {
                    // The hard link is the commit point. Removing the same-directory staging
                    // name does not alter the installed destination; if cleanup is unavailable,
                    // leave the bounded temporary artifact rather than reporting a false failed
                    // commit.
                    let _ = fs::remove_file(&staged.temporary);
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    no_replace_conflict(create_only, &staged.destination)
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

fn no_replace_conflict(create_only: bool, destination: &Path) -> ToolResultInternal<()> {
    if create_only {
        Err(ToolInternalError::DestinationExists { path: destination.display().to_string() })
    } else {
        Err(ToolInternalError::ConcurrentModification { path: destination.display().to_string() })
    }
}

fn should_fallback_to_hard_link(error: &io::Error) -> bool {
    match error.kind() {
        io::ErrorKind::Unsupported | io::ErrorKind::CrossesDevices => true,
        _ => error.raw_os_error().is_some_and(|code| {
            // EXDEV, ENOSYS, ENOTSUP/EOPNOTSUPP. Exact values vary by Unix; keep this fallback
            // narrow so a real AlreadyExists never degrades into replacement.
            matches!(code, 18 | 38 | 45 | 78 | 95)
        }),
    }
}

pub(crate) fn atomic_write(
    path: &Path,
    bytes: &[u8],
    cancellation: &CancellationFlag,
) -> ToolResultInternal<()> {
    let initial_state = file_state(path)?;
    let staged = stage_replacement(path, bytes, cancellation)?;
    let result = install_replacement(&staged, &initial_state, false, cancellation);
    if result.is_err() {
        staged.cleanup();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Barrier, mpsc},
        time::Duration,
    };

    fn temp_root(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "aether-common-{label}-{}-{}",
            std::process::id(),
            temporary_path(Path::new("fixture")).file_name().unwrap().to_string_lossy()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn output_buffer_preserves_recent_bytes_and_counts_every_drop() {
        let buffer = OutputBuffer::new(4);
        buffer.push(b"abc");
        buffer.push(b"def");
        let first = buffer.take(64);
        assert_eq!(first.bytes, b"cdef");
        assert_eq!(first.dropped_bytes, 2);
        assert_eq!(first.buffered_bytes, 0);

        buffer.push(b"012345");
        let second = buffer.take(64);
        assert_eq!(second.bytes, b"2345");
        assert_eq!(second.dropped_bytes, 4);
    }

    #[test]
    fn exclusive_install_creates_missing_destination() {
        let root = temp_root("exclusive-create");
        let path = root.join("new.txt");
        let cancellation = CancellationFlag::new();
        let staged = stage_replacement(&path, b"created", &cancellation).unwrap();
        install_replacement(&staged, &FileState::Missing, true, &cancellation).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"created");
        assert!(!staged.temporary.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn no_replace_commit_refuses_destination_that_appears_before_commit() {
        let root = temp_root("create-only");
        let path = root.join("file.txt");
        let cancellation = CancellationFlag::new();
        let staged = stage_replacement(&path, b"replacement", &cancellation).unwrap();
        fs::write(&path, b"external").unwrap();

        let result = install_replacement(&staged, &FileState::Missing, true, &cancellation);
        assert!(matches!(result, Err(ToolInternalError::DestinationExists { .. })));
        assert_eq!(fs::read(&path).unwrap(), b"external");
        staged.cleanup();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn path_mutations_serialize_same_destination_and_clean_dead_entries() {
        let coordinator = Arc::new(PathMutationCoordinator::default());
        let path = PathBuf::from("/workspace/file");
        let first = coordinator.acquire([path.clone()]).unwrap();
        let (sender, receiver) = mpsc::channel();
        let other = Arc::clone(&coordinator);
        let worker = std::thread::spawn(move || {
            let guard = other.acquire([path]).unwrap();
            sender.send(()).unwrap();
            drop(guard);
        });
        assert!(receiver.recv_timeout(Duration::from_millis(25)).is_err());
        drop(first);
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
        assert!(coordinator.locks.lock().unwrap().is_empty());
    }

    #[test]
    fn unrelated_destinations_do_not_share_one_filesystem_lock() {
        let coordinator = Arc::new(PathMutationCoordinator::default());
        let first = coordinator.acquire([PathBuf::from("/workspace/a")]).unwrap();
        let (sender, receiver) = mpsc::channel();
        let other = Arc::clone(&coordinator);
        let worker = std::thread::spawn(move || {
            let guard = other.acquire([PathBuf::from("/workspace/b")]).unwrap();
            sender.send(()).unwrap();
            drop(guard);
        });
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        drop(first);
        worker.join().unwrap();
    }

    #[test]
    fn multi_path_mutations_use_deterministic_lock_order() {
        let coordinator = Arc::new(PathMutationCoordinator::default());
        let barrier = Arc::new(Barrier::new(3));
        let (sender, receiver) = mpsc::channel();
        let mut workers = Vec::new();
        for keys in [
            vec![PathBuf::from("/workspace/a"), PathBuf::from("/workspace/b")],
            vec![PathBuf::from("/workspace/b"), PathBuf::from("/workspace/a")],
        ] {
            let coordinator = Arc::clone(&coordinator);
            let barrier = Arc::clone(&barrier);
            let sender = sender.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                let guard = coordinator.acquire(keys).unwrap();
                sender.send(()).unwrap();
                drop(guard);
            }));
        }
        barrier.wait();
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        for worker in workers {
            worker.join().unwrap();
        }
    }
}
