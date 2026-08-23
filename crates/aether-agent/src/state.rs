use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::session::SessionStoreError;

pub(crate) const STATE_DIR_ENV: &str = "AETHER_FX_STATE_DIR";
pub(crate) const STATE_ROOT_NAME: &str = "aether-fx";
pub(crate) const WORKSPACES_DIR: &str = "workspaces";
pub(crate) const SESSION_DIR: &str = "sessions";
pub(crate) const WORKSPACE_METADATA: &str = "workspace.json";
pub(crate) const WORKSPACE_METADATA_VERSION: u32 = 1;
pub(crate) const MAX_WORKSPACE_METADATA_BYTES: usize = 16 * 1024;

/// Resolve the private AETHER Fx application-state root without creating it.
pub(crate) fn resolve_state_root() -> Result<PathBuf, SessionStoreError> {
    if let Some(value) = env::var_os(STATE_DIR_ENV) {
        if value.is_empty() {
            return Err(SessionStoreError::Invalid(format!("{STATE_DIR_ENV} is set but empty")));
        }
        return absolute_path(PathBuf::from(value), STATE_DIR_ENV);
    }

    #[cfg(target_os = "macos")]
    {
        let home = required_env_path("HOME")?;
        Ok(home.join("Library").join("Application Support").join(STATE_ROOT_NAME))
    }

    #[cfg(windows)]
    {
        let local_app_data = required_env_path("LOCALAPPDATA")?;
        return Ok(local_app_data.join(STATE_ROOT_NAME));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let base = match env::var_os("XDG_STATE_HOME") {
            Some(value) if !value.is_empty() => {
                absolute_default_path(PathBuf::from(value), "XDG_STATE_HOME")?
            }
            Some(_) | None => required_env_path("HOME")?.join(".local").join("state"),
        };
        return Ok(base.join(STATE_ROOT_NAME));
    }

    #[cfg(not(any(unix, windows)))]
    {
        Err(SessionStoreError::Invalid(
            "cannot resolve AETHER Fx state directory on this platform".to_owned(),
        ))
    }
}

fn required_env_path(name: &str) -> Result<PathBuf, SessionStoreError> {
    let value = env::var_os(name).ok_or_else(|| {
        SessionStoreError::Invalid(format!(
            "cannot resolve AETHER Fx state directory: {name} is unavailable"
        ))
    })?;
    if value.is_empty() {
        return Err(SessionStoreError::Invalid(format!(
            "cannot resolve AETHER Fx state directory: {name} is empty"
        )));
    }
    absolute_default_path(PathBuf::from(value), name)
}

fn absolute_default_path(path: PathBuf, name: &str) -> Result<PathBuf, SessionStoreError> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(SessionStoreError::Invalid(format!(
            "cannot resolve AETHER Fx state directory: {name} must be absolute"
        )))
    }
}

fn absolute_path(path: PathBuf, name: &str) -> Result<PathBuf, SessionStoreError> {
    if path.is_absolute() {
        return Ok(path);
    }
    env::current_dir().map(|current| current.join(path)).map_err(|error| SessionStoreError::Io {
        operation: format!("resolve relative {name}"),
        message: error.to_string(),
    })
}

pub(crate) fn canonical_workspace(path: &Path) -> Result<PathBuf, SessionStoreError> {
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

pub(crate) fn native_path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        return path.as_os_str().encode_wide().flat_map(u16::to_ne_bytes).collect();
    }

    #[cfg(not(any(unix, windows)))]
    {
        path.as_os_str().to_str().map_or_else(Vec::new, |value| value.as_bytes().to_vec())
    }
}

pub(crate) fn native_path_hex(path: &Path) -> String {
    hex(&native_path_bytes(path))
}

pub(crate) fn workspace_id(canonical_workspace: &Path) -> String {
    blake3::hash(&native_path_bytes(canonical_workspace)).to_hex().to_string()
}

pub(crate) fn workspace_display(canonical_workspace: &Path) -> String {
    canonical_workspace.to_str().map_or_else(
        || format!("native:{}", native_path_hex(canonical_workspace)),
        ToOwned::to_owned,
    )
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        result.push(DIGITS[(byte >> 4) as usize] as char);
        result.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    result
}
