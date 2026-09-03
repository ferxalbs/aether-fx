#![deny(unsafe_code)]

//! The verified, deliberately narrow boundary between AETHER Fx and Obscura.
//!
//! This crate owns the optional external process and its MCP transport.  The rest of AETHER
//! receives only the six stable browser operations exported by [`ObscuraSupervisor`]; provider
//! advertised descriptions and arbitrary provider tools never cross this boundary.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use aether_core::CancellationFlag;
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::Mutex;

mod install;
mod manifest;
mod protocol;
mod security;

pub use install::{
    ObscuraInstallReport, ObscuraInstallationStatus, inspect_installation, install_obscura,
};
pub use manifest::{
    ArchiveFormat, MCP_PROTOCOL_VERSION, OBSCURA_VERSION, ObscuraArtifact, current_artifact,
    current_target,
};
pub use security::{BrowserUrl, sanitized_origin, validate_browser_url};

/// Maximum bytes in one line-delimited JSON-RPC frame, excluding the newline delimiter.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum time allowed for one provider request.
pub const MAX_MCP_REQUEST_MS: u64 = 120_000;
/// Maximum text returned by the provider before it reaches a tool result.
pub const MAX_TOOL_RESULT_BYTES: usize = 256 * 1024;
/// Maximum downloaded archive size accepted by the static installer.
pub const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
/// Maximum total uncompressed bytes accepted from one verified archive.
pub const MAX_EXTRACT_BYTES: u64 = 256 * 1024 * 1024;
/// Maximum size of the private, adapter-managed storage-state file.
pub const MAX_STORAGE_STATE_BYTES: u64 = 1024 * 1024;
/// Maximum output retained for a fixed performance evaluation.
pub const MAX_PERFORMANCE_RESULT_BYTES: usize = 64 * 1024;

/// The only browser operation names that may be registered with Rainy.
pub const BROWSER_TOOL_NAMES: [&str; 6] = [
    "browser.tabs",
    "browser.navigate",
    "browser.snapshot",
    "browser.find",
    "browser.wait",
    "browser.performance_audit",
];

/// Errors crossing the external-provider boundary.  Messages are bounded by the caller before
/// they are rendered or persisted; this type intentionally never carries raw MCP payloads.
#[derive(Debug, Error)]
pub enum ObscuraError {
    #[error("Obscura is not published for target {target}")]
    UnsupportedTarget { target: String },
    #[error("invalid browser URL: {message}")]
    InvalidUrl { message: String },
    #[error("browser network policy rejected the URL: {message}")]
    NetworkPolicy { message: String },
    #[error("Obscura installation failed during {operation}: {message}")]
    Installation { operation: String, message: String },
    #[error("Obscura artifact integrity check failed: {message}")]
    Integrity { message: String },
    #[error("Obscura process failed during {operation}: {message}")]
    Process { operation: String, message: String },
    #[error("MCP framing failed: {message}")]
    Framing { message: String },
    #[error("MCP frame exceeded the {limit}-byte limit")]
    FrameLimit { limit: usize },
    #[error("MCP protocol validation failed: {message}")]
    Protocol { message: String },
    #[error("MCP request timed out during {operation}")]
    Timeout { operation: String },
    #[error("MCP tool is not supported: {name}")]
    ToolNotSupported { name: String },
    #[error("MCP result exceeded the {limit}-byte limit")]
    ResultLimit { limit: usize },
    #[error("MCP error {code}: {message}")]
    McpError { code: i64, message: String },
    #[error("unsupported MCP server message: {method}")]
    UnsupportedServerMessage { method: String },
    #[error("Obscura tool {name} failed: {message}")]
    ToolFailed { name: String, message: String },
    #[error("Obscura storage state failed during {operation}: {message}")]
    Storage { operation: String, message: String },
    #[error("Obscura operation was cancelled")]
    Cancelled,
    #[error("another Obscura installation is already running")]
    InstallLocked,
    #[error("the fixed Obscura binary is not installed for target {target}")]
    NotInstalled { target: String },
}

