use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use aether_core::SessionId;
use serde::{Deserialize, Serialize};

use crate::session::{SESSION_TEMP_PREFIX, SessionStoreError};
use crate::state;

pub(crate) struct SessionEntry {
    pub session_id: SessionId,
    pub file: File,
    pub modified: std::time::SystemTime,
}

pub(crate) struct WorkspaceLayout {
    pub workspace: PathBuf,
    pub state_root: PathBuf,
    #[cfg(unix)]
    pub workspace_id: String,
    pub workspace_dir: PathBuf,
    pub sessions_dir: PathBuf,
}

impl WorkspaceLayout {
    pub(crate) fn resolve(
        workspace: impl AsRef<Path>,
        create: bool,
    ) -> Result<Option<Self>, SessionStoreError> {
        let workspace = state::canonical_workspace(workspace.as_ref())?;
        let state_root = state::validated_state_root(&workspace)?;
        let workspace_id = state::workspace_id(&workspace);
        let workspace_dir = state_root.join(state::WORKSPACES_DIR).join(&workspace_id);
        let sessions_dir = workspace_dir.join(state::SESSION_DIR);

        if create {
            prepare_directories(&state_root, &workspace_id)?;
        } else if !state_directories_exist(&state_root, &workspace_id)? {
            return Ok(None);
        }

        let layout = Self {
            workspace,
            state_root,
            #[cfg(unix)]
            workspace_id,
            workspace_dir,
            sessions_dir,
        };
        ensure_workspace_metadata(&layout)?;
        Ok(Some(layout))
    }

    pub(crate) fn session(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionLayout, SessionStoreError> {
        let file_name = format!("{}.jsonl", session_id.as_str());
        let path = self.sessions_dir.join(&file_name);
        reject_indirect_file(&path, "session file")?;
        Ok(SessionLayout {
            workspace: self.workspace.clone(),
            #[cfg(unix)]
            state_root: self.state_root.clone(),
            #[cfg(unix)]
            workspace_id: self.workspace_id.clone(),
            #[cfg(unix)]
            workspace_dir: self.workspace_dir.clone(),
            #[cfg(unix)]
            sessions_dir: self.sessions_dir.clone(),
            path,
            file_name,
        })
    }
}

pub(crate) struct SessionLayout {
    pub workspace: PathBuf,
    #[cfg(unix)]
    pub state_root: PathBuf,
    #[cfg(unix)]
    pub workspace_id: String,
    #[cfg(unix)]
    pub workspace_dir: PathBuf,
    #[cfg(unix)]
    pub sessions_dir: PathBuf,
    pub path: PathBuf,
    file_name: String,
}

impl SessionLayout {
    pub(crate) fn prepare(
        workspace: impl AsRef<Path>,
        session_id: &SessionId,
    ) -> Result<Self, SessionStoreError> {
        WorkspaceLayout::resolve(workspace, true)?
            .ok_or(SessionStoreError::NotFound)?
            .session(session_id)
    }

    pub(crate) fn existing(
        workspace: impl AsRef<Path>,
        session_id: &SessionId,
    ) -> Result<Self, SessionStoreError> {
        WorkspaceLayout::resolve(workspace, false)?
            .ok_or(SessionStoreError::NotFound)?
            .session(session_id)
    }

    pub(crate) fn open_jsonl(&self, create: bool) -> Result<File, SessionStoreError> {
        open_contained_jsonl(self, create)
    }

