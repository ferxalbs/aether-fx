use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use reqwest::Client;
use sha2::{Digest, Sha256};
use tokio::{
    fs as tokio_fs,
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

use crate::{
    ArchiveFormat, MAX_ARCHIVE_BYTES, MAX_EXTRACT_BYTES, ObscuraArtifact, ObscuraError,
    ObscuraPaths,
};

const MAX_VERSION_OUTPUT_BYTES: usize = 4 * 1024;
const MAX_EXTRACT_FILE_BYTES: u64 = 128 * 1024 * 1024;
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(180);
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_TEMP_ATTEMPTS: usize = 32;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

/// Result of installing the exact artifact embedded in this AETHER build.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObscuraInstallReport {
    pub updated: bool,
    pub version: String,
    pub target: String,
}

/// Read-only installation state used by status and the consent flow.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObscuraInstallationStatus {
    pub installed: bool,
    pub version_matches: bool,
    pub expected_version: String,
    pub installed_version: Option<String>,
    pub version_output: Option<String>,
    pub target: String,
}

#[derive(Debug)]
struct InstallLock {
    file: File,
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

impl ObscuraPaths {
    /// Create the provider's private directory tree with restrictive permissions where possible.
    pub(crate) fn ensure_layout_sync(&self) -> Result<(), ObscuraError> {
        ensure_private_directory(self.state_root())?;
        ensure_private_directory(self.obscura_root())?;
        ensure_private_directory(self.bin_root())?;
        ensure_private_directory(self.profiles_root())?;
        ensure_private_directory(self.install_dir())?;
        ensure_private_directory(self.profile_dir())
    }
}

pub(crate) async fn ensure_layout(paths: &ObscuraPaths) -> Result<(), ObscuraError> {
    paths.ensure_layout_sync()
}

/// Inspect only the fixed version directory. Older version directories are reported but never
/// executed, which makes a pin mismatch explicit instead of silently falling back.
pub async fn inspect_installation(
    paths: &ObscuraPaths,
) -> Result<ObscuraInstallationStatus, ObscuraError> {
    let binary = paths.binary_path();
    let worker = paths.worker_path();
    let binary_exists = path_exists(&binary)?;
    let worker_exists = path_exists(&worker)?;
    if binary_exists != worker_exists {
        return Err(ObscuraError::Installation {
            operation: "inspect fixed installation".to_owned(),
            message: "the pinned binary and worker must be installed together".to_owned(),
        });
    }
    let installed_version = if binary_exists {
        validate_regular_file(&binary, "pinned Obscura binary")?;
        validate_regular_file(&worker, "pinned Obscura worker")?;
        Some(paths.artifact().version.to_owned())
    } else {
        find_other_version(paths)?
    };
    let version_output =
        if binary_exists { Some(run_version(&binary, paths.profile_dir()).await?) } else { None };
    let version_matches = version_output.as_deref() == Some(paths.artifact().version_output);
    Ok(ObscuraInstallationStatus {
        installed: binary_exists,
        version_matches: binary_exists && version_matches,
        expected_version: paths.artifact().version.to_owned(),
        installed_version,
        version_output,
        target: paths.artifact().target.to_owned(),
    })
}

/// Download, verify, extract, and atomically activate only the current static manifest entry.
pub async fn install_obscura(paths: &ObscuraPaths) -> Result<ObscuraInstallReport, ObscuraError> {
    let artifact = paths.artifact();
    paths.ensure_layout_sync()?;
    let _lock = acquire_lock(&paths.obscura_root().join("install.lock"))?;
    let current = inspect_installation(paths).await?;
    if current.installed && current.version_matches {
        return Ok(ObscuraInstallReport {
            updated: false,
            version: artifact.version.to_owned(),
            target: artifact.target.to_owned(),
        });
    }
    let archive = temporary_path(paths.bin_root(), "download", archive_extension(artifact));
    if let Err(error) = download_archive(artifact, &archive).await {
        let _ = tokio_fs::remove_file(&archive).await;
        return Err(error);
    }
    let staging = temporary_directory(paths.bin_root(), "staging")?;
    let extraction = extract_verified_archive(archive.clone(), staging.clone(), *artifact).await;
    let _ = tokio_fs::remove_file(&archive).await;
    if let Err(error) = extraction {
        let _ = tokio_fs::remove_dir_all(&staging).await;
        return Err(error);
    }
    let staged_binary = staging.join(artifact.binary_name);
    let version_output = run_version(&staged_binary, paths.profile_dir()).await;
    let version_output = match version_output {
        Ok(output) => output,
        Err(error) => {
            let _ = tokio_fs::remove_dir_all(&staging).await;
            return Err(error);
        }
    };
    if let Err(error) = validate_version_output(&version_output, artifact) {
        let _ = tokio_fs::remove_dir_all(&staging).await;
        return Err(error);
    }
    if let Err(error) =
        validate_regular_file(&staging.join(artifact.worker_name), "staged Obscura worker")
    {
        let _ = tokio_fs::remove_dir_all(&staging).await;
        return Err(error);
    }
    if let Err(error) = activate_staging(paths, &staging).await {
        let _ = tokio_fs::remove_dir_all(&staging).await;
        return Err(error);
    }
    Ok(ObscuraInstallReport {
        updated: true,
        version: artifact.version.to_owned(),
        target: artifact.target.to_owned(),
    })
}

async fn download_archive(
    artifact: &ObscuraArtifact,
    destination: &Path,
) -> Result<(), ObscuraError> {
    if artifact.archive_size == 0 || artifact.archive_size > MAX_ARCHIVE_BYTES {
        return Err(ObscuraError::Integrity {
            message: "static archive size is outside the installer limit".to_owned(),
        });
    }
    let client = Client::builder()
        // GitHub release assets use an HTTPS redirect to immutable release storage. Follow only
        // HTTPS redirects; the embedded size and SHA-256 still decide whether the bytes can be
        // installed.
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(Duration::from_secs(10))
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| ObscuraError::Installation {
            operation: "create download client".to_owned(),
            message: error.to_string(),
        })?;
    let response =
        client.get(artifact.url).send().await.map_err(|error| ObscuraError::Installation {
            operation: "download Obscura".to_owned(),
            message: error.to_string(),
        })?;
    if !response.status().is_success() {
        return Err(ObscuraError::Installation {
            operation: "download Obscura".to_owned(),
            message: format!("release asset returned HTTP status {}", response.status()),
        });
    }
    if response.content_length().is_some_and(|length| length != artifact.archive_size) {
        return Err(ObscuraError::Integrity {
            message: "release asset Content-Length does not match the static manifest".to_owned(),
        });
    }
    let mut file =
        tokio_fs::OpenOptions::new().write(true).create_new(true).open(destination).await.map_err(
            |error| ObscuraError::Installation {
                operation: "create Obscura download".to_owned(),
                message: error.to_string(),
            },
        )?;
    let mut hasher = Sha256::new();
    let mut downloaded = 0_u64;
    let mut response = response;
    while let Some(chunk) = response.chunk().await.map_err(|error| ObscuraError::Installation {
        operation: "read Obscura download".to_owned(),
        message: error.to_string(),
    })? {
        downloaded = downloaded.saturating_add(chunk.len() as u64);
        if downloaded > artifact.archive_size || downloaded > MAX_ARCHIVE_BYTES {
            return Err(ObscuraError::Integrity {
                message: "release asset exceeded the static size limit".to_owned(),
            });
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(|error| ObscuraError::Installation {
            operation: "write Obscura download".to_owned(),
            message: error.to_string(),
        })?;
    }
    file.sync_all().await.map_err(|error| ObscuraError::Installation {
        operation: "sync Obscura download".to_owned(),
        message: error.to_string(),
    })?;
    drop(file);
    if downloaded != artifact.archive_size {
        return Err(ObscuraError::Integrity {
            message: "release asset size does not match the static manifest".to_owned(),
        });
    }
    if !hex_digest(&hasher.finalize()).eq_ignore_ascii_case(artifact.sha256) {
        return Err(ObscuraError::Integrity {
            message: "release asset SHA-256 does not match the static manifest".to_owned(),
        });
    }
    Ok(())
}

async fn extract_verified_archive(
    archive: PathBuf,
    staging: PathBuf,
    artifact: ObscuraArtifact,
) -> Result<(), ObscuraError> {
    tokio::task::spawn_blocking(move || extract_archive(&archive, &staging, &artifact))
        .await
        .map_err(|error| ObscuraError::Installation {
            operation: "extract Obscura archive".to_owned(),
            message: error.to_string(),
        })?
}

fn extract_archive(
    archive: &Path,
    staging: &Path,
    artifact: &ObscuraArtifact,
) -> Result<(), ObscuraError> {
    match artifact.archive_format {
        ArchiveFormat::Zip => extract_zip(archive, staging, artifact),
        ArchiveFormat::TarGz => extract_tar_gz(archive, staging, artifact),
    }
}

#[cfg(unix)]
fn extract_tar_gz(
    archive: &Path,
    staging: &Path,
    artifact: &ObscuraArtifact,
) -> Result<(), ObscuraError> {
    use flate2::read::GzDecoder;
    use tar::Archive;

    let file = File::open(archive).map_err(|error| archive_error("open", error))?;
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    let mut seen = [false; 2];
    let mut total = 0_u64;
    let entries = archive.entries().map_err(|error| archive_error("read", error))?;
    for entry in entries {
        let mut entry = entry.map_err(|error| archive_error("read entry", error))?;
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() || entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive contains a non-regular member".to_owned(),
            });
        }
        let name = entry.path().map_err(|error| archive_error("read entry path", error))?;
        let Some(index) = member_index(&name, artifact) else {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive contains an unexpected member".to_owned(),
            });
        };
        if seen[index] {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive contains a duplicate binary member".to_owned(),
            });
        }
        let declared =
            entry.header().size().map_err(|error| archive_error("read member size", error))?;
        if declared > MAX_EXTRACT_FILE_BYTES || total.saturating_add(declared) > MAX_EXTRACT_BYTES {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive exceeds the extraction limit".to_owned(),
            });
        }
        let destination =
            staging.join(if index == 0 { artifact.binary_name } else { artifact.worker_name });
        let mut output = create_staged_file(&destination)?;
        let copied = io::copy(&mut (&mut entry).take(MAX_EXTRACT_FILE_BYTES + 1), &mut output)
            .map_err(|error| archive_error("extract member", error))?;
        if copied != declared {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive member size changed while extracting".to_owned(),
            });
        }
        output.sync_all().map_err(|error| archive_error("sync member", error))?;
        set_executable(&destination).map_err(|error| archive_error("protect member", error))?;
        seen[index] = true;
        total = total.saturating_add(copied);
    }
    ensure_binaries_seen(seen)
}