impl ObscuraError {
    /// Stable error category for structured tool results and diagnostics.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::UnsupportedTarget { .. } => "unsupported_target",
            Self::InvalidUrl { .. } => "invalid_url",
            Self::NetworkPolicy { .. } => "network_policy",
            Self::Installation { .. } => "installation_failed",
            Self::Integrity { .. } => "integrity_failed",
            Self::Process { .. } => "process_failed",
            Self::Framing { .. } => "mcp_framing",
            Self::FrameLimit { .. } => "mcp_framing",
            Self::Protocol { .. } => "mcp_protocol",
            Self::Timeout { .. } => "timeout",
            Self::ToolNotSupported { .. } => "tool_not_supported",
            Self::ResultLimit { .. } => "result_limit",
            Self::McpError { .. } | Self::ToolFailed { .. } => "provider_tool_error",
            Self::UnsupportedServerMessage { .. } => "unsupported_server_message",
            Self::Storage { .. } => "storage_failed",
            Self::Cancelled => "cancelled",
            Self::InstallLocked => "install_locked",
            Self::NotInstalled { .. } => "not_installed",
        }
    }

    /// Whether retrying the same semantic operation can reasonably succeed.
    pub const fn retryable(&self) -> bool {
        matches!(
            self,
            Self::Process { .. }
                | Self::Framing { .. }
                | Self::FrameLimit { .. }
                | Self::Protocol { .. }
                | Self::Timeout { .. }
                | Self::McpError { .. }
                | Self::ToolFailed { .. }
                | Self::Storage { .. }
                | Self::Cancelled
        )
    }
}

/// Text returned by a validated MCP tool call.
#[derive(Clone, Debug)]
pub struct ObscuraToolResponse {
    /// Bounded text content from the provider.
    pub text: String,
    /// Whether Obscura reported a tool-level error.
    pub is_error: bool,
}

/// Private paths derived from AETHER's state root and the canonical workspace identity.
#[derive(Clone, Debug)]
pub struct ObscuraPaths {
    state_root: PathBuf,
    obscura_root: PathBuf,
    bin_root: PathBuf,
    profiles_root: PathBuf,
    install_dir: PathBuf,
    profile_dir: PathBuf,
    storage_state_path: PathBuf,
    workspace_id: String,
    artifact: &'static ObscuraArtifact,
}

impl ObscuraPaths {
    /// Derive private provider paths without creating files or contacting the network.
    pub fn for_workspace(
        state_root: impl AsRef<Path>,
        workspace_id: impl AsRef<str>,
        artifact: &'static ObscuraArtifact,
    ) -> Self {
        let state_root = state_root.as_ref().to_owned();
        let workspace_id = safe_component(workspace_id.as_ref());
        let obscura_root = state_root.join("obscura");
        let bin_root = obscura_root.join("bin");
        let profiles_root = obscura_root.join("profiles");
        let install_dir = bin_root.join(artifact.version);
        let profile_dir = profiles_root.join(&workspace_id);
        let storage_state_path = profile_dir.join("browser_storage_state.json");
        Self {
            state_root,
            obscura_root,
            bin_root,
            profiles_root,
            install_dir,
            profile_dir,
            storage_state_path,
            workspace_id,
            artifact,
        }
    }

    /// Return the static artifact this path set belongs to.
    pub const fn artifact(&self) -> &'static ObscuraArtifact {
        self.artifact
    }

    /// Return the logical profile identity, never a sensitive filesystem path.
    pub fn profile_id(&self) -> &str {
        &self.workspace_id
    }

    /// Return the private profile directory used as Obscura's working directory.
    pub fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    /// Return the exact verified binary path for the current static pin.
    pub fn binary_path(&self) -> PathBuf {
        self.install_dir.join(self.artifact.binary_name)
    }

    pub(crate) fn worker_path(&self) -> PathBuf {
        self.install_dir.join(self.artifact.worker_name)
    }

    pub(crate) fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub(crate) fn obscura_root(&self) -> &Path {
        &self.obscura_root
    }

    pub(crate) fn bin_root(&self) -> &Path {
        &self.bin_root
    }

    pub(crate) fn profiles_root(&self) -> &Path {
        &self.profiles_root
    }

    pub(crate) fn install_dir(&self) -> &Path {
        &self.install_dir
    }

    pub(crate) fn storage_state_path(&self) -> &Path {
        &self.storage_state_path
    }

    pub(crate) async fn ensure_layout(&self) -> Result<(), ObscuraError> {
        install::ensure_layout(self).await
    }
}