    pub(crate) fn try_open_existing(&self) -> Result<Option<File>, SessionStoreError> {
        match self.open_jsonl(false) {
            Ok(file) => Ok(Some(file)),
            Err(SessionStoreError::NotFound) => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn replace_with(&self, bytes: &[u8]) -> Result<(), SessionStoreError> {
        replace_contained_jsonl(self, bytes)
    }
}

pub(crate) fn session_entries(
    workspace: impl AsRef<Path>,
) -> Result<Vec<SessionEntry>, SessionStoreError> {
    let Some(layout) = WorkspaceLayout::resolve(workspace, false)? else {
        return Ok(Vec::new());
    };
    contained_session_entries(&layout)
}

pub(crate) fn session_directory(workspace: impl AsRef<Path>) -> Result<PathBuf, SessionStoreError> {
    let workspace = state::canonical_workspace(workspace.as_ref())?;
    let state_root = state::validated_state_root(&workspace)?;
    let workspace_id = state::workspace_id(&workspace);
    Ok(state_root.join(state::WORKSPACES_DIR).join(workspace_id).join(state::SESSION_DIR))
}

pub(crate) fn canonical_workspace(path: impl AsRef<Path>) -> Result<PathBuf, SessionStoreError> {
    state::canonical_workspace(path.as_ref())
}

pub(crate) fn workspace_id(path: impl AsRef<Path>) -> Result<String, SessionStoreError> {
    let workspace = state::canonical_workspace(path.as_ref())?;
    Ok(state::workspace_id(&workspace))
}

pub(crate) fn state_root() -> Result<PathBuf, SessionStoreError> {
    state::resolve_state_root()
}

#[derive(Deserialize, Serialize)]
struct WorkspaceMetadata {
    version: u32,
    workspace: String,
    workspace_native: String,
}

fn workspace_metadata_bytes(workspace: &Path) -> Result<Vec<u8>, SessionStoreError> {
    serde_json::to_vec(&WorkspaceMetadata {
        version: state::WORKSPACE_METADATA_VERSION,
        workspace: state::workspace_display(workspace),
        workspace_native: state::native_path_hex(workspace),
    })
    .map_err(|error| SessionStoreError::Invalid(error.to_string()))
}

fn validate_workspace_metadata(bytes: &[u8], workspace: &Path) -> Result<(), SessionStoreError> {
    if bytes.len() > state::MAX_WORKSPACE_METADATA_BYTES {
        return Err(SessionStoreError::Invalid("workspace metadata exceeds size limit".to_owned()));
    }
    let metadata: WorkspaceMetadata = serde_json::from_slice(bytes).map_err(|error| {
        SessionStoreError::Invalid(format!("workspace metadata is invalid: {error}"))
    })?;
    if metadata.version != state::WORKSPACE_METADATA_VERSION {
        return Err(SessionStoreError::Invalid(format!(
            "unsupported workspace metadata version {}",
            metadata.version
        )));
    }
    let expected = workspace_metadata_bytes(workspace)?;
    let expected: WorkspaceMetadata = serde_json::from_slice(&expected)
        .map_err(|error| SessionStoreError::Invalid(error.to_string()))?;
    if metadata.workspace != expected.workspace
        || metadata.workspace_native != expected.workspace_native
    {
        return Err(SessionStoreError::Invalid(
            "workspace metadata does not match the current canonical workspace".to_owned(),
        ));
    }
    Ok(())
}

fn ensure_workspace_metadata(layout: &WorkspaceLayout) -> Result<(), SessionStoreError> {
    let bytes = workspace_metadata_bytes(&layout.workspace)?;

    #[cfg(unix)]
    {
        ensure_workspace_metadata_unix(layout, &bytes)
    }

    #[cfg(not(unix))]
    {
        ensure_workspace_metadata_std(layout, &bytes)
    }
}

#[cfg(unix)]
fn ensure_workspace_metadata_unix(
    layout: &WorkspaceLayout,
    bytes: &[u8],
) -> Result<(), SessionStoreError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fchmod, fstat, fsync, linkat, openat};
    use std::os::fd::AsFd;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    let workspace_fd = open_state_workspace_dir(layout)?;
    let temp_name = format!(
        ".aether-fx-workspace-{}-{}.tmp",
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let fd = match openat(
        workspace_fd.as_fd(),
        temp_name.as_str(),
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::EXIST) => {
            return Err(SessionStoreError::Invalid(
                "workspace metadata temporary file already exists".to_owned(),
            ));
        }
        Err(error) => {
            return Err(map_unix_open_error("create workspace metadata temporary", error));
        }
    };
    fchmod(&fd, Mode::RUSR | Mode::WUSR)
        .map_err(|error| io_err("restrict workspace metadata temporary", error))?;
    let stat = fstat(&fd).map_err(|error| io_err("stat workspace metadata temporary", error))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(SessionStoreError::Invalid(
            "workspace metadata temporary is not a regular file".to_owned(),
        ));
    }
    let mut file = File::from(fd);
    let write = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = rustix::fs::unlinkat(workspace_fd.as_fd(), temp_name.as_str(), AtFlags::empty());
        return Err(io_err("write workspace metadata", error));
    }
    drop(file);
    match linkat(
        workspace_fd.as_fd(),
        temp_name.as_str(),
        workspace_fd.as_fd(),
        state::WORKSPACE_METADATA,
        AtFlags::empty(),
    ) {
        Ok(()) => {
            let _ = fsync(&workspace_fd);
            rustix::fs::unlinkat(workspace_fd.as_fd(), temp_name.as_str(), AtFlags::empty())
                .map_err(|error| io_err("remove workspace metadata temporary", error))?;
            Ok(())
        }
        Err(rustix::io::Errno::EXIST) => {
            let _ =
                rustix::fs::unlinkat(workspace_fd.as_fd(), temp_name.as_str(), AtFlags::empty());
            validate_workspace_metadata_unix(&workspace_fd, &layout.workspace)
        }
        Err(error) => {
            let _ =
                rustix::fs::unlinkat(workspace_fd.as_fd(), temp_name.as_str(), AtFlags::empty());
            Err(map_unix_open_error("install workspace metadata", error))
        }
    }
}