#[cfg(not(unix))]
fn extract_tar_gz(
    _archive: &Path,
    _staging: &Path,
    _artifact: &ObscuraArtifact,
) -> Result<(), ObscuraError> {
    Err(ObscuraError::UnsupportedTarget { target: "non-unix tar.gz".to_owned() })
}

fn extract_zip(
    archive: &Path,
    staging: &Path,
    artifact: &ObscuraArtifact,
) -> Result<(), ObscuraError> {
    let file = File::open(archive).map_err(|error| archive_error("open", error))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| archive_error("read", error))?;
    let mut seen = [false; 2];
    let mut total = 0_u64;
    for index_in_archive in 0..archive.len() {
        let mut entry = archive
            .by_index(index_in_archive)
            .map_err(|error| archive_error("read entry", error))?;
        if entry.is_dir() || entry.is_symlink() {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive contains a directory or symlink".to_owned(),
            });
        }
        let name = entry.enclosed_name().ok_or_else(|| ObscuraError::Integrity {
            message: "Obscura archive contains an unsafe member path".to_owned(),
        })?;
        let Some(index) = member_index(&name, artifact) else {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive contains an unexpected member".to_owned(),
            });
        };
        if seen[index] {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive contains a duplicate binary member".to_owned(),
            });
        }
        let declared = entry.size();
        if declared > MAX_EXTRACT_FILE_BYTES || total.saturating_add(declared) > MAX_EXTRACT_BYTES {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive exceeds the extraction limit".to_owned(),
            });
        }
        let destination =
            staging.join(if index == 0 { artifact.binary_name } else { artifact.worker_name });
        let mut output = create_staged_file(&destination)?;
        let copied = io::copy(&mut (&mut entry).take(MAX_EXTRACT_FILE_BYTES + 1), &mut output)
            .map_err(|error| archive_error("extract member", error))?;
        if copied != declared {
            return Err(ObscuraError::Integrity {
                message: "Obscura archive member size changed while extracting".to_owned(),
            });
        }
        output.sync_all().map_err(|error| archive_error("sync member", error))?;
        set_executable(&destination).map_err(|error| archive_error("protect member", error))?;
        seen[index] = true;
        total = total.saturating_add(copied);
    }
    ensure_binaries_seen(seen)
}