/// Runtime status for the optional provider.  It deliberately omits sensitive absolute paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObscuraStatus {
    pub installed: bool,
    pub active: bool,
    pub healthy: bool,
    pub expected_version: String,
    pub installed_version: Option<String>,
    pub mcp_protocol_version: Option<String>,
    pub profile_id: String,
}

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// One supervised Obscura process and one serialized MCP `stdio` channel.
pub struct ObscuraSupervisor {
    paths: ObscuraPaths,
    connection: Mutex<Option<protocol::McpConnection>>,
    pending_storage_state: Mutex<Option<Value>>,
    session_id: u64,
    active: AtomicBool,
    healthy: AtomicBool,
}

impl std::fmt::Debug for ObscuraSupervisor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObscuraSupervisor")
            .field("profile_id", &self.paths.profile_id())
            .field("session_id", &self.session_id)
            .field("active", &self.active.load(Ordering::Acquire))
            .field("healthy", &self.healthy.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl ObscuraSupervisor {
    /// Spawn, handshake, validate, and register one fixed Obscura process.
    pub async fn launch(paths: ObscuraPaths) -> Result<Arc<Self>, ObscuraError> {
        paths.ensure_layout().await?;
        let installation = inspect_installation(&paths).await?;
        if !installation.installed || !installation.version_matches {
            return Err(ObscuraError::NotInstalled { target: paths.artifact().target.to_owned() });
        }

        let mut connection = protocol::McpConnection::spawn(
            &paths.binary_path(),
            paths.artifact().launch_args,
            paths.profile_dir(),
        )
        .await?;
        if let Err(error) = connection.handshake(paths.artifact().mcp_protocol_version).await {
            let _ = connection.shutdown().await;
            return Err(error);
        }
        let pending_storage_state = restore_storage_state(&mut connection, &paths).await?;

        Ok(Arc::new(Self {
            paths,
            connection: Mutex::new(Some(connection)),
            pending_storage_state: Mutex::new(pending_storage_state),
            session_id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            active: AtomicBool::new(true),
            healthy: AtomicBool::new(true),
        }))
    }

    /// Return the provider's logical browser-session resource id.
    pub const fn session_id(&self) -> u64 {
        self.session_id
    }

    /// Return the fixed artifact selected by this build.
    pub const fn artifact(&self) -> &'static ObscuraArtifact {
        self.paths.artifact()
    }

    /// Return the logical profile id without revealing its absolute path.
    pub fn profile_id(&self) -> &str {
        self.paths.profile_id()
    }

    /// Return whether a healthy provider is currently attached.
    pub fn is_healthy(&self) -> bool {
        self.active.load(Ordering::Acquire) && self.healthy.load(Ordering::Acquire)
    }

    /// Inspect installation and process state for `/browser status`.
    pub async fn status(&self) -> Result<ObscuraStatus, ObscuraError> {
        let installation = inspect_installation(&self.paths).await?;
        let mut active = self.active.load(Ordering::Acquire);
        let healthy = if active {
            let mut connection = self.connection.lock().await;
            let exited = connection
                .as_mut()
                .map(protocol::McpConnection::process_exited)
                .transpose()?
                .unwrap_or(true);
            if exited {
                connection.take();
                self.active.store(false, Ordering::Release);
                self.healthy.store(false, Ordering::Release);
                active = false;
                false
            } else {
                self.healthy.load(Ordering::Acquire)
            }
        } else {
            false
        };
        Ok(ObscuraStatus {
            installed: installation.installed,
            active,
            healthy,
            expected_version: self.artifact().version.to_owned(),
            installed_version: installation.installed_version,
            mcp_protocol_version: active.then(|| self.artifact().mcp_protocol_version.to_owned()),
            profile_id: self.profile_id().to_owned(),
        })
    }

    /// Return a bounded list of tabs from the active Obscura session.
    pub async fn tabs(&self, cancellation: &CancellationFlag) -> Result<String, ObscuraError> {
        self.invoke_text("browser_tab_list", json!({}), cancellation).await
    }

    /// Navigate the active tab to one allowed public HTTP(S) URL.
    pub async fn navigate(
        &self,
        raw_url: &str,
        cancellation: &CancellationFlag,
    ) -> Result<String, ObscuraError> {
        let url = validate_browser_url(raw_url)?;
        if url.loopback {
            return Err(ObscuraError::NetworkPolicy {
                message: "Obscura 0.2.1 blocks loopback unless its broad private-network flag is enabled; AETHER leaves that flag disabled".to_owned(),
            });
        }
        let result = self
            .invoke_text(
                "browser_navigate",
                json!({"url": url.url.as_str(), "waitUntil": "load"}),
                cancellation,
            )
            .await?;
        self.restore_pending_storage_state(&url.origin, cancellation).await?;
        Ok(result)
    }

    /// Return bounded visible page text as provided by Obscura's snapshot operation.
    pub async fn snapshot(
        &self,
        max_chars: Option<u64>,
        cancellation: &CancellationFlag,
    ) -> Result<String, ObscuraError> {
        let max_chars = max_chars.unwrap_or(8_000).min(64_000);
        self.invoke_text("browser_snapshot", json!({"max_chars": max_chars}), cancellation).await
    }

    /// Search the visible page text with bounded result count and context.
    pub async fn find(
        &self,
        query: &str,
        case_sensitive: bool,
        limit: u64,
        context_chars: u64,
        cancellation: &CancellationFlag,
    ) -> Result<String, ObscuraError> {
        self.invoke_text(
            "browser_search",
            json!({
                "query": query,
                "case_sensitive": case_sensitive,
                "limit": limit.clamp(1, 50),
                "context_chars": context_chars.min(512)
            }),
            cancellation,
        )
        .await
    }

    /// Wait for a bounded CSS selector interval.  The provider contract expresses seconds.
    pub async fn wait(
        &self,
        selector: &str,
        timeout_ms: u64,
        cancellation: &CancellationFlag,
    ) -> Result<String, ObscuraError> {
        let timeout_seconds = (timeout_ms.clamp(1_000, 60_000) as f64) / 1_000.0;
        self.invoke_text(
            "browser_wait_for",
            json!({"selector": selector, "timeout": timeout_seconds}),
            cancellation,
        )
        .await
    }

    /// Perform the fixed, read-only performance script and normalize its structured result.
    pub async fn performance_audit(
        &self,
        raw_url: &str,
        cancellation: &CancellationFlag,
    ) -> Result<String, ObscuraError> {
        let url = validate_browser_url(raw_url)?;
        if url.loopback {
            return Err(ObscuraError::NetworkPolicy {
                message: "Obscura 0.2.1 cannot audit loopback with AETHER's safe network policy"
                    .to_owned(),
            });
        }
        let _ = self
            .invoke_text(
                "browser_navigate",
                json!({"url": url.url.as_str(), "waitUntil": "load"}),
                cancellation,
            )
            .await?;
        self.restore_pending_storage_state(&url.origin, cancellation).await?;
        let evaluated = self
            .invoke_text(
                "browser_evaluate",
                json!({"expression": PERFORMANCE_AUDIT_EXPRESSION}),
                cancellation,
            )
            .await?;
        normalize_performance_result(&evaluated, url.url.as_str())
    }

    /// Stop the provider, persist private browser state, and retain the profile directory.
    pub async fn shutdown(&self) -> Result<(), ObscuraError> {
        self.active.store(false, Ordering::Release);
        let connection = { self.connection.lock().await.take() };
        let Some(mut connection) = connection else {
            self.healthy.store(false, Ordering::Release);
            return Ok(());
        };
        let storage_error = save_storage_state(&mut connection, &self.paths).await.err();
        let shutdown_error = connection.shutdown().await.err();
        self.healthy.store(false, Ordering::Release);
        if let Some(error) = shutdown_error.or(storage_error) {
            return Err(error);
        }
        Ok(())
    }

    async fn invoke_text(
        &self,
        name: &str,
        arguments: Value,
        cancellation: &CancellationFlag,
    ) -> Result<String, ObscuraError> {
        let response = self.invoke(name, arguments, cancellation).await?;
        if response.is_error {
            return Err(ObscuraError::ToolFailed {
                name: name.to_owned(),
                message: bounded_message(&response.text, 1_024),
            });
        }
        Ok(response.text)
    }

    async fn invoke(
        &self,
        name: &str,
        arguments: Value,
        cancellation: &CancellationFlag,
    ) -> Result<ObscuraToolResponse, ObscuraError> {
        let mut connection = self.connection.lock().await;
        let Some(provider) = connection.as_mut() else {
            return Err(ObscuraError::Process {
                operation: "use Obscura".to_owned(),
                message: "provider is inactive; use /browser to start it again".to_owned(),
            });
        };
        if provider.process_exited()? {
            connection.take();
            self.active.store(false, Ordering::Release);
            self.healthy.store(false, Ordering::Release);
            return Err(ObscuraError::Process {
                operation: "use Obscura".to_owned(),
                message: "Obscura terminated during the session".to_owned(),
            });
        }
        let result = provider.call_tool(name, arguments, cancellation).await;
        if should_invalidate(&result) {
            connection.take();
            self.active.store(false, Ordering::Release);
            self.healthy.store(false, Ordering::Release);
        }
        result
    }

    async fn restore_pending_storage_state(
        &self,
        origin: &str,
        cancellation: &CancellationFlag,
    ) -> Result<(), ObscuraError> {
        let state = {
            let pending = self.pending_storage_state.lock().await;
            pending.as_ref().and_then(|state| {
                state
                    .get("origins")
                    .and_then(Value::as_array)
                    .and_then(|origins| {
                        origins.iter().find(|entry| {
                            entry.get("origin").and_then(Value::as_str) == Some(origin)
                        })
                    })
                    .map(|entry| json!({"cookies": [], "origins": [entry]}))
            })
        };
        let Some(state) = state else {
            return Ok(());
        };
        let response =
            self.invoke("browser_set_storage_state", json!({"state": state}), cancellation).await?;
        if response.is_error {
            return Err(ObscuraError::Storage {
                operation: "restore browser origin state".to_owned(),
                message: "Obscura rejected the private origin storage state".to_owned(),
            });
        }
        Ok(())
    }
}

