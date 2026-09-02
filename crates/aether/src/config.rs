use std::{
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use aether_agent::SessionStore;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_CONFIG_BYTES: usize = 64 * 1024;
const MAX_MODEL_BYTES: usize = 256;
const MAX_HELPER_ARGS: usize = 16;
const MAX_HELPER_ARG_BYTES: usize = 1024;
const MAX_HELPER_OUTPUT_BYTES: usize = 16 * 1024;
const CREDENTIAL_HELPER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("configuration I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("configuration is invalid: {0}")]
    Invalid(String),
    #[error(transparent)]
    Session(#[from] aether_agent::SessionStoreError),
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "AuthConfig::is_empty")]
    pub auth: AuthConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuthConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_helper: Option<Vec<String>>,
}

impl AuthConfig {
    fn is_empty(&self) -> bool {
        self.credential_helper.is_none()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LoadedConfig {
    pub path: PathBuf,
    pub config: ConfigFile,
}

pub(crate) fn load(root: &Path) -> Result<LoadedConfig, ConfigError> {
    let path = SessionStore::config_path(root)?;
    reject_config_path(&path)?;
    if !path.exists() {
        return Ok(LoadedConfig { path, config: ConfigFile::default() });
    }
    if let Some(parent) = path.parent() {
        restrict_directory(parent)?;
    }
    let mut file = OpenOptions::new().read(true).open(&path)?;
    restrict_file(&file)?;
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_CONFIG_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::Invalid("configuration exceeds 64 KiB".to_owned()));
    }
    let config: ConfigFile = serde_json::from_slice(&bytes)
        .map_err(|error| ConfigError::Invalid(format!("JSON parse failed: {error}")))?;
    validate(&config)?;
    Ok(LoadedConfig { path, config })
}

