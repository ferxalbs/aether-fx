use std::{
    cmp::Ordering,
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering as AtomicOrdering},
    time::Duration,
};

#[cfg(windows)]
use std::{
    process::{Command as ProcessCommand, ExitCode},
    time::Instant,
};

use aether_tools::replace_existing_file;
use reqwest::{Client, Response, StatusCode, header::ACCEPT};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    fs as tokio_fs,
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
};

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const UPDATE_SOURCE: &str = "github.com/ferxalbs/aether-fx";
const RELEASES_URL: &str = "https://api.github.com/repos/ferxalbs/aether-fx/releases";
const MAX_METADATA_BYTES: usize = 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_VERSION_OUTPUT_BYTES: usize = 4096;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const RETRY_BACKOFF: Duration = Duration::from_millis(200);
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(windows)]
const WINDOWS_HANDOFF_TIMEOUT: Duration = Duration::from_secs(30);
const RELEASES_PER_PAGE: usize = 100;
const MAX_RELEASE_PAGES: usize = 32;
const MAX_TEMP_ATTEMPTS: usize = 32;
const MAX_STALE_TEMP_REMOVALS: usize = 256;
const MAX_RETRIES: usize = 1;

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ErrorCode {
    Network,
    Release,
    Asset,
    Checksum,
    Version,
    Platform,
    Permissions,
    Locked,
    Replacement,
    Cleanup,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Release => "release",
            Self::Asset => "asset",
            Self::Checksum => "checksum",
            Self::Version => "version",
            Self::Platform => "platform",
            Self::Permissions => "permissions",
            Self::Locked => "locked",
            Self::Replacement => "replacement",
            Self::Cleanup => "cleanup",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug)]
pub(crate) struct UpdateError {
    pub(crate) code: ErrorCode,
    message: String,
}

impl UpdateError {
    fn new(code: ErrorCode, message: impl AsRef<str>) -> Self {
        Self { code, message: sanitize_message(message.as_ref()) }
    }

    fn from_io(code: ErrorCode, context: &str, error: &io::Error) -> Self {
        Self::new(code, format!("{context}: {error}"))
    }

    pub(crate) fn json_line(&self) -> String {
        serde_json::json!({
            "current_version": CURRENT_VERSION,
            "latest_version": serde_json::Value::Null,
            "updated": false,
            "platform": platform_name(),
            "path": serde_json::Value::Null,
            "error": {
                "code": self.code.as_str(),
                "message": self.message,
            },
        })
        .to_string()
    }
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "update failed ({}): {}", self.code, self.message)
    }
}

impl std::error::Error for UpdateError {}

pub(crate) struct UpdateResult {
    latest_version: String,
    updated: bool,
    path: String,
}

impl UpdateResult {
    pub(crate) fn json_line(&self) -> String {
        serde_json::json!({
            "current_version": CURRENT_VERSION,
            "latest_version": self.latest_version,
            "updated": self.updated,
            "platform": platform_name(),
            "path": self.path,
        })
        .to_string()
    }
}

#[derive(Debug)]
struct DestinationInfo {
    permissions: fs::Permissions,
    length: u64,
    permission_key: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DestinationFailureKind {
    Platform,
    Permissions,
    Replacement,
}

#[derive(Debug)]
struct DestinationFailure {
    kind: DestinationFailureKind,
    message: String,
}

impl DestinationFailure {
    fn new(kind: DestinationFailureKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }

    fn update_error(self) -> UpdateError {
        let code = match self.kind {
            DestinationFailureKind::Platform => ErrorCode::Platform,
            DestinationFailureKind::Permissions => ErrorCode::Permissions,
            DestinationFailureKind::Replacement => ErrorCode::Replacement,
        };
        UpdateError::new(code, self.message)
    }
}

#[derive(Debug)]
struct FileIdentity {
    length: u64,
    sha256: [u8; 32],
}

#[derive(Debug)]
struct TemporaryFile {
    path: PathBuf,
    retain_on_drop: bool,
}

impl TemporaryFile {
    fn retain(&mut self) {
        self.retain_on_drop = true;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.retain_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug)]
struct UpdateLock {
    file: File,
    path: PathBuf,
    retain_path: bool,
}

impl UpdateLock {
    fn acquire(path: &Path) -> Result<Self, UpdateError> {
        Self::try_acquire(path)
    }

    fn try_acquire(path: &Path) -> Result<Self, UpdateError> {
        validate_lock_path(path)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(|error| {
                if error.kind() == io::ErrorKind::PermissionDenied {
                    UpdateError::from_io(
                        ErrorCode::Permissions,
                        "unable to open update lock",
                        &error,
                    )
                } else {
                    UpdateError::from_io(
                        ErrorCode::Replacement,
                        "unable to open update lock",
                        &error,
                    )
                }
            })?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(UpdateError::new(
                    ErrorCode::Locked,
                    "another update is already running",
                ));
            }
            Err(TryLockError::Error(error)) => {
                return Err(UpdateError::from_io(
                    ErrorCode::Locked,
                    "unable to acquire update lock",
                    &error,
                ));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
        }
        Ok(Self { file, path: path.to_owned(), retain_path: false })
    }