const PERFORMANCE_AUDIT_EXPRESSION: &str = r#"(function(){
  var n = (performance.getEntriesByType('navigation') || [])[0] || {};
  var paints = performance.getEntriesByType('paint') || [];
  var fcp = null;
  for (var i = 0; i < paints.length; i++) {
    if (paints[i].name === 'first-contentful-paint') { fcp = paints[i].startTime; }
  }
  var lcpEntries = performance.getEntriesByType('largest-contentful-paint') || [];
  var lcp = lcpEntries.length ? lcpEntries[lcpEntries.length - 1].startTime : null;
  var shifts = performance.getEntriesByType('layout-shift') || [];
  var cls = 0;
  for (var j = 0; j < shifts.length; j++) {
    if (!shifts[j].hadRecentInput) { cls += Number(shifts[j].value || 0); }
  }
  var resources = performance.getEntriesByType('resource') || [];
  var bytes = 0;
  for (var k = 0; k < resources.length; k++) { bytes += Number(resources[k].transferSize || 0); }
  function finite(value) { return typeof value === 'number' && isFinite(value) ? value : null; }
  return JSON.stringify({
    final_url: String(location.href || ''),
    redirects: [],
    navigation_time_ms: finite(n.duration),
    response_initial_ms: finite(n.responseStart),
    dom_content_loaded_ms: finite(n.domContentLoadedEventEnd),
    load_ms: finite(n.loadEventEnd),
    first_contentful_paint_ms: finite(fcp),
    largest_contentful_paint_ms: finite(lcp),
    cumulative_layout_shift: finite(cls),
    resource_count: resources.length,
    bytes_transferred: bytes,
    warnings: [],
    missing_metrics: []
  });
})()"#;