#[cfg(unix)]
fn validate_workspace_metadata_unix(
    workspace_fd: &rustix::fd::OwnedFd,
    workspace: &Path,
) -> Result<(), SessionStoreError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, openat};
    use std::os::fd::AsFd;

    let fd = openat(
        workspace_fd.as_fd(),
        state::WORKSPACE_METADATA,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open workspace metadata", error))?;
    let stat = fstat(&fd).map_err(|error| io_err("stat workspace metadata", error))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(SessionStoreError::Invalid(
            "workspace metadata is not a regular file".to_owned(),
        ));
    }
    let mut file = File::from(fd);
    let existing = read_bounded(&mut file, "read workspace metadata")?;
    validate_workspace_metadata(&existing, workspace)
}

#[cfg(not(unix))]
fn ensure_workspace_metadata_std(
    layout: &WorkspaceLayout,
    bytes: &[u8],
) -> Result<(), SessionStoreError> {
    let path = layout.workspace_dir.join(state::WORKSPACE_METADATA);
    reject_indirect_file(&path, "workspace metadata")?;
    let temporary = layout.workspace_dir.join(format!(
        ".aether-fx-workspace-{}-{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| io_err("name workspace metadata temporary", error))?
            .as_nanos()
    ));
    reject_symlink(&temporary, "workspace metadata temporary file")?;
    let mut options = open_options_nofollow();
    options.write(true).create_new(true);
    match options.open(&temporary) {
        Ok(mut file) => {
            let write = (|| {
                file.write_all(bytes)?;
                file.flush()?;
                file.sync_all()
            })();
            if let Err(error) = write {
                let _ = fs::remove_file(&temporary);
                return Err(io_err("write workspace metadata", error));
            }
            drop(file);
            match fs::hard_link(&temporary, &path) {
                Ok(()) => {
                    let _ = fs::remove_file(&temporary);
                    reject_indirect_file(&path, "workspace metadata")?;
                    Ok(())
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&temporary);
                    let mut options = open_options_nofollow();
                    options.read(true);
                    let mut installed = options
                        .open(&path)
                        .map_err(|error| io_err("open workspace metadata", error))?;
                    let existing = read_bounded(&mut installed, "read workspace metadata")?;
                    validate_workspace_metadata(&existing, &layout.workspace)
                }
                Err(error) => {
                    let _ = fs::remove_file(&temporary);
                    Err(io_err("install workspace metadata", error))
                }
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            Err(SessionStoreError::Invalid(
                "workspace metadata temporary file already exists".to_owned(),
            ))
        }
        Err(error) => Err(io_err("create workspace metadata temporary", error)),
    }
}

fn read_bounded(file: &mut File, operation: &str) -> Result<Vec<u8>, SessionStoreError> {
    let limit = state::MAX_WORKSPACE_METADATA_BYTES;
    let mut bytes = Vec::with_capacity(limit.min(4096));
    file.take((limit as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| io_err(operation, error))?;
    if bytes.len() > limit {
        return Err(SessionStoreError::Invalid("workspace metadata exceeds size limit".to_owned()));
    }
    Ok(bytes)
}

fn reject_indirect_file(path: &Path, what: &str) -> Result<(), SessionStoreError> {
    reject_symlink(path, what)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_err("stat state file", error)),
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(SessionStoreError::Invalid(format!("{what} is not a regular file"))),
    }
}