    #[cfg(windows)]
    fn retain_path(&mut self) {
        self.retain_path = true;
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
        if !self.retain_path {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug)]
struct ParsedRelease {
    tag_name: String,
    version: Version,
    assets: Vec<GithubAsset>,
}

pub(crate) async fn run(json_output: bool) -> Result<UpdateResult, UpdateError> {
    if !json_output {
        println!("AETHER Fx current version: {CURRENT_VERSION}");
    }

    let destination = std::env::current_exe().map_err(|error| {
        UpdateError::from_io(ErrorCode::Replacement, "unable to resolve current executable", &error)
    })?;
    let destination_info =
        inspect_destination(&destination).map_err(DestinationFailure::update_error)?;
    if !json_output {
        println!("Checking for updates...");
    }

    let lock_path = lock_path(&destination)?;
    #[cfg(windows)]
    let mut lock = UpdateLock::acquire(&lock_path)?;
    #[cfg(not(windows))]
    let _lock = UpdateLock::acquire(&lock_path)?;
    cleanup_stale_temps(&destination)?;

    let original_identity = file_identity(&destination).await?;
    if original_identity.length != destination_info.length {
        return Err(UpdateError::new(
            ErrorCode::Replacement,
            "current executable changed while preparing the update",
        ));
    }
    let current_version = Version::parse(CURRENT_VERSION).map_err(|error| {
        UpdateError::new(ErrorCode::Version, format!("current package version is invalid: {error}"))
    })?;
    let client = build_client()?;
    let latest = fetch_latest_release(&client).await?;

    if latest.version.cmp_precedence(&current_version) != Ordering::Greater {
        if !json_output {
            println!("No update available.");
        }
        return Ok(UpdateResult {
            latest_version: latest.version.to_string(),
            updated: false,
            path: display_path(&destination),
        });
    }

    let platform = platform_name().ok_or_else(|| {
        UpdateError::new(
            ErrorCode::Platform,
            "the current target is not supported by AETHER releases",
        )
    })?;
    let asset_name = direct_asset_name(&latest.tag_name, platform);
    let asset = unique_asset(&latest.assets, &asset_name).ok_or_else(|| {
        UpdateError::new(
            ErrorCode::Asset,
            format!("release has no unique asset named {asset_name}"),
        )
    })?;
    validate_asset(asset, ErrorCode::Asset, "binary asset")?;
    validate_announced_size(
        asset.size,
        MAX_BINARY_BYTES,
        ErrorCode::Asset,
        "binary asset exceeds the 128 MiB limit",
    )?;

    let checksum_asset = unique_asset(&latest.assets, "SHA256SUMS").ok_or_else(|| {
        UpdateError::new(ErrorCode::Checksum, "release has no unique SHA256SUMS asset")
    })?;
    validate_asset(checksum_asset, ErrorCode::Checksum, "checksum asset")?;
    validate_announced_size(
        checksum_asset.size,
        MAX_METADATA_BYTES as u64,
        ErrorCode::Checksum,
        "SHA256SUMS exceeds the 1 MiB limit",
    )?;

    if !json_output {
        println!("Update available: {}", latest.version);
        println!("Downloading and verifying...");
    }
    let checksum_url = release_asset_url(&latest.tag_name, "SHA256SUMS");
    let checksum_body = fetch_bounded(
        &client,
        &checksum_url,
        MAX_METADATA_BYTES,
        checksum_asset.size,
        ErrorCode::Checksum,
        "SHA256SUMS",
    )
    .await?;
    let expected_hash = checksum_for_asset(&checksum_body, &asset_name)?;

    let binary_url = release_asset_url(&latest.tag_name, &asset_name);
    let downloaded = download_binary(
        &client,
        &binary_url,
        asset.size,
        expected_hash,
        &destination_info.permissions,
        &destination,
    )
    .await?;
    validate_temporary_path(&downloaded.temporary.path)?;
    verify_staged_version(&downloaded.temporary.path, &latest.version.to_string()).await?;
    let staged_identity = file_identity(&downloaded.temporary.path).await?;
    if staged_identity.length != downloaded.identity.length
        || staged_identity.sha256 != downloaded.identity.sha256
    {
        return Err(UpdateError::new(
            ErrorCode::Checksum,
            "verified update temporary changed before replacement",
        ));
    }
    revalidate_original(&destination, &destination_info, &original_identity).await?;

    #[cfg(windows)]
    let handoff = {
        let result = handoff_windows(
            &destination,
            &destination_info,
            &original_identity,
            downloaded,
            &mut lock,
        )
        .await;
        if result.is_ok() {
            if !json_output {
                println!(
                    "Update handed off to the Windows helper. Restart AETHER Fx to confirm the new version."
                );
            }
        }
        result.map(|()| true)
    }?;

    #[cfg(not(windows))]
    let handoff = {
        let mut downloaded = downloaded;
        replace_existing_file(&destination, &downloaded.temporary.path).map_err(|error| {
            UpdateError::from_io(
                ErrorCode::Replacement,
                "atomic executable replacement failed",
                &error,
            )
        })?;
        downloaded.temporary.retain();
        sync_parent_directory(&destination);
        if !json_output {
            println!("Updated successfully. Restart AETHER Fx to use the new version.");
        }
        true
    };

    let _ = handoff;
    Ok(UpdateResult {
        latest_version: latest.version.to_string(),
        updated: true,
        path: display_path(&destination),
    })
}

#[cfg(windows)]
pub(crate) fn maybe_run_windows_helper() -> Option<ExitCode> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next()?;
    if first != std::ffi::OsStr::new(WINDOWS_HELPER_FLAG) {
        return None;
    }
    let values = arguments.collect::<Vec<_>>();
    let result = (|| {
        if values.len() != 6 {
            return Err(UpdateError::new(ErrorCode::Replacement, "invalid Windows update handoff"));
        }
        let destination = PathBuf::from(&values[0]);
        let replacement = PathBuf::from(&values[1]);
        let original_hash = parse_hash_argument(&values[2])?;
        let original_size = parse_size_argument(&values[3])?;
        let replacement_hash = parse_hash_argument(&values[4])?;
        let replacement_size = parse_size_argument(&values[5])?;
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(
            |error| {
                UpdateError::new(ErrorCode::Replacement, format!("helper runtime failed: {error}"))
            },
        )?;
        runtime.block_on(run_windows_helper(
            destination,
            replacement,
            FileIdentity { length: original_size, sha256: original_hash },
            FileIdentity { length: replacement_size, sha256: replacement_hash },
        ))
    })();
    Some(match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("AETHER Fx: {error}");
            ExitCode::FAILURE
        }
    })
}

pub(crate) fn local_status_json() -> serde_json::Value {
    let executable = std::env::current_exe().ok();
    let (supported, reason) = match executable.as_deref() {
        None => (false, Some("unable to resolve current executable".to_owned())),
        Some(path) => match inspect_destination(path) {
            Ok(_) => (true, None),
            Err(error) => (false, Some(sanitize_message(&error.message))),
        },
    };
    serde_json::json!({
        "source": UPDATE_SOURCE,
        "current_executable": executable.as_deref().map(display_path),
        "supported": supported,
        "reason": reason,
    })
}

