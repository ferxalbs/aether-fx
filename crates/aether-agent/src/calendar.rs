use std::{
    fs::{self, File, OpenOptions},
    future::Future,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
};

use aether_core::{
    Appointment, AppointmentDraft, AppointmentId, CoreError, payload_contains_secrets,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::{SessionStore, SessionStoreError, session_fs, state};

const APPOINTMENT_SCHEMA_VERSION: u32 = 1;
const MAX_APPOINTMENTS: usize = 256;
const MAX_APPOINTMENT_FILE_BYTES: usize = 1024 * 1024;
const APPOINTMENT_TEMP_PREFIX: &str = ".aether-fx-appointments";

/// Boxed future returned by a calendar adapter.
pub type CalendarFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Appointment, CalendarError>> + Send + 'a>>;

/// Provider-neutral appointment creation boundary.
pub trait CalendarAdapter: Send + Sync {
    /// Validate and create one appointment.
    fn create<'a>(&'a self, draft: AppointmentDraft) -> CalendarFuture<'a>;
}

/// Errors returned by local or future provider-backed calendar adapters.
#[derive(Debug, Error)]
pub enum CalendarError {
    /// Appointment fields did not satisfy the core contract.
    #[error(transparent)]
    Invalid(#[from] CoreError),
    /// Private state storage failed or was malformed.
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    /// A local serialization boundary failed.
    #[error("appointment serialization failed: {0}")]
    Serialization(String),
    /// A blocking local operation could not be joined.
    #[error("appointment storage worker failed: {0}")]
    Worker(String),
}

#[derive(Clone, Debug)]
pub struct AppointmentStore {
    workspace: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct AppointmentFile {
    schema_version: u32,
    appointments: Vec<Appointment>,
}

impl AppointmentStore {
    /// Open a workspace-scoped appointment store in private OS state.
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, CalendarError> {
        let workspace = SessionStore::canonical_workspace(workspace)?;
        let _ = SessionStore::workspace_directory(&workspace)?;
        Ok(Self { workspace })
    }

    /// Return all locally confirmed appointments, bounded by the store contract.
    pub fn read(&self) -> Result<Vec<Appointment>, CalendarError> {
        let path = self.path()?;
        let Some(bytes) = read_optional_bounded(&path)? else {
            return Ok(Vec::new());
        };
        let file: AppointmentFile = serde_json::from_slice(&bytes)
            .map_err(|error| CalendarError::Serialization(error.to_string()))?;
        if file.schema_version != APPOINTMENT_SCHEMA_VERSION {
            return Err(CalendarError::Serialization(format!(
                "unsupported appointment schema {}",
                file.schema_version
            )));
        }
        if file.appointments.len() > MAX_APPOINTMENTS {
            return Err(CalendarError::Serialization(
                "appointment record limit exceeded".to_owned(),
            ));
        }
        for appointment in &file.appointments {
            appointment
                .validate()
                .map_err(|error| CalendarError::Serialization(error.to_string()))?;
        }
        Ok(file.appointments)
    }

    fn create(&self, draft: AppointmentDraft) -> Result<Appointment, CalendarError> {
        let now = OffsetDateTime::now_utc();
        let created_at = now
            .format(&Rfc3339)
            .map_err(|error| CalendarError::Serialization(error.to_string()))?;
        let pending_id = AppointmentId::new("appointment-pending")?;
        let mut appointment = draft.normalize(pending_id, now)?;
        let identity = serde_json::to_vec(&appointment)
            .map_err(|error| CalendarError::Serialization(error.to_string()))?;
        let mut identity_with_creation = identity;
        identity_with_creation.extend_from_slice(b"\n");
        identity_with_creation.extend_from_slice(created_at.as_bytes());
        let digest = blake3::hash(&identity_with_creation).to_hex().to_string();
        appointment.id = AppointmentId::new(format!("appointment-{digest}"))?;
        appointment.validate()?;

        let mut appointments = self.read()?;
        if appointments.len() >= MAX_APPOINTMENTS {
            return Err(CalendarError::Serialization(
                "appointment record limit reached".to_owned(),
            ));
        }
        appointments.push(appointment.clone());
        let file = AppointmentFile { schema_version: APPOINTMENT_SCHEMA_VERSION, appointments };
        let encoded = serde_json::to_vec(&file)
            .map_err(|error| CalendarError::Serialization(error.to_string()))?;
        if encoded.len() > MAX_APPOINTMENT_FILE_BYTES {
            return Err(CalendarError::Serialization(
                "appointment state exceeds its byte limit".to_owned(),
            ));
        }
        let value = serde_json::to_value(&file)
            .map_err(|error| CalendarError::Serialization(error.to_string()))?;
        if payload_contains_secrets(&value) {
            return Err(CalendarError::Serialization(
                "appointment state contains a secret-like value".to_owned(),
            ));
        }
        replace_atomically(&self.path()?, &encoded)?;
        Ok(appointment)
    }