fn reject_symlink(path: &Path, what: &str) -> Result<(), SessionStoreError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_err("stat state storage", error)),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(SessionStoreError::Invalid(format!(
                    "{what} is a symbolic link or reparse point"
                )));
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn ensure_state_root_path(state_root: &Path) -> Result<(), SessionStoreError> {
    use rustix::fs::{FileType, Mode, OFlags, fstat, mkdirat, openat};
    use std::os::fd::AsFd;

    if !state_root.is_absolute() {
        return Err(SessionStoreError::Invalid("AETHER Fx state root must be absolute".to_owned()));
    }
    let mut directory = openat(
        rustix::fs::CWD,
        "/",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open filesystem root", error))?;
    for component in state_root.components() {
        let std::path::Component::Normal(name) = component else { continue };
        match mkdirat(directory.as_fd(), name, Mode::RWXU) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) => return Err(map_unix_open_error("create AETHER Fx state root", error)),
        }
        directory = openat(
            directory.as_fd(),
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| map_unix_open_error("open AETHER Fx state root", error))?;
        let stat = fstat(&directory).map_err(|error| io_err("stat AETHER Fx state root", error))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(SessionStoreError::Invalid(
                "AETHER Fx state root is not a directory".to_owned(),
            ));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_state_root_path(state_root: &Path) -> Result<(), SessionStoreError> {
    reject_symlink(state_root, "AETHER Fx state root")?;
    if let Err(error) = fs::create_dir(state_root)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(io_err("create AETHER Fx state root", error));
    }
    real_directory_exists(state_root, "AETHER Fx state root")?.then_some(()).ok_or_else(|| {
        SessionStoreError::Invalid("AETHER Fx state root is not a directory".to_owned())
    })
}

fn state_directories_exist(
    state_root: &Path,
    workspace_id: &str,
) -> Result<bool, SessionStoreError> {
    if !real_directory_exists(state_root, "AETHER Fx state root")? {
        return Ok(false);
    }
    let workspaces = state_root.join(state::WORKSPACES_DIR);
    if !real_directory_exists(&workspaces, "state workspaces directory")? {
        return Ok(false);
    }
    let workspace_dir = workspaces.join(workspace_id);
    if !real_directory_exists(&workspace_dir, "workspace state directory")? {
        return Ok(false);
    }
    if !real_directory_exists(&workspace_dir.join(state::SESSION_DIR), "session directory")? {
        return Ok(false);
    }
    #[cfg(unix)]
    restrict_existing_directories(state_root, workspace_id)?;
    Ok(true)
}

fn real_directory_exists(path: &Path, what: &str) -> Result<bool, SessionStoreError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(io_err("stat state directory", error)),
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || is_reparse_point(&metadata)
                || !metadata.file_type().is_dir() =>
        {
            Err(SessionStoreError::Invalid(format!(
                "{what} is a symbolic link, reparse point, or not a directory"
            )))
        }
        Ok(_) => Ok(true),
    }
}

#[cfg(unix)]
fn prepare_directories(state_root: &Path, workspace_id: &str) -> Result<(), SessionStoreError> {
    use rustix::fs::{CWD, Mode, OFlags, fchmod, openat};
    use std::os::fd::AsFd;

    ensure_state_root_path(state_root)?;
    let state = openat(
        CWD,
        state_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open AETHER Fx state root", error))?;
    fchmod(&state, Mode::RWXU).map_err(|error| io_err("restrict AETHER Fx state root", error))?;
    let workspaces = ensure_unix_dir(state.as_fd(), state::WORKSPACES_DIR)?;
    let workspace = ensure_unix_dir(workspaces.as_fd(), workspace_id)?;
    let _sessions = ensure_unix_dir(workspace.as_fd(), state::SESSION_DIR)?;
    Ok(())
}

#[cfg(not(unix))]
fn prepare_directories(state_root: &Path, workspace_id: &str) -> Result<(), SessionStoreError> {
    ensure_state_root_path(state_root)?;
    let workspaces = state_root.join(state::WORKSPACES_DIR);
    ensure_real_dir(&workspaces, state_root, "state workspaces directory")?;
    let workspace = workspaces.join(workspace_id);
    ensure_real_dir(&workspace, state_root, "workspace state directory")?;
    ensure_real_dir(&workspace.join(state::SESSION_DIR), state_root, "session directory")
}

#[cfg(unix)]
fn ensure_unix_dir(
    parent: rustix::fd::BorrowedFd<'_>,
    name: &str,
) -> Result<rustix::fd::OwnedFd, SessionStoreError> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, mkdirat, openat};

    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(map_unix_open_error("create state directory", error)),
    }
    let fd = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open state directory", error))?;
    fchmod(&fd, Mode::RWXU).map_err(|error| io_err("restrict state directory", error))?;
    let stat = fstat(&fd).map_err(|error| io_err("stat state directory", error))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(SessionStoreError::Invalid("state directory is not a directory".to_owned()));
    }
    Ok(fd)
}