fn build_client() -> Result<Client, UpdateError> {
    Client::builder()
        .user_agent(format!("aether-fx/{CURRENT_VERSION}"))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(REQUEST_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .referer(false)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 10 {
                attempt.error("too many redirects")
            } else if attempt.url().scheme() == "https" {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .build()
        .map_err(|error| {
            UpdateError::new(
                ErrorCode::Network,
                format!("HTTPS client initialization failed: {error}"),
            )
        })
}

async fn fetch_latest_release(client: &Client) -> Result<ParsedRelease, UpdateError> {
    let mut candidates = Vec::new();
    for page in 1..=MAX_RELEASE_PAGES {
        let url = format!("{RELEASES_URL}?per_page={RELEASES_PER_PAGE}&page={page}");
        let body = fetch_bounded(
            client,
            &url,
            MAX_METADATA_BYTES,
            None,
            ErrorCode::Release,
            "GitHub release metadata",
        )
        .await?;
        let releases: Vec<GithubRelease> = serde_json::from_slice(&body).map_err(|error| {
            UpdateError::new(
                ErrorCode::Release,
                format!("GitHub release metadata is invalid: {error}"),
            )
        })?;
        let count = releases.len();
        candidates.extend(releases.into_iter().filter_map(parse_public_release));
        if count < RELEASES_PER_PAGE {
            break;
        }
        if page == MAX_RELEASE_PAGES {
            return Err(UpdateError::new(
                ErrorCode::Release,
                "GitHub release history exceeds the bounded pagination limit",
            ));
        }
    }
    select_latest_release(candidates)
        .ok_or_else(|| UpdateError::new(ErrorCode::Release, "no public SemVer release was found"))
}

async fn fetch_bounded(
    client: &Client,
    url: &str,
    max_bytes: usize,
    expected_length: Option<u64>,
    category: ErrorCode,
    label: &str,
) -> Result<Vec<u8>, UpdateError> {
    let mut response = send_with_retry(client, url, category, label).await?;
    if response.content_length().is_some_and(|length| length > max_bytes as u64) {
        return Err(UpdateError::new(category, format!("{label} exceeds its size limit")));
    }
    let advertised_length = response.content_length().or(expected_length);
    let capacity =
        advertised_length.unwrap_or(0).min(max_bytes as u64).try_into().unwrap_or(max_bytes);
    let mut body = Vec::with_capacity(capacity);
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        UpdateError::new(ErrorCode::Network, format!("{label} download failed: {error}"))
    })? {
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(UpdateError::new(category, format!("{label} exceeds its size limit")));
        }
        body.extend_from_slice(&chunk);
    }
    if let Some(length) = advertised_length
        && length != body.len() as u64
    {
        return Err(UpdateError::new(category, format!("{label} response was truncated")));
    }
    if let Some(expected) = expected_length
        && expected != body.len() as u64
    {
        return Err(UpdateError::new(
            category,
            format!("{label} metadata size did not match the response"),
        ));
    }
    Ok(body)
}

async fn send_with_retry(
    client: &Client,
    url: &str,
    category: ErrorCode,
    label: &str,
) -> Result<Response, UpdateError> {
    for attempt in 0..=MAX_RETRIES {
        let response = client.get(url).header(ACCEPT, "application/vnd.github+json").send().await;
        match response {
            Ok(response) if response.status().is_success() => return Ok(response),
            Ok(response) if attempt < MAX_RETRIES && transient_status(response.status()) => {
                drop(response);
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
            Ok(response) => {
                let status = response.status();
                drop(response);
                return Err(UpdateError::new(category, format!("{label} returned HTTP {status}")));
            }
            Err(error) if attempt < MAX_RETRIES && (error.is_connect() || error.is_timeout()) => {
                tokio::time::sleep(RETRY_BACKOFF).await;
            }
            Err(error) => {
                return Err(UpdateError::new(
                    ErrorCode::Network,
                    format!("{label} request failed: {error}"),
                ));
            }
        }
    }
    Err(UpdateError::new(ErrorCode::Network, format!("{label} request failed")))
}

fn transient_status(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 425 | 429 | 500 | 502 | 503 | 504)
}

fn parse_public_release(release: GithubRelease) -> Option<ParsedRelease> {
    if release.draft {
        return None;
    }
    let version =
        release.tag_name.strip_prefix('v').and_then(|value| Version::parse(value).ok())?;
    Some(ParsedRelease { tag_name: release.tag_name, version, assets: release.assets })
}

fn select_latest_release(releases: Vec<ParsedRelease>) -> Option<ParsedRelease> {
    releases.into_iter().max_by(|left, right| {
        left.version.cmp_precedence(&right.version).then_with(|| left.tag_name.cmp(&right.tag_name))
    })
}

fn unique_asset<'a>(assets: &'a [GithubAsset], name: &str) -> Option<&'a GithubAsset> {
    let mut matches = assets.iter().filter(|asset| asset.name == name);
    let first = matches.next()?;
    matches.next().is_none().then_some(first)
}

fn validate_asset(
    asset: &GithubAsset,
    category: ErrorCode,
    label: &str,
) -> Result<(), UpdateError> {
    if asset.state.as_deref().is_some_and(|state| state != "uploaded") {
        return Err(UpdateError::new(category, format!("{label} is not uploaded")));
    }
    Ok(())
}

fn validate_announced_size(
    size: Option<u64>,
    limit: u64,
    code: ErrorCode,
    message: &str,
) -> Result<(), UpdateError> {
    if size.is_some_and(|size| size > limit) {
        return Err(UpdateError::new(code, message));
    }
    Ok(())
}

fn checksum_for_asset(body: &[u8], asset_name: &str) -> Result<[u8; 32], UpdateError> {
    let text = std::str::from_utf8(body).map_err(|error| {
        UpdateError::new(ErrorCode::Checksum, format!("SHA256SUMS is not UTF-8: {error}"))
    })?;
    let mut found = None;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        if fields.len() != 2 {
            return Err(UpdateError::new(
                ErrorCode::Checksum,
                "SHA256SUMS contains a malformed line",
            ));
        }
        let digest = decode_hash(fields[0])?;
        if fields[1] == asset_name {
            if found.is_some() {
                return Err(UpdateError::new(
                    ErrorCode::Checksum,
                    "SHA256SUMS contains duplicate asset entries",
                ));
            }
            found = Some(digest);
        }
    }
    found.ok_or_else(|| {
        UpdateError::new(ErrorCode::Checksum, "SHA256SUMS has no exact entry for the binary asset")
    })
}