fn member_index(path: &Path, artifact: &ObscuraArtifact) -> Option<usize> {
    let mut components = path.components();
    let Component::Normal(name) = components.next()? else { return None };
    if components.next().is_some() {
        return None;
    }
    let name = name.to_str()?;
    if name == artifact.binary_name {
        Some(0)
    } else if name == artifact.worker_name {
        Some(1)
    } else {
        None
    }
}

fn ensure_binaries_seen(seen: [bool; 2]) -> Result<(), ObscuraError> {
    if seen.into_iter().all(|value| value) {
        Ok(())
    } else {
        Err(ObscuraError::Integrity {
            message: "Obscura archive omitted the binary or worker".to_owned(),
        })
    }
}

fn create_staged_file(path: &Path) -> Result<File, ObscuraError> {
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| archive_error("create staged member", error))?;
    set_executable(path).map_err(|error| archive_error("protect staged member", error))?;
    Ok(file)
}

fn set_executable(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

async fn activate_staging(paths: &ObscuraPaths, staging: &Path) -> Result<(), ObscuraError> {
    let install_dir = paths.install_dir();
    if path_exists(install_dir)? {
        let metadata =
            fs::symlink_metadata(install_dir).map_err(|error| ObscuraError::Installation {
                operation: "inspect existing Obscura installation".to_owned(),
                message: error.to_string(),
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(ObscuraError::Integrity {
                message: "the fixed Obscura installation directory is not a regular directory"
                    .to_owned(),
            });
        }
    }
    let backup = temporary_directory_path(paths.bin_root(), "backup");
    let had_previous = path_exists(install_dir)?;
    if had_previous {
        fs::rename(install_dir, &backup).map_err(|error| ObscuraError::Installation {
            operation: "stage previous Obscura installation".to_owned(),
            message: error.to_string(),
        })?;
    }
    if let Err(error) = fs::rename(staging, install_dir) {
        if had_previous {
            let _ = fs::rename(&backup, install_dir);
        }
        return Err(ObscuraError::Installation {
            operation: "activate Obscura installation".to_owned(),
            message: error.to_string(),
        });
    }
    if had_previous {
        tokio_fs::remove_dir_all(&backup).await.map_err(|error| ObscuraError::Installation {
            operation: "clean previous Obscura installation".to_owned(),
            message: error.to_string(),
        })?;
    }
    Ok(())
}

async fn run_version(path: &Path, cwd: &Path) -> Result<String, ObscuraError> {
    if !path.is_absolute() || !cwd.is_absolute() {
        return Err(ObscuraError::Installation {
            operation: "execute Obscura --version".to_owned(),
            message: "the verified binary and private profile must use absolute paths".to_owned(),
        });
    }
    let mut child = Command::new(path);
    child
        .arg("--version")
        .current_dir(cwd)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let mut child = child.spawn().map_err(|error| ObscuraError::Installation {
        operation: "execute Obscura --version".to_owned(),
        message: error.to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| ObscuraError::Installation {
        operation: "read Obscura --version".to_owned(),
        message: "version command did not provide stdout".to_owned(),
    })?;
    let mut output = Vec::new();
    let read = timeout(
        VERSION_TIMEOUT,
        stdout.take((MAX_VERSION_OUTPUT_BYTES + 1) as u64).read_to_end(&mut output),
    )
    .await;
    if read.is_err() || read.as_ref().is_ok_and(|result| result.is_err()) {
        let _ = child.kill().await;
        return Err(ObscuraError::Installation {
            operation: "read Obscura --version".to_owned(),
            message: "version command timed out or failed".to_owned(),
        });
    }
    if output.len() > MAX_VERSION_OUTPUT_BYTES {
        let _ = child.kill().await;
        return Err(ObscuraError::Integrity {
            message: "Obscura --version output exceeded the validation limit".to_owned(),
        });
    }
    let status = timeout(VERSION_TIMEOUT, child.wait())
        .await
        .map_err(|_| ObscuraError::Installation {
            operation: "wait for Obscura --version".to_owned(),
            message: "version command timed out".to_owned(),
        })?
        .map_err(|error| ObscuraError::Installation {
            operation: "wait for Obscura --version".to_owned(),
            message: error.to_string(),
        })?;
    if !status.success() {
        return Err(ObscuraError::Integrity {
            message: "Obscura --version exited unsuccessfully".to_owned(),
        });
    }
    let text = String::from_utf8(output).map_err(|_| ObscuraError::Integrity {
        message: "Obscura --version output was not UTF-8".to_owned(),
    })?;
    Ok(text.trim().to_owned())
}

fn validate_version_output(actual: &str, artifact: &ObscuraArtifact) -> Result<(), ObscuraError> {
    if actual == artifact.version_output {
        Ok(())
    } else {
        Err(ObscuraError::Integrity {
            message: "verified archive contains an unexpected Obscura --version output".to_owned(),
        })
    }
}

fn acquire_lock(path: &Path) -> Result<InstallLock, ObscuraError> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && (metadata.file_type().is_symlink() || !metadata.is_file() || is_reparse_point(&metadata))
    {
        return Err(ObscuraError::Integrity {
            message: "Obscura installation lock is not a regular file".to_owned(),
        });
    }
    let file =
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).map_err(
            |error| ObscuraError::Installation {
                operation: "open Obscura installation lock".to_owned(),
                message: error.to_string(),
            },
        )?;
    match file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Err(ObscuraError::InstallLocked),
        Err(TryLockError::Error(error)) => {
            return Err(ObscuraError::Installation {
                operation: "lock Obscura installation".to_owned(),
                message: error.to_string(),
            });
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    // Keep the lock file in the private state directory. Removing it on release creates a
    // split-lock race: another installer could create a new inode while a waiter still holds
    // the old inode open.
    Ok(InstallLock { file })
}

fn validate_regular_file(path: &Path, label: &str) -> Result<(), ObscuraError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| ObscuraError::Installation {
        operation: format!("inspect {label}"),
        message: error.to_string(),
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(ObscuraError::Integrity {
            message: format!("{label} is not a regular non-link file"),
        });
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, ObscuraError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(ObscuraError::Installation {
            operation: "inspect Obscura installation path".to_owned(),
            message: error.to_string(),
        }),
    }
}

fn find_other_version(paths: &ObscuraPaths) -> Result<Option<String>, ObscuraError> {
    let entries = match fs::read_dir(paths.bin_root()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ObscuraError::Installation {
                operation: "inspect installed Obscura versions".to_owned(),
                message: error.to_string(),
            });
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| ObscuraError::Installation {
            operation: "inspect installed Obscura versions".to_owned(),
            message: error.to_string(),
        })?;
        let name = entry.file_name();
        if name == paths.artifact().version {
            continue;
        }
        let metadata =
            fs::symlink_metadata(entry.path()).map_err(|error| ObscuraError::Installation {
                operation: "inspect installed Obscura version".to_owned(),
                message: error.to_string(),
            })?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() && !is_reparse_point(&metadata) {
            let binary = entry.path().join(paths.artifact().binary_name);
            let worker = entry.path().join(paths.artifact().worker_name);
            if path_exists(&binary)? && path_exists(&worker)? {
                return Ok(name.to_str().map(str::to_owned));
            }
        }
    }
    Ok(None)
}