fn normalize_performance_result(raw: &str, requested_url: &str) -> Result<String, ObscuraError> {
    let parsed = serde_json::from_str::<Value>(raw)
        .or_else(|_| {
            serde_json::from_str::<Value>(&serde_json::from_str::<String>(raw).unwrap_or_default())
        })
        .map_err(|_| ObscuraError::Protocol {
            message: "fixed performance audit returned invalid JSON".to_owned(),
        })?;
    let mut object = parsed.as_object().cloned().ok_or_else(|| ObscuraError::Protocol {
        message: "fixed performance audit returned a non-object".to_owned(),
    })?;
    let required = [
        "final_url",
        "redirects",
        "navigation_time_ms",
        "response_initial_ms",
        "dom_content_loaded_ms",
        "load_ms",
        "first_contentful_paint_ms",
        "largest_contentful_paint_ms",
        "cumulative_layout_shift",
        "resource_count",
        "bytes_transferred",
        "warnings",
        "missing_metrics",
    ];
    object.entry("final_url").or_insert_with(|| Value::String(requested_url.to_owned()));
    object.entry("redirects").or_insert_with(|| json!([]));
    object.entry("warnings").or_insert_with(|| json!([]));
    object.entry("missing_metrics").or_insert_with(|| json!([]));
    let missing_fields = required
        .iter()
        .copied()
        .filter(|field| !object.get(*field).is_some_and(|value| !value.is_null()))
        .collect::<Vec<_>>();
    let missing =
        object.get_mut("missing_metrics").and_then(Value::as_array_mut).ok_or_else(|| {
            ObscuraError::Protocol {
                message: "performance audit missing_metrics is not an array".to_owned(),
            }
        })?;
    for field in missing_fields {
        if !missing.iter().any(|value| value.as_str() == Some(field)) {
            missing.push(Value::String(field.to_owned()));
        }
    }
    if let Some(warnings) = object.get_mut("warnings").and_then(Value::as_array_mut)
        && !warnings.iter().any(|value| value.as_str() == Some("redirect chain unavailable"))
    {
        warnings.push(Value::String("redirect chain unavailable".to_owned()));
    }
    let output =
        serde_json::to_string(&Value::Object(object)).map_err(|error| ObscuraError::Protocol {
            message: format!("performance audit serialization failed: {error}"),
        })?;
    if output.len() > MAX_PERFORMANCE_RESULT_BYTES {
        return Err(ObscuraError::ResultLimit { limit: MAX_PERFORMANCE_RESULT_BYTES });
    }
    Ok(output)
}