fn decode_hash(value: &str) -> Result<[u8; 32], UpdateError> {
    let bytes = value.as_bytes();
    if bytes.len() != 64 {
        return Err(UpdateError::new(
            ErrorCode::Checksum,
            "SHA256SUMS contains an invalid SHA-256 digest",
        ));
    }
    let mut digest = [0_u8; 32];
    for (index, pair) in bytes.as_chunks::<2>().0.iter().enumerate() {
        let high = hex_digit(pair[0]).ok_or_else(|| {
            UpdateError::new(ErrorCode::Checksum, "SHA256SUMS contains an invalid SHA-256 digest")
        })?;
        let low = hex_digit(pair[1]).ok_or_else(|| {
            UpdateError::new(ErrorCode::Checksum, "SHA256SUMS contains an invalid SHA-256 digest")
        })?;
        digest[index] = (high << 4) | low;
    }
    Ok(digest)
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

struct DownloadedBinary {
    temporary: TemporaryFile,
    identity: FileIdentity,
}

async fn download_binary(
    client: &Client,
    url: &str,
    expected_length: Option<u64>,
    expected_hash: [u8; 32],
    permissions: &fs::Permissions,
    destination: &Path,
) -> Result<DownloadedBinary, UpdateError> {
    let mut response = send_with_retry(client, url, ErrorCode::Asset, "binary asset").await?;
    let response_length = response.content_length().or(expected_length);
    validate_announced_size(
        response_length,
        MAX_BINARY_BYTES,
        ErrorCode::Asset,
        "binary asset exceeds the 128 MiB limit",
    )?;

    let (mut temporary, mut file) = create_temporary_file(destination).await?;
    let mut hasher = Sha256::new();
    let mut length = 0_u64;
    let result = async {
        while let Some(chunk) = response.chunk().await.map_err(|error| {
            UpdateError::new(ErrorCode::Network, format!("binary asset download failed: {error}"))
        })? {
            let next = length.saturating_add(chunk.len() as u64);
            if next > MAX_BINARY_BYTES {
                return Err(UpdateError::new(
                    ErrorCode::Asset,
                    "binary asset exceeds the 128 MiB limit",
                ));
            }
            file.write_all(&chunk).await.map_err(|error| {
                UpdateError::from_io(
                    ErrorCode::Replacement,
                    "unable to write update temporary",
                    &error,
                )
            })?;
            hasher.update(&chunk);
            length = next;
        }
        if let Some(advertised) = response_length
            && advertised != length
        {
            return Err(UpdateError::new(
                ErrorCode::Network,
                "binary asset response was truncated",
            ));
        }
        if let Some(metadata_length) = expected_length
            && metadata_length != length
        {
            return Err(UpdateError::new(
                ErrorCode::Asset,
                "binary asset metadata size did not match the download",
            ));
        }
        let digest: [u8; 32] = hasher.finalize().into();
        if digest != expected_hash {
            return Err(UpdateError::new(ErrorCode::Checksum, "checksum verification failed"));
        }
        file.flush().await.map_err(|error| {
            UpdateError::from_io(ErrorCode::Replacement, "unable to flush update temporary", &error)
        })?;
        tokio_fs::set_permissions(&temporary.path, permissions.clone()).await.map_err(|error| {
            UpdateError::from_io(
                ErrorCode::Permissions,
                "unable to preserve executable permissions",
                &error,
            )
        })?;
        file.sync_all().await.map_err(|error| {
            UpdateError::from_io(ErrorCode::Replacement, "unable to sync update temporary", &error)
        })?;
        Ok(FileIdentity { length, sha256: digest })
    }
    .await;
    match result {
        Ok(identity) => {
            temporary.retain_on_drop = false;
            Ok(DownloadedBinary { temporary, identity })
        }
        Err(error) => Err(error),
    }
}

async fn create_temporary_file(
    destination: &Path,
) -> Result<(TemporaryFile, tokio_fs::File), UpdateError> {
    let prefix = temp_prefix(destination)?;
    for _ in 0..MAX_TEMP_ATTEMPTS {
        let candidate = next_temp_path(destination, &prefix);
        match tokio_fs::OpenOptions::new().write(true).create_new(true).open(&candidate).await {
            Ok(file) => {
                #[cfg(unix)]
                if let Err(error) = tokio_fs::set_permissions(&candidate, {
                    use std::os::unix::fs::PermissionsExt;
                    fs::Permissions::from_mode(0o600)
                })
                .await
                {
                    drop(file);
                    let _ = fs::remove_file(&candidate);
                    return Err(UpdateError::from_io(
                        ErrorCode::Permissions,
                        "unable to protect update temporary",
                        &error,
                    ));
                }
                return Ok((TemporaryFile { path: candidate, retain_on_drop: false }, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err(UpdateError::from_io(
                    ErrorCode::Permissions,
                    "unable to create update temporary",
                    &error,
                ));
            }
            Err(error) => {
                return Err(UpdateError::from_io(
                    ErrorCode::Replacement,
                    "unable to create update temporary",
                    &error,
                ));
            }
        }
    }
    Err(UpdateError::new(
        ErrorCode::Replacement,
        "unable to allocate a collision-free update temporary",
    ))
}

async fn file_identity(path: &Path) -> Result<FileIdentity, UpdateError> {
    let mut file = tokio_fs::File::open(path).await.map_err(|error| {
        UpdateError::from_io(ErrorCode::Replacement, "unable to read executable identity", &error)
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut length = 0_u64;
    loop {
        let count = file.read(&mut buffer).await.map_err(|error| {
            UpdateError::from_io(
                ErrorCode::Replacement,
                "unable to read executable identity",
                &error,
            )
        })?;
        if count == 0 {
            break;
        }
        length = length.saturating_add(count as u64);
        if length > MAX_BINARY_BYTES {
            return Err(UpdateError::new(
                ErrorCode::Replacement,
                "current executable exceeds the 128 MiB limit",
            ));
        }
        hasher.update(&buffer[..count]);
    }
    Ok(FileIdentity { length, sha256: hasher.finalize().into() })
}

async fn revalidate_original(
    destination: &Path,
    expected_info: &DestinationInfo,
    expected_identity: &FileIdentity,
) -> Result<(), UpdateError> {
    let current_info =
        inspect_destination(destination).map_err(DestinationFailure::update_error)?;
    if current_info.length != expected_info.length
        || current_info.permission_key != expected_info.permission_key
    {
        return Err(UpdateError::new(
            ErrorCode::Replacement,
            "current executable metadata changed during the update",
        ));
    }
    let current_identity = file_identity(destination).await?;
    if current_identity.length != expected_identity.length
        || current_identity.sha256 != expected_identity.sha256
    {
        return Err(UpdateError::new(
            ErrorCode::Replacement,
            "current executable changed during the update",
        ));
    }
    Ok(())
}

fn validate_temporary_path(path: &Path) -> Result<(), UpdateError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        UpdateError::from_io(ErrorCode::Replacement, "unable to inspect update temporary", &error)
    })?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) || !metadata.is_file() {
        return Err(UpdateError::new(
            ErrorCode::Replacement,
            "update temporary is not a regular file",
        ));
    }
    Ok(())
}

fn inspect_destination(path: &Path) -> Result<DestinationInfo, DestinationFailure> {
    if platform_name().is_none() {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Platform,
            "the current target is not supported by AETHER releases",
        ));
    }
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        let kind = if error.kind() == io::ErrorKind::PermissionDenied {
            DestinationFailureKind::Permissions
        } else {
            DestinationFailureKind::Replacement
        };
        DestinationFailure::new(kind, format!("unable to inspect executable: {error}"))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Permissions,
            "the current executable is a symbolic link",
        ));
    }
    if is_reparse_point(&metadata) {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Permissions,
            "the current executable is a reparse point",
        ));
    }
    if !metadata.is_file() {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Replacement,
            "the current executable is not a regular file",
        ));
    }
    if !metadata_is_writable(&metadata) {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Permissions,
            "the current executable is read-only",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        DestinationFailure::new(
            DestinationFailureKind::Permissions,
            "the executable has no writable parent directory",
        )
    })?;
    let parent_metadata = fs::symlink_metadata(parent).map_err(|error| {
        DestinationFailure::new(
            DestinationFailureKind::Permissions,
            format!("unable to inspect executable directory: {error}"),
        )
    })?;
    if parent_metadata.file_type().is_symlink() || is_reparse_point(&parent_metadata) {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Permissions,
            "the executable directory is a symbolic link or reparse point",
        ));
    }
    if !parent_metadata.is_dir() {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Permissions,
            "the executable parent is not a directory",
        ));
    }
    if !metadata_is_writable(&parent_metadata) {
        return Err(DestinationFailure::new(
            DestinationFailureKind::Permissions,
            "the executable directory has no write permission",
        ));
    }
    Ok(DestinationInfo {
        permissions: metadata.permissions(),
        length: metadata.len(),
        permission_key: permission_key(&metadata),
    })
}