#[cfg(unix)]
fn restrict_existing_directories(
    state_root: &Path,
    workspace_id: &str,
) -> Result<(), SessionStoreError> {
    use rustix::fs::{CWD, Mode, OFlags, fchmod, openat};
    use std::os::fd::AsFd;

    let state = openat(
        CWD,
        state_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open AETHER Fx state root", error))?;
    fchmod(&state, Mode::RWXU).map_err(|error| io_err("restrict AETHER Fx state root", error))?;
    let workspaces = openat(
        state.as_fd(),
        state::WORKSPACES_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open state workspaces directory", error))?;
    fchmod(&workspaces, Mode::RWXU)
        .map_err(|error| io_err("restrict state workspaces directory", error))?;
    let workspace = openat(
        workspaces.as_fd(),
        workspace_id,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open workspace state directory", error))?;
    fchmod(&workspace, Mode::RWXU)
        .map_err(|error| io_err("restrict workspace state directory", error))?;
    let sessions = openat(
        workspace.as_fd(),
        state::SESSION_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open session directory", error))?;
    fchmod(&sessions, Mode::RWXU).map_err(|error| io_err("restrict session directory", error))?;
    Ok(())
}

#[cfg(not(unix))]
fn ensure_real_dir(path: &Path, state_root: &Path, what: &str) -> Result<(), SessionStoreError> {
    reject_symlink(path, what)?;
    if let Err(error) = fs::create_dir(path)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(io_err("create state directory", error));
    }
    reject_symlink(path, what)?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_err("stat state directory", error))?;
    if !metadata.file_type().is_dir() {
        return Err(SessionStoreError::Invalid(format!("{what} is not a directory")));
    }
    let canonical =
        fs::canonicalize(path).map_err(|error| io_err("canonicalize state directory", error))?;
    let state_root = fs::canonicalize(state_root)
        .map_err(|error| io_err("canonicalize AETHER Fx state root", error))?;
    if !path_inside(&canonical, &state_root) {
        return Err(SessionStoreError::Invalid("state path escapes the state root".to_owned()));
    }
    Ok(())
}

#[cfg(unix)]
fn open_state_workspace_dir(
    layout: &WorkspaceLayout,
) -> Result<rustix::fd::OwnedFd, SessionStoreError> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    use std::os::fd::AsFd;

    let state = openat(
        CWD,
        &layout.state_root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open AETHER Fx state root", error))?;
    let workspaces = openat(
        state.as_fd(),
        state::WORKSPACES_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open state workspaces directory", error))?;
    openat(
        workspaces.as_fd(),
        layout.workspace_id.as_str(),
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open workspace state directory", error))
}

#[cfg(unix)]
fn open_state_sessions(layout: &WorkspaceLayout) -> Result<rustix::fd::OwnedFd, SessionStoreError> {
    use rustix::fs::{Mode, OFlags, openat};
    use std::os::fd::AsFd;

    let workspace = open_state_workspace_dir(layout)?;
    openat(
        workspace.as_fd(),
        state::SESSION_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open session directory", error))
}

#[cfg(unix)]
fn contained_session_entries(
    layout: &WorkspaceLayout,
) -> Result<Vec<SessionEntry>, SessionStoreError> {
    use rustix::fs::{Dir, FileType, Mode, OFlags, fstat, openat};
    use std::os::fd::AsFd;

    let sessions = open_state_sessions(layout)?;
    let directory =
        Dir::read_from(&sessions).map_err(|error| io_err("read session directory", error))?;
    let mut entries = Vec::new();
    for entry in directory {
        let entry = entry.map_err(|error| io_err("read session directory", error))?;
        let Ok(name) = entry.file_name().to_str() else { continue };
        let Some(stem) = name.strip_suffix(".jsonl") else { continue };
        let Ok(session_id) = SessionId::new(stem.to_owned()) else { continue };
        let fd = openat(
            sessions.as_fd(),
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map_err(|error| map_unix_open_error("open session file", error))?;
        let stat = fstat(&fd).map_err(|error| io_err("stat session file", error))?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            continue;
        }
        let file = File::from(fd);
        let modified = file
            .metadata()
            .and_then(|metadata| metadata.modified())
            .map_err(|error| io_err("stat session file", error))?;
        entries.push(SessionEntry { session_id, file, modified });
    }
    Ok(entries)
}

#[cfg(unix)]
fn open_contained_jsonl(layout: &SessionLayout, create: bool) -> Result<File, SessionStoreError> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, openat};
    use std::os::fd::AsFd;

    let workspace = open_state_workspace_dir(&WorkspaceLayout {
        workspace: layout.workspace.clone(),
        state_root: layout.state_root.clone(),
        workspace_id: layout.workspace_id.clone(),
        workspace_dir: layout.workspace_dir.clone(),
        sessions_dir: layout.sessions_dir.clone(),
    })?;
    let sessions = openat(
        workspace.as_fd(),
        state::SESSION_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open session directory", error))?;
    let mut flags = OFlags::RDWR | OFlags::APPEND | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    if create {
        flags |= OFlags::CREATE;
    }
    let fd = openat(sessions.as_fd(), layout.file_name.as_str(), flags, Mode::RUSR | Mode::WUSR)
        .map_err(|error| map_unix_open_error("open session file", error))?;
    fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(|error| io_err("restrict session file", error))?;
    let stat = fstat(&fd).map_err(|error| io_err("stat session file", error))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(SessionStoreError::Invalid("session file is not a regular file".to_owned()));
    }
    Ok(File::from(fd))
}

#[cfg(unix)]
fn replace_contained_jsonl(layout: &SessionLayout, bytes: &[u8]) -> Result<(), SessionStoreError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, fchmod, openat, renameat, statat};
    use std::os::fd::AsFd;

    let workspace = open_state_workspace_dir(&WorkspaceLayout {
        workspace: layout.workspace.clone(),
        state_root: layout.state_root.clone(),
        workspace_id: layout.workspace_id.clone(),
        workspace_dir: layout.workspace_dir.clone(),
        sessions_dir: layout.sessions_dir.clone(),
    })?;
    let sessions = openat(
        workspace.as_fd(),
        state::SESSION_DIR,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open session directory", error))?;
    match statat(sessions.as_fd(), layout.file_name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink => {
            return Err(SessionStoreError::Invalid(
                "session file is a symbolic link or reparse point".to_owned(),
            ));
        }
        Ok(stat) if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile => {
            return Err(SessionStoreError::Invalid(
                "session file is not a regular file".to_owned(),
            ));
        }
        Ok(_) | Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(map_unix_open_error("stat session file", error)),
    }
    let temp_name =
        format!("{SESSION_TEMP_PREFIX}-{}-{}.tmp", std::process::id(), layout.file_name.as_str());
    let fd = openat(
        sessions.as_fd(),
        temp_name.as_str(),
        OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|error| map_unix_open_error("create compacted session", error))?;
    fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(|error| io_err("restrict session file", error))?;
    let mut file = File::from(fd);
    let write = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = rustix::fs::unlinkat(sessions.as_fd(), temp_name.as_str(), AtFlags::empty());
        return Err(io_err("write compacted session", error));
    }
    drop(file);
    renameat(sessions.as_fd(), temp_name.as_str(), sessions.as_fd(), layout.file_name.as_str())
        .map_err(|error| {
            let _ = rustix::fs::unlinkat(sessions.as_fd(), temp_name.as_str(), AtFlags::empty());
            map_unix_open_error("install compacted session", error)
        })?;
    Ok(())
}

#[cfg(not(unix))]
fn contained_session_entries(
    layout: &WorkspaceLayout,
) -> Result<Vec<SessionEntry>, SessionStoreError> {
    let canonical_sessions = fs::canonicalize(&layout.sessions_dir)
        .map_err(|error| io_err("canonicalize session directory", error))?;
    let canonical_root = fs::canonicalize(&layout.state_root)
        .map_err(|error| io_err("canonicalize AETHER Fx state root", error))?;
    if !path_inside(&canonical_sessions, &canonical_root) {
        return Err(SessionStoreError::Invalid("state path escapes the state root".to_owned()));
    }
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&layout.sessions_dir).map_err(|error| io_err("list sessions", error))?
    {
        let entry = entry.map_err(|error| io_err("read session directory", error))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(stem) = name.strip_suffix(".jsonl") else { continue };
        let Ok(session_id) = SessionId::new(stem.to_owned()) else { continue };
        let path = layout.sessions_dir.join(name);
        reject_indirect_file(&path, "session file")?;
        let mut options = open_options_nofollow();
        options.read(true);
        let file = options.open(&path).map_err(|error| io_err("open session file", error))?;
        let metadata = file.metadata().map_err(|error| io_err("stat session file", error))?;
        if !metadata.is_file() {
            continue;
        }
        let modified = metadata.modified().map_err(|error| io_err("stat session file", error))?;
        entries.push(SessionEntry { session_id, file, modified });
    }
    Ok(entries)
}

