use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use aether_core::SessionId;

use crate::session::SessionStoreError;

pub(crate) struct SessionLayout {
    pub workspace: PathBuf,
    pub path: PathBuf,
    file_name: String,
}

impl SessionLayout {
    pub(crate) fn prepare(
        workspace: impl AsRef<Path>,
        session_id: &SessionId,
    ) -> Result<Self, SessionStoreError> {
        let workspace = canonical_workspace(workspace.as_ref())?;
        let file_name = format!("{}.jsonl", session_id.as_str());
        let path = workspace.join(".aether").join("sessions").join(&file_name);
        prepare_directories(&workspace)?;
        reject_indirect_file(&path)?;
        Ok(Self { workspace, path, file_name })
    }

    pub(crate) fn from_existing_path(
        live_workspace: impl AsRef<Path>,
        path: impl AsRef<Path>,
    ) -> Result<Self, SessionStoreError> {
        let workspace = canonical_workspace(live_workspace.as_ref())?;
        let path = path.as_ref();
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| SessionStoreError::Invalid("session file name is invalid".to_owned()))?
            .to_owned();
        if !file_name.ends_with(".jsonl") {
            return Err(SessionStoreError::Invalid("session file name is invalid".to_owned()));
        }
        let sessions_dir = path.parent().ok_or_else(|| {
            SessionStoreError::Invalid("session path escapes the workspace".to_owned())
        })?;
        let aether_dir = sessions_dir.parent().ok_or_else(|| {
            SessionStoreError::Invalid("session path escapes the workspace".to_owned())
        })?;
        let claimed_workspace = aether_dir.parent().ok_or_else(|| {
            SessionStoreError::Invalid("session path escapes the workspace".to_owned())
        })?;
        if sessions_dir.file_name() != Some(std::ffi::OsStr::new("sessions"))
            || aether_dir.file_name() != Some(std::ffi::OsStr::new(".aether"))
        {
            return Err(SessionStoreError::Invalid(
                "session path escapes the workspace".to_owned(),
            ));
        }
        let claimed = fs::canonicalize(claimed_workspace)
            .map_err(|error| io_err("canonicalize session workspace prefix", error))?;
        if claimed != workspace {
            return Err(SessionStoreError::Invalid(
                "session path escapes the workspace".to_owned(),
            ));
        }
        prepare_directories(&workspace)?;
        let expected = workspace.join(".aether").join("sessions").join(&file_name);
        reject_indirect_file(&expected)?;
        Ok(Self { workspace, path: expected, file_name })
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

fn canonical_workspace(path: &Path) -> Result<PathBuf, SessionStoreError> {
    let workspace = fs::canonicalize(path).map_err(|error| SessionStoreError::Io {
        operation: "canonicalize workspace".to_owned(),
        message: error.to_string(),
    })?;
    let metadata = fs::metadata(&workspace).map_err(|error| SessionStoreError::Io {
        operation: "stat workspace".to_owned(),
        message: error.to_string(),
    })?;
    if !metadata.is_dir() {
        return Err(SessionStoreError::Invalid("workspace is not a directory".to_owned()));
    }
    Ok(workspace)
}

#[cfg(not(unix))]
fn path_inside(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

fn io_err(operation: &str, error: impl ToString) -> SessionStoreError {
    SessionStoreError::Io { operation: operation.to_owned(), message: error.to_string() }
}

fn reject_symlink(path: &Path, what: &str) -> Result<(), SessionStoreError> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_err("stat session storage", error)),
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

fn reject_indirect_file(path: &Path) -> Result<(), SessionStoreError> {
    reject_symlink(path, "session file")?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_err("stat session file", error)),
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(SessionStoreError::Invalid("session file is not a regular file".to_owned())),
    }
}

#[cfg(unix)]
fn prepare_directories(workspace: &Path) -> Result<(), SessionStoreError> {
    use rustix::fs::{CWD, Mode, OFlags, openat};
    use std::os::fd::AsFd;

    let workspace_fd =
        openat(CWD, workspace, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|error| io_err("open workspace", error))?;
    let aether = ensure_unix_dir(workspace_fd.as_fd(), ".aether")?;
    let _sessions = ensure_unix_dir(aether.as_fd(), "sessions")?;
    Ok(())
}