fn validate_lock_path(path: &Path) -> Result<(), UpdateError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || is_reparse_point(&metadata) => {
            Err(UpdateError::new(
                ErrorCode::Permissions,
                "the update lock path is a link or reparse point",
            ))
        }
        Ok(metadata) if !metadata.is_file() => Err(UpdateError::new(
            ErrorCode::Permissions,
            "the update lock path is not a regular file",
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(UpdateError::from_io(
            ErrorCode::Permissions,
            "unable to inspect update lock",
            &error,
        )),
    }
}

fn cleanup_stale_temps(destination: &Path) -> Result<(), UpdateError> {
    let parent = destination.parent().ok_or_else(|| {
        UpdateError::new(ErrorCode::Cleanup, "the executable has no temporary-file directory")
    })?;
    let prefix = temp_prefix(destination)?;
    let entries = fs::read_dir(parent).map_err(|error| {
        UpdateError::from_io(ErrorCode::Cleanup, "unable to scan update temporaries", &error)
    })?;
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(|error| {
            UpdateError::from_io(ErrorCode::Cleanup, "unable to inspect update temporary", &error)
        })?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(temp_extension()) {
            continue;
        }
        if removed >= MAX_STALE_TEMP_REMOVALS {
            return Err(UpdateError::new(ErrorCode::Cleanup, "too many stale update temporaries"));
        }
        let metadata = fs::symlink_metadata(entry.path()).map_err(|error| {
            UpdateError::from_io(
                ErrorCode::Cleanup,
                "unable to inspect stale update temporary",
                &error,
            )
        })?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            continue;
        }
        fs::remove_file(entry.path()).map_err(|error| {
            UpdateError::from_io(
                ErrorCode::Cleanup,
                "unable to remove stale update temporary",
                &error,
            )
        })?;
        removed += 1;
    }
    Ok(())
}

fn lock_path(destination: &Path) -> Result<PathBuf, UpdateError> {
    let name = destination
        .file_name()
        .map(|value| value.to_string_lossy())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| UpdateError::new(ErrorCode::Replacement, "executable name is empty"))?;
    Ok(destination.with_file_name(format!(".aether-fx-update-{name}.lock")))
}

fn temp_prefix(destination: &Path) -> Result<String, UpdateError> {
    let name = destination
        .file_name()
        .map(|value| value.to_string_lossy())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| UpdateError::new(ErrorCode::Replacement, "executable name is empty"))?;
    Ok(format!(".aether-fx-update-{name}-"))
}

fn next_temp_path(destination: &Path, prefix: &str) -> PathBuf {
    let id = NEXT_TEMP_ID.fetch_add(1, AtomicOrdering::Relaxed);
    let pid = std::process::id();
    destination.with_file_name(format!("{prefix}{pid}-{id}{}", temp_extension()))
}

fn temp_extension() -> &'static str {
    if cfg!(windows) { ".exe" } else { ".tmp" }
}

fn release_asset_url(tag: &str, asset: &str) -> String {
    format!("https://github.com/ferxalbs/aether-fx/releases/download/{tag}/{asset}")
}

fn direct_asset_name(tag: &str, platform: &str) -> String {
    let extension = if cfg!(windows) { ".exe" } else { "" };
    format!("aether-{tag}-{platform}{extension}")
}

fn display_path(path: &Path) -> String {
    sanitize_message(&path.to_string_lossy())
}