#[cfg(not(unix))]
fn open_contained_jsonl(layout: &SessionLayout, create: bool) -> Result<File, SessionStoreError> {
    reject_indirect_file(&layout.path, "session file")?;
    let mut options = open_options_nofollow();
    options.read(true).write(true).append(true);
    if create {
        options.create(true);
    }
    let file = options.open(&layout.path).map_err(|error| io_err("open session file", error))?;
    let metadata =
        fs::symlink_metadata(&layout.path).map_err(|error| io_err("stat session file", error))?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(SessionStoreError::Invalid(
            "session file is a symbolic link or reparse point".to_owned(),
        ));
    }
    if !metadata.file_type().is_file() {
        return Err(SessionStoreError::Invalid("session file is not a regular file".to_owned()));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn replace_contained_jsonl(layout: &SessionLayout, bytes: &[u8]) -> Result<(), SessionStoreError> {
    reject_indirect_file(&layout.path, "session file")?;
    let directory = layout.path.parent().ok_or_else(|| {
        SessionStoreError::Invalid("session path escapes the state root".to_owned())
    })?;
    let temporary = directory.join(format!(
        "{SESSION_TEMP_PREFIX}-{}-{}.tmp",
        std::process::id(),
        layout.file_name.as_str()
    ));
    reject_symlink(&temporary, "session temporary file")?;
    let mut options = open_options_nofollow();
    options.create_new(true).write(true);
    let mut file =
        options.open(&temporary).map_err(|error| io_err("create compacted session", error))?;
    let metadata = fs::symlink_metadata(&temporary)
        .map_err(|error| io_err("stat session temporary file", error))?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        let _ = fs::remove_file(&temporary);
        return Err(SessionStoreError::Invalid(
            "session temporary file is a symbolic link or reparse point".to_owned(),
        ));
    }
    let write = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = write {
        let _ = fs::remove_file(&temporary);
        return Err(io_err("write compacted session", error));
    }
    drop(file);
    fs::rename(&temporary, &layout.path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        io_err("install compacted session", error)
    })?;
    reject_indirect_file(&layout.path, "session file")
}

#[cfg(windows)]
fn open_options_nofollow() -> std::fs::OpenOptions {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
    let mut options = std::fs::OpenOptions::new();
    options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    options
}

#[cfg(not(unix))]
#[cfg(not(windows))]
fn open_options_nofollow() -> std::fs::OpenOptions {
    std::fs::OpenOptions::new()
}

#[cfg(not(unix))]
fn path_inside(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

fn io_err(operation: &str, error: impl ToString) -> SessionStoreError {
    SessionStoreError::Io { operation: operation.to_owned(), message: error.to_string() }
}

#[cfg(unix)]
fn map_unix_open_error(operation: &str, error: rustix::io::Errno) -> SessionStoreError {
    if error == rustix::io::Errno::NOENT {
        SessionStoreError::NotFound
    } else if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR {
        SessionStoreError::Invalid("state storage is a symbolic link or reparse point".to_owned())
    } else {
        io_err(operation, error)
    }
}