    fn path(&self) -> Result<PathBuf, CalendarError> {
        Ok(SessionStore::workspace_directory(&self.workspace)?.join(state::APPOINTMENTS_FILE))
    }
}

/// Local provider-neutral adapter used by the interactive scheduling card.
#[derive(Clone, Debug)]
pub struct LocalCalendarAdapter {
    store: AppointmentStore,
}

impl LocalCalendarAdapter {
    /// Open a local adapter bound to one canonical workspace.
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, CalendarError> {
        Ok(Self { store: AppointmentStore::open(workspace)? })
    }

    /// Borrow the underlying local store for diagnostics and tests.
    pub fn store(&self) -> &AppointmentStore {
        &self.store
    }
}

impl CalendarAdapter for LocalCalendarAdapter {
    fn create<'a>(&'a self, draft: AppointmentDraft) -> CalendarFuture<'a> {
        let store = self.store.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || store.create(draft))
                .await
                .map_err(|error| CalendarError::Worker(error.to_string()))?
        })
    }
}

fn read_optional_bounded(path: &Path) -> Result<Option<Vec<u8>>, CalendarError> {
    session_fs::reject_indirect_file_for_calendar(path, "appointment file")?;
    let mut file = match open_readonly_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SessionStoreError::Io {
                operation: "open appointment file".to_owned(),
                message: error.to_string(),
            }
            .into());
        }
    };
    let mut bytes = Vec::with_capacity(MAX_APPOINTMENT_FILE_BYTES.min(4096));
    Read::by_ref(&mut file)
        .take((MAX_APPOINTMENT_FILE_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| SessionStoreError::Io {
            operation: "read appointment file".to_owned(),
            message: error.to_string(),
        })?;
    if bytes.len() > MAX_APPOINTMENT_FILE_BYTES {
        return Err(CalendarError::Serialization(
            "appointment file exceeds its byte limit".to_owned(),
        ));
    }
    Ok(Some(bytes))
}

fn replace_atomically(path: &Path, bytes: &[u8]) -> Result<(), CalendarError> {
    session_fs::reject_indirect_file_for_calendar(path, "appointment file")?;
    let directory = path.parent().ok_or_else(|| {
        CalendarError::Serialization("appointment path has no state directory".to_owned())
    })?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary =
        directory.join(format!("{APPOINTMENT_TEMP_PREFIX}-{}-{sequence}.tmp", std::process::id()));
    session_fs::reject_indirect_file_for_calendar(&temporary, "appointment temporary file")?;
    let mut file =
        OpenOptions::new().write(true).create_new(true).open(&temporary).map_err(|error| {
            SessionStoreError::Io {
                operation: "create appointment temporary".to_owned(),
                message: error.to_string(),
            }
        })?;
    let write_result = (|| {
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(SessionStoreError::Io {
            operation: "write appointment file".to_owned(),
            message: error.to_string(),
        }
        .into());
    }
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&temporary);
            return Err(SessionStoreError::Io {
                operation: "restrict appointment file".to_owned(),
                message: error.to_string(),
            }
            .into());
        }
    }
    install_replacement(path, &temporary).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        SessionStoreError::Io {
            operation: "install appointment file".to_owned(),
            message: error.to_string(),
        }
    })?;
    #[cfg(unix)]
    File::open(directory).and_then(|directory| directory.sync_all()).map_err(|error| {
        SessionStoreError::Io {
            operation: "sync appointment directory".to_owned(),
            message: error.to_string(),
        }
    })?;
    session_fs::reject_indirect_file_for_calendar(path, "appointment file")?;
    Ok(())
}