fn sanitize_message(value: &str) -> String {
    value
        .chars()
        .map(|character| if character.is_control() { '�' } else { character })
        .take(512)
        .collect()
}

#[cfg(unix)]
fn permission_key(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() as u64
}

#[cfg(windows)]
fn permission_key(metadata: &fs::Metadata) -> u64 {
    use std::os::windows::fs::MetadataExt;
    u64::from(metadata.file_attributes() & 0x1)
}

#[cfg(not(any(unix, windows)))]
fn permission_key(metadata: &fs::Metadata) -> u64 {
    u64::from(metadata.permissions().readonly())
}

#[cfg(unix)]
fn metadata_is_writable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o222 != 0
}

#[cfg(not(unix))]
fn metadata_is_writable(metadata: &fs::Metadata) -> bool {
    !metadata.permissions().readonly()
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_REPARSE_POINT is the standard Windows attribute used for links and junctions.
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn platform_name() -> Option<&'static str> {
    Some("macos-x86_64")
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn platform_name() -> Option<&'static str> {
    Some("macos-aarch64")
}

#[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
fn platform_name() -> Option<&'static str> {
    Some("linux-x86_64-gnu")
}

#[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
fn platform_name() -> Option<&'static str> {
    Some("linux-aarch64-gnu")
}

#[cfg(all(target_os = "windows", target_arch = "x86_64"))]
fn platform_name() -> Option<&'static str> {
    Some("windows-x86_64")
}

#[cfg(all(target_os = "windows", target_arch = "aarch64"))]
fn platform_name() -> Option<&'static str> {
    Some("windows-aarch64")
}

#[cfg(not(any(
    all(target_os = "macos", target_arch = "x86_64"),
    all(target_os = "macos", target_arch = "aarch64"),
    all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
    all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
    all(target_os = "windows", target_arch = "x86_64"),
    all(target_os = "windows", target_arch = "aarch64")
)))]
fn platform_name() -> Option<&'static str> {
    None
}

async fn verify_staged_version(path: &Path, expected: &str) -> Result<(), UpdateError> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| {
            UpdateError::from_io(ErrorCode::Version, "unable to execute downloaded binary", &error)
        })?;
    let stdout = child.stdout.take().ok_or_else(|| {
        UpdateError::new(ErrorCode::Version, "downloaded binary stdout was not available")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        UpdateError::new(ErrorCode::Version, "downloaded binary stderr was not available")
    })?;
    let result = tokio::time::timeout(VERSION_CHECK_TIMEOUT, async {
        let stdout_result = read_bounded(stdout, MAX_VERSION_OUTPUT_BYTES);
        let stderr_result = read_bounded(stderr, MAX_VERSION_OUTPUT_BYTES);
        let status_result = child.wait();
        tokio::join!(stdout_result, stderr_result, status_result)
    })
    .await;
    let (stdout, stderr, status) = match result {
        Ok(result) => result,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(UpdateError::new(
                ErrorCode::Version,
                "downloaded binary version check timed out",
            ));
        }
    };
    let stdout = stdout.map_err(|error| {
        UpdateError::from_io(
            ErrorCode::Version,
            "downloaded binary version output exceeded its limit",
            &error,
        )
    })?;
    let _stderr = stderr.map_err(|error| {
        UpdateError::from_io(
            ErrorCode::Version,
            "downloaded binary error output exceeded its limit",
            &error,
        )
    })?;
    let status = status.map_err(|error| {
        UpdateError::from_io(ErrorCode::Version, "downloaded binary version check failed", &error)
    })?;
    if !status.success() || !exact_version_output(&stdout, expected) {
        return Err(UpdateError::new(
            ErrorCode::Version,
            "downloaded binary did not report the release version exactly",
        ));
    }
    Ok(())
}

async fn read_bounded<R: AsyncRead + Unpin>(mut reader: R, limit: usize) -> io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit.min(1024));
    let mut buffer = [0_u8; 1024];
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            return Ok(output);
        }
        if output.len().saturating_add(count) > limit {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "output limit exceeded"));
        }
        output.extend_from_slice(&buffer[..count]);
    }
}

fn exact_version_output(output: &[u8], expected: &str) -> bool {
    let Ok(output) = std::str::from_utf8(output) else { return false };
    let output = output.strip_suffix('\n').unwrap_or(output);
    let output = output.strip_suffix('\r').unwrap_or(output);
    output == expected
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) {
    if let Some(parent) = path.parent()
        && let Ok(directory) = File::open(parent)
    {
        let _ = directory.sync_all();
    }
}

#[cfg(windows)]
const WINDOWS_HELPER_FLAG: &str = "--__aether-update-helper";