pub(crate) fn save(root: &Path, config: ConfigFile) -> Result<PathBuf, ConfigError> {
    validate(&config)?;
    let path = SessionStore::prepare_config_path(root)?;
    reject_config_path(&path)?;
    if let Some(parent) = path.parent() {
        restrict_directory(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(&config)
        .map_err(|error| ConfigError::Invalid(format!("configuration encode failed: {error}")))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::Invalid("configuration exceeds 64 KiB".to_owned()));
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let temporary =
        path.with_file_name(format!(".aether-fx-config-{}-{stamp}.tmp", std::process::id()));
    reject_config_path(&temporary)?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(&temporary)?;
    restrict_file(&file)?;
    let write_result = (|| {
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.flush()?;
        file.sync_all()
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Io(error));
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(ConfigError::Io(error));
    }
    reject_config_path(&path)?;
    Ok(path)
}

pub(crate) fn editor_command(root: &Path) -> Result<PathBuf, ConfigError> {
    let loaded = load(root)?;
    let path =
        if loaded.path.exists() { loaded.path.clone() } else { save(root, loaded.config.clone())? };
    let editor = env::var("VISUAL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| env::var("EDITOR").ok().filter(|value| !value.trim().is_empty()))
        .ok_or_else(|| {
            ConfigError::Invalid("set VISUAL or EDITOR to edit configuration".to_owned())
        })?;
    let command = parse_direct_command(&editor)?;
    let program = command
        .first()
        .ok_or_else(|| ConfigError::Invalid("editor command is empty".to_owned()))?;
    let status = Command::new(program).args(command.iter().skip(1)).arg(&path).status()?;
    if !status.success() {
        return Err(ConfigError::Invalid(format!("editor exited with {status}")));
    }
    let _ = load(root)?;
    Ok(path)
}

pub(crate) fn helper_key(config: &ConfigFile) -> Result<Option<String>, ConfigError> {
    let Some(command) = config.auth.credential_helper.as_ref() else {
        return Ok(None);
    };
    let program = command
        .first()
        .ok_or_else(|| ConfigError::Invalid("credential helper command is empty".to_owned()))?;
    let mut child = Command::new(program)
        .args(command.iter().skip(1))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| {
        ConfigError::Invalid("credential helper stdout was unavailable".to_owned())
    })?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader_thread = thread::spawn(move || {
        let mut stdout = stdout;
        let mut bytes = Vec::new();
        let result = Read::by_ref(&mut stdout)
            .take((MAX_HELPER_OUTPUT_BYTES as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let deadline = Instant::now() + CREDENTIAL_HELPER_TIMEOUT;
    let mut read_result = None;
    let mut status = None;
    let mut kill_sent = false;
    loop {
        if read_result.is_none() {
            match receiver.try_recv() {
                Ok(result) => read_result = Some(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    read_result =
                        Some(Err(io::Error::other("credential helper output reader stopped")))
                }
            }
        }
        if status.is_none() {
            status = match child.try_wait() {
                Ok(status) => status,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    if receiver.recv_timeout(Duration::from_secs(1)).is_ok() {
                        let _ = reader_thread.join();
                    }
                    return Err(ConfigError::Io(error));
                }
            };
        }
        let should_kill = read_result.as_ref().is_some_and(|result| {
            result.as_ref().is_ok_and(|bytes| bytes.len() > MAX_HELPER_OUTPUT_BYTES)
        }) || read_result.as_ref().is_some_and(Result::is_err);
        if should_kill && !kill_sent {
            let _ = child.kill();
            kill_sent = true;
        }
        if status.is_some() && read_result.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            if !kill_sent {
                let _ = child.kill();
            }
            let _ = child.wait();
            if receiver.recv_timeout(Duration::from_secs(1)).is_ok() {
                let _ = reader_thread.join();
            }
            return Err(ConfigError::Invalid(
                "credential helper timed out after 5 seconds".to_owned(),
            ));
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = reader_thread.join();
    let bytes = read_result
        .expect("credential helper output result is present")
        .map_err(ConfigError::Io)?;
    if bytes.len() > MAX_HELPER_OUTPUT_BYTES {
        return Err(ConfigError::Invalid("credential helper output exceeds 16 KiB".to_owned()));
    }
    let status = status.expect("credential helper exit status is present");
    if !status.success() {
        return Err(ConfigError::Invalid(format!("credential helper exited with {}", status)));
    }
    let value = String::from_utf8(bytes)
        .map_err(|_| ConfigError::Invalid("credential helper output is not UTF-8".to_owned()))?;
    let value = value.trim().to_owned();
    if value.is_empty()
        || value.len() > MAX_HELPER_OUTPUT_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(ConfigError::Invalid("credential helper returned an invalid key".to_owned()));
    }
    Ok(Some(value))
}

fn validate(config: &ConfigFile) -> Result<(), ConfigError> {
    if let Some(model) = config.default_model.as_deref()
        && (model.is_empty()
            || model.len() > MAX_MODEL_BYTES
            || model.chars().any(char::is_control)
            || model.chars().any(char::is_whitespace))
    {
        return Err(ConfigError::Invalid(
            "default_model must be non-empty, bounded, and free of controls".to_owned(),
        ));
    }
    if let Some(command) = config.auth.credential_helper.as_ref() {
        if command.is_empty() || command.len() > MAX_HELPER_ARGS {
            return Err(ConfigError::Invalid(
                "credential_helper must contain one to sixteen argv entries".to_owned(),
            ));
        }
        for argument in command {
            if argument.is_empty()
                || argument.len() > MAX_HELPER_ARG_BYTES
                || argument.chars().any(char::is_control)
            {
                return Err(ConfigError::Invalid(
                    "credential_helper arguments must be bounded and free of controls".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn parse_direct_command(value: &str) -> Result<Vec<String>, ConfigError> {
    if value.len() > 4096 || value.chars().any(char::is_control) {
        return Err(ConfigError::Invalid("editor command is invalid or too long".to_owned()));
    }
    let mut command = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut escaped = false;
    for character in value.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            continue;
        }
        match (quote, character) {
            (_, '\\') => escaped = true,
            (Some(expected), character) if character == expected => quote = None,
            (Some(_), character) => current.push(character),
            (None, '\'' | '"') => quote = Some(character),
            (None, character) if character.is_whitespace() => {
                if !current.is_empty() {
                    command.push(std::mem::take(&mut current));
                }
            }
            (None, character) => current.push(character),
        }
    }
    if escaped || quote.is_some() {
        return Err(ConfigError::Invalid("editor command has an unfinished quote".to_owned()));
    }
    if !current.is_empty() {
        command.push(current);
    }
    if command.is_empty() {
        return Err(ConfigError::Invalid("editor command is empty".to_owned()));
    }
    Ok(command)
}

fn reject_config_path(path: &Path) -> Result<(), ConfigError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(ConfigError::Invalid("configuration path is a symbolic link".to_owned()))
        }
        Ok(metadata) if !metadata.file_type().is_file() => {
            Err(ConfigError::Invalid("configuration path is not a regular file".to_owned()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ConfigError::Io(error)),
    }
}

fn restrict_directory(_path: &Path) -> Result<(), ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(_path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn restrict_file(_file: &File) -> Result<(), ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_command_parser_preserves_arguments_without_shell_expansion() {
        assert_eq!(
            parse_direct_command(r###"editor --title 'AETHER $HOME' "config file""###).unwrap(),
            vec![
                "editor".to_owned(),
                "--title".to_owned(),
                "AETHER $HOME".to_owned(),
                "config file".to_owned(),
            ]
        );
        assert!(parse_direct_command("editor 'unfinished").is_err());
        assert!(parse_direct_command("editor\u{0000}").is_err());
    }

    #[test]
    fn config_validation_rejects_ambiguous_model_and_unbounded_helper_entries() {
        let whitespace_model =
            ConfigFile { default_model: Some("model name".to_owned()), ..ConfigFile::default() };
        assert!(validate(&whitespace_model).is_err());

        let empty_helper = ConfigFile {
            auth: AuthConfig { credential_helper: Some(Vec::new()) },
            ..ConfigFile::default()
        };
        assert!(validate(&empty_helper).is_err());

        let long_argument = ConfigFile {
            auth: AuthConfig { credential_helper: Some(vec!["tool".to_owned(), "x".repeat(1025)]) },
            ..ConfigFile::default()
        };
        assert!(validate(&long_argument).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn credential_helper_reads_bounded_stdout_without_printing_it() {
        let config = ConfigFile {
            auth: AuthConfig {
                credential_helper: Some(vec![
                    "/usr/bin/printf".to_owned(),
                    "helper-key\\n".to_owned(),
                ]),
            },
            ..ConfigFile::default()
        };
        assert_eq!(helper_key(&config).unwrap(), Some("helper-key".to_owned()));
    }
}