#[cfg(unix)]
fn ensure_unix_dir(
    parent: rustix::fd::BorrowedFd<'_>,
    name: &str,
) -> Result<rustix::fd::OwnedFd, SessionStoreError> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, mkdirat, openat};

    match mkdirat(parent, name, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(map_unix_open_error("create session directory", error)),
    }
    let fd = openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open session directory", error))?;
    fchmod(&fd, Mode::RWXU).map_err(|error| io_err("restrict session directory", error))?;
    let stat = fstat(&fd).map_err(|error| io_err("stat session directory", error))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(SessionStoreError::Invalid("session directory is not a directory".to_owned()));
    }
    Ok(fd)
}

#[cfg(unix)]
fn map_unix_open_error(operation: &str, error: rustix::io::Errno) -> SessionStoreError {
    if error == rustix::io::Errno::NOENT {
        SessionStoreError::NotFound
    } else if error == rustix::io::Errno::LOOP {
        SessionStoreError::Invalid("session storage is a symbolic link or reparse point".to_owned())
    } else {
        io_err(operation, error)
    }
}

#[cfg(unix)]
fn open_contained_jsonl(layout: &SessionLayout, create: bool) -> Result<File, SessionStoreError> {
    use rustix::fs::{CWD, FileType, Mode, OFlags, fchmod, fstat, openat};
    use std::os::fd::AsFd;

    let workspace_fd = openat(
        CWD,
        &layout.workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_err("open workspace", error))?;
    let aether = openat(
        workspace_fd.as_fd(),
        ".aether",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open session directory", error))?;
    let sessions = openat(
        aether.as_fd(),
        "sessions",
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
    use rustix::fs::{AtFlags, CWD, FileType, Mode, OFlags, fchmod, openat, renameat, statat};
    use std::os::fd::AsFd;

    let workspace_fd = openat(
        CWD,
        &layout.workspace,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| io_err("open workspace", error))?;
    let aether = openat(
        workspace_fd.as_fd(),
        ".aether",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| map_unix_open_error("open session directory", error))?;
    let sessions = openat(
        aether.as_fd(),
        "sessions",
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
        format!(".aether-session-{}-{}.tmp", std::process::id(), layout.file_name.as_str());
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
fn prepare_directories(workspace: &Path) -> Result<(), SessionStoreError> {
    let aether = workspace.join(".aether");
    ensure_real_dir(&aether, workspace)?;
    let sessions = aether.join("sessions");
    ensure_real_dir(&sessions, workspace)
}

#[cfg(not(unix))]
fn ensure_real_dir(path: &Path, workspace: &Path) -> Result<(), SessionStoreError> {
    reject_symlink(path, "session directory")?;
    if let Err(error) = fs::create_dir(path)
        && error.kind() != io::ErrorKind::AlreadyExists
    {
        return Err(io_err("create session directory", error));
    }
    reject_symlink(path, "session directory")?;
    let metadata =
        fs::symlink_metadata(path).map_err(|error| io_err("stat session directory", error))?;
    if !metadata.file_type().is_dir() {
        return Err(SessionStoreError::Invalid("session directory is not a directory".to_owned()));
    }
    let canonical =
        fs::canonicalize(path).map_err(|error| io_err("canonicalize session directory", error))?;
    if !path_inside(&canonical, workspace) {
        return Err(SessionStoreError::Invalid("session path escapes the workspace".to_owned()));
    }
    Ok(())
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
fn open_contained_jsonl(layout: &SessionLayout, create: bool) -> Result<File, SessionStoreError> {
    reject_indirect_file(&layout.path)?;
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
    reject_indirect_file(&layout.path)?;
    let directory = layout.path.parent().ok_or_else(|| {
        SessionStoreError::Invalid("session path escapes the workspace".to_owned())
    })?;
    let temporary = directory.join(format!(
        ".aether-session-{}-{}.tmp",
        std::process::id(),
        layout.file_name.as_str()
    ));
    reject_symlink(&temporary, "session temporary file")?;
    let mut options = open_options_nofollow();
    options.create_new(true).write(true);
    let mut file =
        options.open(&temporary).map_err(|error| io_err("create compacted session", error))?;
    let metadata =
        fs::symlink_metadata(&temporary).map_err(|error| io_err("stat session file", error))?;
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
    reject_indirect_file(&layout.path)
}