#[cfg(windows)]
async fn handoff_windows(
    destination: &Path,
    destination_info: &DestinationInfo,
    original_identity: &FileIdentity,
    mut downloaded: DownloadedBinary,
    lock: &mut UpdateLock,
) -> Result<(), UpdateError> {
    let (mut helper, helper_file) = create_temporary_file(destination).await?;
    drop(helper_file);
    tokio_fs::copy(&downloaded.temporary.path, &helper.path).await.map_err(|error| {
        UpdateError::from_io(
            ErrorCode::Replacement,
            "unable to prepare Windows update helper",
            &error,
        )
    })?;
    tokio_fs::set_permissions(&helper.path, destination_info.permissions.clone()).await.map_err(
        |error| {
            UpdateError::from_io(
                ErrorCode::Permissions,
                "unable to preserve helper permissions",
                &error,
            )
        },
    )?;
    validate_temporary_path(&helper.path)?;
    let helper_identity = file_identity(&helper.path).await?;
    if helper_identity.length != downloaded.identity.length
        || helper_identity.sha256 != downloaded.identity.sha256
    {
        return Err(UpdateError::new(
            ErrorCode::Checksum,
            "Windows helper copy failed identity validation",
        ));
    }

    let mut command = ProcessCommand::new(&helper.path);
    command
        .arg(WINDOWS_HELPER_FLAG)
        .arg(destination)
        .arg(&downloaded.temporary.path)
        .arg(hash_argument(&original_identity.sha256))
        .arg(original_identity.length.to_string())
        .arg(hash_argument(&downloaded.identity.sha256))
        .arg(downloaded.identity.length.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    command.spawn().map_err(|error| {
        UpdateError::from_io(
            ErrorCode::Replacement,
            "unable to start Windows update helper",
            &error,
        )
    })?;
    downloaded.temporary.retain();
    helper.retain();
    lock.retain_path();
    Ok(())
}

#[cfg(windows)]
async fn run_windows_helper(
    destination: PathBuf,
    replacement: PathBuf,
    original_identity: FileIdentity,
    replacement_identity: FileIdentity,
) -> Result<(), UpdateError> {
    let lock_path = lock_path(&destination)?;
    let deadline = Instant::now() + WINDOWS_HANDOFF_TIMEOUT;
    let lock = loop {
        match UpdateLock::try_acquire(&lock_path) {
            Ok(lock) => break lock,
            Err(error) if error.code == ErrorCode::Locked && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let _lock = lock;

    loop {
        if Instant::now() >= deadline {
            return Err(UpdateError::new(
                ErrorCode::Replacement,
                "Windows update handoff timed out",
            ));
        }
        let current_info = match inspect_destination(&destination) {
            Ok(info) => info,
            Err(error) if retryable_windows_message(&error.message) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(error) => return Err(error.update_error()),
        };
        if current_info.length != original_identity.length {
            return Err(UpdateError::new(
                ErrorCode::Replacement,
                "current executable changed before Windows replacement",
            ));
        }
        let current_identity = match file_identity(&destination).await {
            Ok(identity) => identity,
            Err(error) if error.code == ErrorCode::Replacement && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        if current_identity.length != original_identity.length
            || current_identity.sha256 != original_identity.sha256
        {
            return Err(UpdateError::new(
                ErrorCode::Replacement,
                "current executable changed before Windows replacement",
            ));
        }
        validate_temporary_path(&replacement)?;
        let replacement_identity_now = file_identity(&replacement).await?;
        if replacement_identity_now.length != replacement_identity.length
            || replacement_identity_now.sha256 != replacement_identity.sha256
        {
            return Err(UpdateError::new(
                ErrorCode::Checksum,
                "verified Windows replacement changed",
            ));
        }
        match replace_existing_file(&destination, &replacement) {
            Ok(()) => {
                if let Ok(helper_path) = std::env::current_exe() {
                    let _ = fs::remove_file(helper_path);
                }
                return Ok(());
            }
            Err(error) if retryable_windows_io(&error) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => {
                return Err(UpdateError::from_io(
                    ErrorCode::Replacement,
                    "Windows atomic executable replacement failed",
                    &error,
                ));
            }
        }
    }
}

#[cfg(windows)]
fn retryable_windows_io(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
        || error.raw_os_error().is_some_and(|code| matches!(code, 5 | 32 | 33))
}

#[cfg(windows)]
fn retryable_windows_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("permission") || message.contains("access") || message.contains("sharing")
}

#[cfg(windows)]
fn hash_argument(hash: &[u8; 32]) -> String {
    let mut output = String::with_capacity(64);
    for byte in hash {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[cfg(windows)]
fn parse_hash_argument(value: &std::ffi::OsString) -> Result<[u8; 32], UpdateError> {
    let value = value
        .to_str()
        .ok_or_else(|| UpdateError::new(ErrorCode::Replacement, "invalid update hash"))?;
    decode_hash(value)
}

#[cfg(windows)]
fn parse_size_argument(value: &std::ffi::OsString) -> Result<u64, UpdateError> {
    value
        .to_str()
        .ok_or_else(|| UpdateError::new(ErrorCode::Replacement, "invalid update size"))?
        .parse::<u64>()
        .map_err(|_| UpdateError::new(ErrorCode::Replacement, "invalid update size"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_root(label: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let root = std::env::temp_dir().join(format!(
            "aether-updater-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    fn release(tag: &str, draft: bool) -> GithubRelease {
        GithubRelease { tag_name: tag.to_owned(), draft, assets: Vec::new() }
    }

    #[test]
    fn release_selection_uses_semver_precedence_and_ignores_drafts_and_invalid_tags() {
        let releases = vec![
            release("v1.0.0-alpha.9", false),
            release("v1.0.0-alpha.10", false),
            release("v1.0.0-beta.1", false),
            release("v1.0.0", false),
            release("v2.0.0", true),
            release("release-9.0.0", false),
            release("v1.0.0+build.99", false),
        ];
        let parsed = releases.into_iter().filter_map(parse_public_release).collect();
        let selected = select_latest_release(parsed).unwrap();
        assert_eq!(selected.tag_name, "v1.0.0+build.99");
        assert_eq!(selected.version.to_string(), "1.0.0+build.99");
    }

    #[test]
    fn semver_numeric_prerelease_identifiers_are_not_lexicographic() {
        let parsed = [release("v1.0.0-alpha.9", false), release("v1.0.0-alpha.10", false)]
            .into_iter()
            .filter_map(parse_public_release)
            .collect();
        assert_eq!(select_latest_release(parsed).unwrap().tag_name, "v1.0.0-alpha.10");
    }

    #[test]
    fn direct_asset_name_is_exact_for_the_current_target() {
        let platform = platform_name().unwrap();
        let name = direct_asset_name("v0.1.0-alpha-03", platform);
        #[cfg(windows)]
        assert_eq!(name, format!("aether-v0.1.0-alpha-03-{platform}.exe"));
        #[cfg(not(windows))]
        assert_eq!(name, format!("aether-v0.1.0-alpha-03-{platform}"));
    }

    #[test]
    fn direct_asset_selection_requires_one_exact_uploaded_asset() {
        let target = direct_asset_name("v1.0.0", platform_name().unwrap());
        let assets = vec![
            GithubAsset { name: target.clone(), size: Some(1), state: Some("uploaded".to_owned()) },
            GithubAsset {
                name: "other".to_owned(),
                size: Some(1),
                state: Some("uploaded".to_owned()),
            },
        ];
        assert!(unique_asset(&assets, &target).is_some());
        assert!(unique_asset(&assets, "missing").is_none());

        let mut duplicate = assets;
        duplicate.push(GithubAsset {
            name: target.clone(),
            size: Some(1),
            state: Some("uploaded".to_owned()),
        });
        assert!(unique_asset(&duplicate, &target).is_none());
    }

    #[test]
    fn announced_asset_size_is_bounded() {
        assert!(
            validate_announced_size(
                Some(MAX_BINARY_BYTES),
                MAX_BINARY_BYTES,
                ErrorCode::Asset,
                "too large",
            )
            .is_ok()
        );
        let error = validate_announced_size(
            Some(MAX_BINARY_BYTES + 1),
            MAX_BINARY_BYTES,
            ErrorCode::Asset,
            "too large",
        )
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::Asset);
    }

    #[test]
    fn asset_state_must_be_uploaded_when_github_reports_state() {
        let asset =
            GithubAsset { name: "aether".to_owned(), size: Some(1), state: Some("new".to_owned()) };
        let error = validate_asset(&asset, ErrorCode::Asset, "binary asset").unwrap_err();
        assert_eq!(error.code, ErrorCode::Asset);
    }

    #[test]
    fn checksum_manifest_requires_one_exact_valid_entry() {
        let asset = "aether-v1.0.0-macos-x86_64";
        let digest = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let body = format!(
            "{digest}  {asset}\n1111111111111111111111111111111111111111111111111111111111111111  other\n"
        );
        assert_eq!(checksum_for_asset(body.as_bytes(), asset).unwrap()[0], 0);
        assert!(
            checksum_for_asset(format!("{digest}  {asset}\n{digest}  {asset}\n").as_bytes(), asset)
                .is_err()
        );
        assert!(checksum_for_asset(format!("not-a-hash  {asset}\n").as_bytes(), asset).is_err());
        assert!(checksum_for_asset(format!("{digest}  other\n").as_bytes(), asset).is_err());
    }

    #[test]
    fn version_output_is_exact_but_accepts_one_platform_line_ending() {
        assert!(exact_version_output(b"1.2.3\n", "1.2.3"));
        assert!(exact_version_output(b"1.2.3\r\n", "1.2.3"));
        assert!(!exact_version_output(b" 1.2.3\n", "1.2.3"));
        assert!(!exact_version_output(b"1.2.3\nextra\n", "1.2.3"));
    }

    #[test]
    fn stale_update_temporaries_are_scoped_to_the_destination() {
        let root = test_root("cleanup");
        let destination = root.join("aether");
        fs::write(&destination, b"old").unwrap();
        let prefix = temp_prefix(&destination).unwrap();
        let stale = root.join(format!("{prefix}old-1{}", temp_extension()));
        fs::write(&stale, b"stale").unwrap();
        fs::write(root.join(".aether-fx-other-1.tmp"), b"keep").unwrap();
        cleanup_stale_temps(&destination).unwrap();
        assert!(!stale.exists());
        assert!(root.join(".aether-fx-other-1.tmp").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn update_lock_refuses_a_second_owner_and_releases_after_drop() {
        let root = test_root("lock");
        let path = root.join(".aether-fx-update-aether.lock");
        let first = UpdateLock::acquire(&path).unwrap();
        let second = UpdateLock::acquire(&path).unwrap_err();
        assert_eq!(second.code, ErrorCode::Locked);
        drop(first);
        assert!(UpdateLock::acquire(&path).is_ok());
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn destination_validation_rejects_links_directories_and_read_only_files() {
        let root = test_root("destination");
        let regular = root.join("aether");
        fs::write(&regular, b"binary").unwrap();
        assert!(inspect_destination(&regular).is_ok());
        let directory = root.join("directory");
        fs::create_dir(&directory).unwrap();
        assert!(inspect_destination(&directory).is_err());
        let read_only = root.join("read-only");
        fs::write(&read_only, b"binary").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&read_only, fs::Permissions::from_mode(0o444)).unwrap();
        assert_eq!(
            inspect_destination(&read_only).unwrap_err().kind,
            DestinationFailureKind::Permissions
        );
        let link = root.join("link");
        std::os::unix::fs::symlink(&regular, &link).unwrap();
        assert_eq!(
            inspect_destination(&link).unwrap_err().kind,
            DestinationFailureKind::Permissions
        );
        let no_write_parent = root.join("no-write-parent");
        fs::create_dir(&no_write_parent).unwrap();
        let nested = no_write_parent.join("aether");
        fs::write(&nested, b"binary").unwrap();
        fs::set_permissions(&no_write_parent, fs::Permissions::from_mode(0o555)).unwrap();
        assert_eq!(
            inspect_destination(&nested).unwrap_err().kind,
            DestinationFailureKind::Permissions
        );
        fs::set_permissions(&no_write_parent, fs::Permissions::from_mode(0o755)).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn temporary_files_are_removed_unless_explicitly_retained() {
        let root = test_root("temporary");
        let removed = root.join("removed.tmp");
        {
            fs::write(&removed, b"temporary").unwrap();
            let _temporary = TemporaryFile { path: removed.clone(), retain_on_drop: false };
        }
        assert!(!removed.exists());

        let retained = root.join("retained.tmp");
        {
            fs::write(&retained, b"temporary").unwrap();
            let mut temporary = TemporaryFile { path: retained.clone(), retain_on_drop: false };
            temporary.retain();
        }
        assert!(retained.exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn result_and_error_json_have_stable_fields() {
        let result = UpdateResult {
            latest_version: CURRENT_VERSION.to_owned(),
            updated: false,
            path: "/tmp/aether".to_owned(),
        };
        let value: serde_json::Value = serde_json::from_str(&result.json_line()).unwrap();
        assert_eq!(value["current_version"], CURRENT_VERSION);
        assert_eq!(value["updated"], false);
        assert_eq!(value["path"], "/tmp/aether");
        let error: serde_json::Value = serde_json::from_str(
            &UpdateError::new(ErrorCode::Checksum, "checksum verification failed").json_line(),
        )
        .unwrap();
        assert_eq!(error["latest_version"], serde_json::Value::Null);
        assert_eq!(error["error"]["code"], "checksum");
        assert_eq!(error["path"], serde_json::Value::Null);
    }

    #[test]
    fn replacement_url_is_fixed_to_the_official_repository() {
        assert_eq!(
            release_asset_url("v1.2.3", "SHA256SUMS"),
            "https://github.com/ferxalbs/aether-fx/releases/download/v1.2.3/SHA256SUMS"
        );
    }
}