async fn restore_storage_state(
    connection: &mut protocol::McpConnection,
    paths: &ObscuraPaths,
) -> Result<Option<Value>, ObscuraError> {
    if !connection.supports("browser_set_storage_state") {
        return Ok(None);
    }
    let Some(state) = read_storage_state(paths).await? else {
        return Ok(None);
    };
    // Obscura v0.2.1 can restore cookies before a tab exists, but its local/session storage
    // implementation evaluates JavaScript on the active page. Apply the cookie portion now and
    // retain origin entries for the first matching navigation.
    let bootstrap = json!({
        "cookies": state.get("cookies").cloned().unwrap_or_else(|| json!([])),
        "origins": []
    });
    let response = connection
        .call_tool(
            "browser_set_storage_state",
            json!({"state": bootstrap}),
            &CancellationFlag::new(),
        )
        .await?;
    if response.is_error {
        return Err(ObscuraError::Storage {
            operation: "restore browser state".to_owned(),
            message: "Obscura rejected the private storage state".to_owned(),
        });
    }
    let has_origins = state.get("origins").and_then(Value::as_array).is_some_and(|origins| {
        origins.iter().any(|entry| entry.get("origin").and_then(Value::as_str).is_some())
    });
    Ok(has_origins.then_some(state))
}

async fn save_storage_state(
    connection: &mut protocol::McpConnection,
    paths: &ObscuraPaths,
) -> Result<(), ObscuraError> {
    if !connection.supports("browser_storage_state") {
        return Ok(());
    }
    let response =
        connection.call_tool("browser_storage_state", json!({}), &CancellationFlag::new()).await?;
    if response.is_error {
        return Err(ObscuraError::Storage {
            operation: "export browser state".to_owned(),
            message: "Obscura rejected the private storage-state export".to_owned(),
        });
    }
    let state =
        serde_json::from_str::<Value>(&response.text).map_err(|_| ObscuraError::Storage {
            operation: "export browser state".to_owned(),
            message: "Obscura returned invalid storage-state JSON".to_owned(),
        })?;
    if !state.is_object() {
        return Err(ObscuraError::Storage {
            operation: "export browser state".to_owned(),
            message: "Obscura returned a non-object storage state".to_owned(),
        });
    }
    let bytes = serde_json::to_vec(&state).map_err(|error| ObscuraError::Storage {
        operation: "serialize browser state".to_owned(),
        message: error.to_string(),
    })?;
    if bytes.len() as u64 > MAX_STORAGE_STATE_BYTES {
        return Err(ObscuraError::ResultLimit { limit: MAX_STORAGE_STATE_BYTES as usize });
    }
    write_storage_state(paths.storage_state_path(), &bytes).await
}