fn ensure_private_directory(path: &Path) -> Result<(), ObscuraError> {
    ensure_directory_chain(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
            ObscuraError::Installation {
                operation: "protect private Obscura directory".to_owned(),
                message: error.to_string(),
            }
        })?;
    }
    Ok(())
}

/// Create a missing directory chain without changing permissions on pre-existing ancestors.
/// Every newly created component is private, and every existing component is checked without
/// following links or reparse points.
fn ensure_directory_chain(path: &Path) -> Result<(), ObscuraError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => validate_private_directory_metadata(path, &metadata),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
                && parent != path
            {
                ensure_directory_chain(parent)?;
            }
            match fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(ObscuraError::Installation {
                        operation: "create private Obscura directory".to_owned(),
                        message: error.to_string(),
                    });
                }
            }
            let metadata =
                fs::symlink_metadata(path).map_err(|error| ObscuraError::Installation {
                    operation: "inspect private Obscura directory".to_owned(),
                    message: error.to_string(),
                })?;
            validate_private_directory_metadata(path, &metadata)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|error| {
                    ObscuraError::Installation {
                        operation: "protect private Obscura directory".to_owned(),
                        message: error.to_string(),
                    }
                })?;
            }
            Ok(())
        }
        Err(error) => Err(ObscuraError::Installation {
            operation: "inspect private Obscura directory".to_owned(),
            message: error.to_string(),
        }),
    }
}