fn open_readonly_nofollow(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, openat};

        openat(
            rustix::fs::CWD,
            path,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )
        .map(File::from)
        .map_err(io::Error::from)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        let mut options = OpenOptions::new();
        options.read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        options.open(path)
    }
    #[cfg(not(any(unix, windows)))]
    {
        File::open(path)
    }
}

fn install_replacement(destination: &Path, replacement: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        if fs::symlink_metadata(destination).is_ok() {
            replace_existing_windows(destination, replacement)
        } else {
            install_missing_windows(destination, replacement)
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(replacement, destination)
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn replace_existing_windows(destination: &Path, replacement: &Path) -> io::Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::ReplaceFileW;

    let destination =
        OsStr::new(destination).encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
    let replacement =
        OsStr::new(replacement).encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
    // SAFETY: both UTF-16 buffers are NUL-terminated and remain alive for the call; all optional
    // backup, exclusion, and preserved-metadata pointers are intentionally null.
    let result = unsafe {
        ReplaceFileW(
            destination.as_ptr(),
            replacement.as_ptr(),
            std::ptr::null(),
            0,
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn install_missing_windows(destination: &Path, replacement: &Path) -> io::Result<()> {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW};

    let destination =
        OsStr::new(destination).encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
    let replacement =
        OsStr::new(replacement).encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
    // SAFETY: both UTF-16 buffers are NUL-terminated and remain alive for the call. The replace
    // flag is intentionally omitted so a concurrent destination is never overwritten here.
    let result =
        unsafe { MoveFileExW(replacement.as_ptr(), destination.as_ptr(), MOVEFILE_WRITE_THROUGH) };
    if result == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_workspace(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "aether-calendar-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn draft() -> AppointmentDraft {
        AppointmentDraft {
            title: "Appointment".to_owned(),
            date: "2026-09-03".to_owned(),
            time: "14:30".to_owned(),
            utc_offset: "-05:00".to_owned(),
            duration_minutes: "30".to_owned(),
            location: None,
            notes: None,
            attendees: Vec::new(),
        }
    }

    #[tokio::test]
    async fn local_adapter_round_trips_confirmed_appointments() {
        let root = temp_workspace("round-trip");
        let adapter = LocalCalendarAdapter::open(&root).unwrap();
        let appointment = adapter.create(draft()).await.unwrap();
        let stored = adapter.store().read().unwrap();
        assert_eq!(stored, vec![appointment]);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn appointment_file_is_outside_workspace() {
        let root = temp_workspace("outside");
        let store = AppointmentStore::open(&root).unwrap();
        assert!(!store.path().unwrap().starts_with(&root));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_existing_state_fails_closed_without_replacing_it() {
        let root = temp_workspace("malformed");
        let store = AppointmentStore::open(&root).unwrap();
        let path = store.path().unwrap();
        fs::write(&path, b"not-json").unwrap();
        let before = fs::read(&path).unwrap();
        assert!(store.create(draft()).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_existing_state_is_rejected() {
        let root = temp_workspace("oversized");
        let store = AppointmentStore::open(&root).unwrap();
        fs::write(store.path().unwrap(), vec![b'x'; MAX_APPOINTMENT_FILE_BYTES + 1]).unwrap();
        assert!(store.read().is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_existing_state_is_rejected_and_files_are_private() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let root = temp_workspace("symlink");
        let store = AppointmentStore::open(&root).unwrap();
        let path = store.path().unwrap();
        let target = root.join("redirected.json");
        fs::write(&target, b"{}").unwrap();
        symlink(&target, &path).unwrap();
        assert!(store.read().is_err());

        fs::remove_file(&path).unwrap();
        let adapter = LocalCalendarAdapter::open(&root).unwrap();
        let created = adapter.create(draft()).await.unwrap();
        let metadata = fs::metadata(store.path().unwrap()).unwrap();
        assert_eq!(metadata.mode() & 0o777, 0o600);
        assert_eq!(created.title.as_str(), "Appointment");
        let _ = fs::remove_dir_all(root);
    }
}