async fn read_storage_state(paths: &ObscuraPaths) -> Result<Option<Value>, ObscuraError> {
    let metadata = match tokio::fs::symlink_metadata(paths.storage_state_path()).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ObscuraError::Storage {
                operation: "inspect browser state".to_owned(),
                message: error.to_string(),
            });
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(ObscuraError::Storage {
            operation: "inspect browser state".to_owned(),
            message: "storage-state path is not a regular file".to_owned(),
        });
    }
    if metadata.len() > MAX_STORAGE_STATE_BYTES {
        return Err(ObscuraError::ResultLimit { limit: MAX_STORAGE_STATE_BYTES as usize });
    }
    let bytes = tokio::fs::read(paths.storage_state_path()).await.map_err(|error| {
        ObscuraError::Storage {
            operation: "read browser state".to_owned(),
            message: error.to_string(),
        }
    })?;
    let state = serde_json::from_slice::<Value>(&bytes).map_err(|_| ObscuraError::Storage {
        operation: "parse browser state".to_owned(),
        message: "storage-state file is not valid JSON".to_owned(),
    })?;
    if !state.is_object() {
        return Err(ObscuraError::Storage {
            operation: "parse browser state".to_owned(),
            message: "storage-state root must be an object".to_owned(),
        });
    }
    Ok(Some(state))
}

async fn write_storage_state(path: &Path, bytes: &[u8]) -> Result<(), ObscuraError> {
    let parent = path.parent().ok_or_else(|| ObscuraError::Storage {
        operation: "write browser state".to_owned(),
        message: "storage-state path has no parent".to_owned(),
    })?;
    let metadata = match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(ObscuraError::Storage {
                operation: "inspect browser state destination".to_owned(),
                message: error.to_string(),
            });
        }
    };
    if metadata.is_some_and(|metadata| !metadata.is_file() || metadata.file_type().is_symlink()) {
        return Err(ObscuraError::Storage {
            operation: "write browser state".to_owned(),
            message: "storage-state destination is not a regular file".to_owned(),
        });
    }
    let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".browser-storage-{id}.tmp"));
    let result = async {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .await
            .map_err(|error| ObscuraError::Storage {
                operation: "create browser state temporary".to_owned(),
                message: error.to_string(),
            })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                .await
                .map_err(|error| ObscuraError::Storage {
                    operation: "protect browser state".to_owned(),
                    message: error.to_string(),
                })?;
        }
        use tokio::io::AsyncWriteExt;
        file.write_all(bytes).await.map_err(|error| ObscuraError::Storage {
            operation: "write browser state".to_owned(),
            message: error.to_string(),
        })?;
        file.sync_all().await.map_err(|error| ObscuraError::Storage {
            operation: "sync browser state".to_owned(),
            message: error.to_string(),
        })?;
        drop(file);
        tokio::fs::rename(&temporary, path).await.map_err(|error| ObscuraError::Storage {
            operation: "install browser state".to_owned(),
            message: error.to_string(),
        })
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

fn should_invalidate(result: &Result<ObscuraToolResponse, ObscuraError>) -> bool {
    matches!(
        result,
        Err(ObscuraError::Process { .. }
            | ObscuraError::Framing { .. }
            | ObscuraError::Protocol { .. }
            | ObscuraError::UnsupportedServerMessage { .. }
            | ObscuraError::ResultLimit { .. }
            | ObscuraError::ToolNotSupported { .. }
            | ObscuraError::Timeout { .. }
            | ObscuraError::Cancelled)
    )
}

fn safe_component(value: &str) -> String {
    let mut result = String::with_capacity(value.len().min(96));
    for character in value.chars() {
        if result.len() >= 96 {
            break;
        }
        if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
            result.push(character);
        } else {
            result.push('_');
        }
    }
    if result.is_empty() { "workspace".to_owned() } else { result }
}

fn bounded_message(value: &str, limit: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        if output.len().saturating_add(character.len_utf8()) > limit {
            break;
        }
        output.push(if character.is_control() { '�' } else { character });
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_flight_timeout_and_cancellation_invalidate_the_stdio_session() {
        assert!(should_invalidate(&Err(ObscuraError::Timeout {
            operation: "tools/call".to_owned(),
        })));
        assert!(should_invalidate(&Err(ObscuraError::Cancelled)));
        assert!(!should_invalidate(&Ok(ObscuraToolResponse {
            text: "ok".to_owned(),
            is_error: false,
        })));
    }
}