fn validate_private_directory_metadata(
    path: &Path,
    metadata: &fs::Metadata,
) -> Result<(), ObscuraError> {
    if !metadata.is_dir() || metadata.file_type().is_symlink() || is_reparse_point(metadata) {
        return Err(ObscuraError::Integrity {
            message: format!(
                "private provider path is not a regular directory: {}",
                path.display()
            ),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
const fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn temporary_path(parent: &Path, prefix: &str, suffix: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{prefix}-{}-{id}.{suffix}", std::process::id()))
}

fn temporary_directory(parent: &Path, prefix: &str) -> Result<PathBuf, ObscuraError> {
    for _ in 0..MAX_TEMP_ATTEMPTS {
        let candidate = temporary_directory_path(parent, prefix);
        match fs::create_dir(&candidate) {
            Ok(()) => {
                ensure_private_directory(&candidate)?;
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(ObscuraError::Installation {
                    operation: "create Obscura staging directory".to_owned(),
                    message: error.to_string(),
                });
            }
        }
    }
    Err(ObscuraError::Installation {
        operation: "create Obscura staging directory".to_owned(),
        message: "could not allocate a unique temporary directory".to_owned(),
    })
}

fn temporary_directory_path(parent: &Path, prefix: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{prefix}-{}-{id}", std::process::id()))
}

const fn archive_extension(artifact: &ObscuraArtifact) -> &'static str {
    match artifact.archive_format {
        ArchiveFormat::TarGz => "tar.gz",
        ArchiveFormat::Zip => "zip",
    }
}

fn archive_error(operation: &str, error: impl std::fmt::Display) -> ObscuraError {
    ObscuraError::Integrity { message: format!("could not {operation} Obscura archive: {error}") }
}

fn hex_digest(digest: &[u8]) -> String {
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchiveFormat, ObscuraArtifact, ObscuraPaths, current_artifact, current_target};
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_directory(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let directory = std::env::temp_dir().join(format!(
            "aether-obscura-install-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    static TEST_ZIP_ARTIFACT: ObscuraArtifact = ObscuraArtifact {
        version: "test",
        target: "test-target",
        asset_name: "test.zip",
        url: "https://example.invalid/test.zip",
        archive_size: 1,
        sha256: "0000000000000000000000000000000000000000000000000000000000000000",
        version_output: "obscura test",
        mcp_protocol_version: "2024-11-05",
        archive_format: ArchiveFormat::Zip,
        binary_name: "obscura",
        worker_name: "obscura-worker",
        launch_args: &[],
        profile_argument: None,
    };

    fn test_zip_artifact() -> &'static ObscuraArtifact {
        &TEST_ZIP_ARTIFACT
    }

    #[test]
    fn static_manifest_has_no_latest_or_non_https_download() {
        match current_artifact() {
            Ok(artifact) => {
                assert_eq!(
                    artifact.archive_format,
                    if cfg!(windows) { ArchiveFormat::Zip } else { ArchiveFormat::TarGz }
                );
                assert!(artifact.url.starts_with("https://"));
                assert!(!artifact.url.contains("latest"));
                assert_eq!(artifact.sha256.len(), 64);
                assert!(artifact.archive_size <= MAX_ARCHIVE_BYTES);
                assert_eq!(artifact.version_output, "obscura 0.2.1");
                assert_eq!(artifact.mcp_protocol_version, crate::MCP_PROTOCOL_VERSION);
            }
            Err(ObscuraError::UnsupportedTarget { target }) => {
                assert_eq!(target, current_target());
            }
            Err(error) => panic!("unexpected static manifest error: {error}"),
        }
    }

    #[test]
    fn paths_keep_profile_outside_a_workspace_and_bound_component() {
        let artifact = test_zip_artifact();
        let paths =
            ObscuraPaths::for_workspace("/tmp/aether-state", "../../workspace id", artifact);
        assert_eq!(paths.profile_id(), "______workspace_id");
        assert!(paths.profile_dir().starts_with("/tmp/aether-state/obscura/profiles"));
    }

    #[test]
    fn zip_extraction_requires_exact_regular_binary_members() {
        let directory = test_directory("members");
        let archive_path = directory.join("provider.zip");
        let staging = directory.join("staging");
        fs::create_dir(&staging).unwrap();
        let artifact = test_zip_artifact();
        let mut archive = zip::ZipWriter::new(File::create(&archive_path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file("obscura", options).unwrap();
        archive.write_all(b"binary").unwrap();
        archive.start_file("obscura-worker", options).unwrap();
        archive.write_all(b"worker").unwrap();
        archive.finish().unwrap();

        extract_archive(&archive_path, &staging, artifact).unwrap();
        assert_eq!(fs::read(staging.join("obscura")).unwrap(), b"binary");
        assert_eq!(fs::read(staging.join("obscura-worker")).unwrap(), b"worker");
        let _ = fs::remove_dir_all(directory);
    }

    #[test]
    fn zip_extraction_rejects_unsafe_or_extra_members() {
        let directory = test_directory("unsafe");
        let archive_path = directory.join("provider.zip");
        let staging = directory.join("staging");
        fs::create_dir(&staging).unwrap();
        let artifact = test_zip_artifact();
        let mut archive = zip::ZipWriter::new(File::create(&archive_path).unwrap());
        let options = zip::write::SimpleFileOptions::default();
        archive.start_file("obscura", options).unwrap();
        archive.write_all(b"binary").unwrap();
        archive.start_file("../outside", options).unwrap();
        archive.write_all(b"unexpected").unwrap();
        archive.finish().unwrap();

        assert!(matches!(
            extract_archive(&archive_path, &staging, artifact),
            Err(ObscuraError::Integrity { .. })
        ));
        assert!(!directory.join("outside").exists());
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn version_validation_rejects_an_unexpected_output() {
        use std::os::unix::fs::PermissionsExt;

        let directory = test_directory("version");
        let script = directory.join("obscura");
        fs::write(&script, b"#!/bin/sh\nprintf 'obscura wrong\\n'\n").unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let output = run_version(&script, &directory).await.unwrap();
        let artifact = test_zip_artifact();
        let error = validate_version_output(&output, artifact).unwrap_err();
        assert!(matches!(error, ObscuraError::Integrity { .. }));
        let _ = fs::remove_dir_all(directory);
    }
}
