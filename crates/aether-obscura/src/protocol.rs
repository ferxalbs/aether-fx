use std::{
    collections::{HashSet, VecDeque},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
};

use aether_core::CancellationFlag;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
    task::JoinHandle,
    time::{Duration, timeout},
};

use crate::{
    MAX_FRAME_BYTES, MAX_MCP_REQUEST_MS, MAX_TOOL_RESULT_BYTES, ObscuraError, ObscuraToolResponse,
};

const STDERR_CAPACITY: usize = 32 * 1024;
const STDERR_READ_CHUNK: usize = 4096;

/// Names that may cross the provider boundary. The model-facing registry is even smaller.
pub(crate) const INTERNAL_TOOL_NAMES: [&str; 8] = [
    "browser_navigate",
    "browser_snapshot",
    "browser_wait_for",
    "browser_search",
    "browser_tab_list",
    "browser_evaluate",
    "browser_storage_state",
    "browser_set_storage_state",
];

#[derive(Debug)]
pub(crate) struct McpConnection {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    tools: HashSet<String>,
    stderr: Arc<Mutex<StderrBuffer>>,
    stderr_task: Option<JoinHandle<()>>,
}

#[derive(Debug, Default)]
struct StderrBuffer {
    bytes: VecDeque<u8>,
}

impl StderrBuffer {
    fn push(&mut self, bytes: &[u8]) {
        let excess = self.bytes.len().saturating_add(bytes.len()).saturating_sub(STDERR_CAPACITY);
        for _ in 0..excess {
            let _ = self.bytes.pop_front();
        }
        self.bytes.extend(bytes.iter().copied());
    }
}

impl McpConnection {
    pub(crate) async fn spawn(
        binary: &Path,
        launch_args: &[&str],
        profile_dir: &Path,
    ) -> Result<Self, ObscuraError> {
        if !binary.is_absolute() || !profile_dir.is_absolute() {
            return Err(ObscuraError::Process {
                operation: "spawn Obscura".to_owned(),
                message: "the verified binary and private profile must use absolute paths"
                    .to_owned(),
            });
        }
        let mut command = tokio::process::Command::new(binary);
        command
            .args(launch_args)
            .current_dir(profile_dir)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn().map_err(|error| ObscuraError::Process {
            operation: "spawn Obscura".to_owned(),
            message: error.to_string(),
        })?;
        let stdin = child.stdin.take().ok_or_else(|| ObscuraError::Process {
            operation: "open Obscura stdin".to_owned(),
            message: "child did not provide stdin".to_owned(),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| ObscuraError::Process {
            operation: "open Obscura stdout".to_owned(),
            message: "child did not provide stdout".to_owned(),
        })?;
        let stderr = Arc::new(Mutex::new(StderrBuffer::default()));
        let stderr_task = child.stderr.take().map(|mut stream| {
            let stderr = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut buffer = [0_u8; STDERR_READ_CHUNK];
                loop {
                    match stream.read(&mut buffer).await {
                        Ok(0) | Err(_) => break,
                        Ok(count) => {
                            if let Ok(mut retained) = stderr.lock() {
                                retained.push(&buffer[..count]);
                            }
                        }
                    }
                }
            })
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
            tools: HashSet::new(),
            stderr,
            stderr_task,
        })
    }

    pub(crate) async fn handshake(&mut self, expected_protocol: &str) -> Result<(), ObscuraError> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": expected_protocol,
                    "capabilities": {},
                    "clientInfo": {"name": "aether-fx", "version": env!("CARGO_PKG_VERSION")}
                }),
                None,
            )
            .await?;
        let protocol = result.get("protocolVersion").and_then(Value::as_str).ok_or_else(|| {
            ObscuraError::Protocol {
                message: "initialize response omitted protocolVersion".to_owned(),
            }
        })?;
        if protocol != expected_protocol {
            return Err(ObscuraError::Protocol {
                message: format!(
                    "MCP protocol {protocol} is incompatible with {expected_protocol}"
                ),
            });
        }
        if result.get("capabilities").and_then(Value::as_object).is_none() {
            return Err(ObscuraError::Protocol {
                message: "initialize response omitted capabilities".to_owned(),
            });
        }
        self.notification("notifications/initialized", json!({})).await?;
        let tools = self.request("tools/list", json!({}), None).await?;
        self.validate_tools(&tools)?;
        Ok(())
    }

    pub(crate) fn supports(&self, name: &str) -> bool {
        self.tools.contains(name) && INTERNAL_TOOL_NAMES.contains(&name)
    }

    pub(crate) async fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        cancellation: &CancellationFlag,
    ) -> Result<ObscuraToolResponse, ObscuraError> {
        if !self.supports(name) {
            return Err(ObscuraError::ToolNotSupported { name: name.to_owned() });
        }
        let result = self
            .request(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                Some(cancellation),
            )
            .await?;
        let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
        let mut text = String::new();
        if let Some(content) = result.get("content").and_then(Value::as_array) {
            for item in content {
                if item.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }
                if let Some(value) = item.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(value);
                }
            }
        }
        if text.is_empty() {
            return Err(ObscuraError::Protocol {
                message: "MCP tool response contained no text content".to_owned(),
            });
        }
        if text.len() > MAX_TOOL_RESULT_BYTES {
            return Err(ObscuraError::ResultLimit { limit: MAX_TOOL_RESULT_BYTES });
        }
        Ok(ObscuraToolResponse { text, is_error })
    }

    pub(crate) async fn shutdown(&mut self) -> Result<(), ObscuraError> {
        // Obscura v0.2.1 has no protocol shutdown method. Closing stdin is its documented EOF
        // boundary; kill_on_drop remains the fallback if it does not exit promptly.
        self.stdin.take();
        let wait = timeout(Duration::from_secs(2), self.child.wait()).await;
        match wait {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                self.abort_stderr();
                return Err(ObscuraError::Process {
                    operation: "wait for Obscura shutdown".to_owned(),
                    message: error.to_string(),
                });
            }
            Err(_) => {
                self.child.kill().await.map_err(|error| ObscuraError::Process {
                    operation: "terminate Obscura".to_owned(),
                    message: error.to_string(),
                })?;
                let _ = self.child.wait().await;
            }
        }
        self.abort_stderr();
        Ok(())
    }

    pub(crate) fn process_exited(&mut self) -> Result<bool, ObscuraError> {
        self.child.try_wait().map(|status| status.is_some()).map_err(|error| {
            ObscuraError::Process {
                operation: "inspect Obscura process".to_owned(),
                message: error.to_string(),
            }
        })
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<(), ObscuraError> {
        self.write_frame(json!({"jsonrpc":"2.0","method":method,"params":params})).await
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        cancellation: Option<&CancellationFlag>,
    ) -> Result<Value, ObscuraError> {
        if cancellation.is_some_and(CancellationFlag::is_cancelled) {
            return Err(ObscuraError::Cancelled);
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.write_frame(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})).await?;
        let response = if let Some(cancellation) = cancellation {
            tokio::select! {
                _ = wait_for_cancellation(cancellation) => return Err(ObscuraError::Cancelled),
                response = timeout(Duration::from_millis(MAX_MCP_REQUEST_MS), self.read_response(id)) => response,
            }
        } else {
            timeout(Duration::from_millis(MAX_MCP_REQUEST_MS), self.read_response(id)).await
        };
        response.map_err(|_| ObscuraError::Timeout { operation: method.to_owned() })?
    }

    async fn read_response(&mut self, expected_id: u64) -> Result<Value, ObscuraError> {
        let frame = read_frame(&mut self.stdout).await?;
        let message: Value = serde_json::from_slice(&frame).map_err(|_| ObscuraError::Framing {
            message: "MCP server sent invalid JSON".to_owned(),
        })?;
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err(ObscuraError::Protocol {
                message: "MCP message did not use JSON-RPC 2.0".to_owned(),
            });
        }
        if message.get("id").is_none() {
            let method = message.get("method").and_then(Value::as_str).unwrap_or("");
            return Err(ObscuraError::UnsupportedServerMessage { method: bounded(method, 128) });
        }
        let id = message.get("id").and_then(Value::as_u64);
        if id != Some(expected_id) {
            return Err(ObscuraError::Protocol {
                message: "MCP response id did not match the in-flight request".to_owned(),
            });
        }
        if let Some(error) = message.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32000);
            let message = error.get("message").and_then(Value::as_str).unwrap_or("MCP error");
            return Err(ObscuraError::McpError { code, message: bounded(message, 512) });
        }
        message.get("result").cloned().ok_or_else(|| ObscuraError::Protocol {
            message: "MCP response omitted result".to_owned(),
        })
    }

    async fn write_frame(&mut self, message: Value) -> Result<(), ObscuraError> {
        let mut frame = serde_json::to_vec(&message).map_err(|error| ObscuraError::Framing {
            message: format!("MCP request serialization failed: {error}"),
        })?;
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ObscuraError::FrameLimit { limit: MAX_FRAME_BYTES });
        }
        frame.push(b'\n');
        let stdin = self.stdin.as_mut().ok_or_else(|| ObscuraError::Process {
            operation: "write to Obscura".to_owned(),
            message: "stdin is closed".to_owned(),
        })?;
        stdin.write_all(&frame).await.map_err(|error| ObscuraError::Process {
            operation: "write to Obscura".to_owned(),
            message: error.to_string(),
        })?;
        stdin.flush().await.map_err(|error| ObscuraError::Process {
            operation: "flush Obscura request".to_owned(),
            message: error.to_string(),
        })
    }

    fn validate_tools(&mut self, response: &Value) -> Result<(), ObscuraError> {
        let tools = response.get("tools").and_then(Value::as_array).ok_or_else(|| {
            ObscuraError::Protocol { message: "tools/list response omitted tools".to_owned() }
        })?;
        for name in [
            "browser_navigate",
            "browser_snapshot",
            "browser_wait_for",
            "browser_search",
            "browser_tab_list",
            "browser_evaluate",
        ] {
            let tool = tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
                .ok_or_else(|| ObscuraError::Protocol {
                    message: format!("required Obscura tool {name} is missing"),
                })?;
            validate_tool_schema(name, tool.get("inputSchema")).map_err(|message| {
                ObscuraError::Protocol {
                    message: format!("schema for {name} is incompatible: {message}"),
                }
            })?;
            self.tools.insert(name.to_owned());
        }
        for name in ["browser_storage_state", "browser_set_storage_state"] {
            if let Some(tool) =
                tools.iter().find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
            {
                validate_tool_schema(name, tool.get("inputSchema")).map_err(|message| {
                    ObscuraError::Protocol {
                        message: format!("schema for {name} is incompatible: {message}"),
                    }
                })?;
                self.tools.insert(name.to_owned());
            }
        }
        Ok(())
    }

    fn abort_stderr(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
        let _ = &self.stderr;
    }
}

async fn wait_for_cancellation(cancellation: &CancellationFlag) {
    while !cancellation.is_cancelled() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<Vec<u8>, ObscuraError> {
    let mut frame = Vec::with_capacity(1024);
    loop {
        let buffer = reader.fill_buf().await.map_err(|error| ObscuraError::Process {
            operation: "read Obscura stdout".to_owned(),
            message: error.to_string(),
        })?;
        if buffer.is_empty() {
            return Err(ObscuraError::Process {
                operation: "read Obscura stdout".to_owned(),
                message: "Obscura closed stdout during an MCP call".to_owned(),
            });
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let available = newline.map_or(buffer.len(), |index| index + 1);
        if frame.len().saturating_add(available) > MAX_FRAME_BYTES {
            return Err(ObscuraError::FrameLimit { limit: MAX_FRAME_BYTES });
        }
        frame.extend_from_slice(&buffer[..available]);
        reader.consume(available);
        if newline.is_some() {
            while frame.last() == Some(&b'\n') || frame.last() == Some(&b'\r') {
                let _ = frame.pop();
            }
            if frame.is_empty() {
                continue;
            }
            return Ok(frame);
        }
    }
}

fn validate_tool_schema(name: &str, schema: Option<&Value>) -> Result<(), String> {
    let schema = schema.ok_or("inputSchema is missing")?;
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        return Err("root type must be object".to_owned());
    }
    let expected_properties: &[(&str, &str)] = match name {
        "browser_navigate" => &[("url", "string"), ("waitUntil", "string")],
        "browser_snapshot" => &[("max_chars", "number")],
        "browser_wait_for" => &[("selector", "string"), ("timeout", "number")],
        "browser_search" => &[
            ("query", "string"),
            ("case_sensitive", "boolean"),
            ("limit", "number"),
            ("context_chars", "number"),
        ],
        "browser_tab_list" | "browser_storage_state" => &[],
        "browser_evaluate" => &[("expression", "string")],
        "browser_set_storage_state" => &[("state", "object")],
        _ => return Err("unknown schema contract".to_owned()),
    };
    let properties =
        schema.get("properties").and_then(Value::as_object).ok_or("properties is missing")?;
    if properties.len() != expected_properties.len()
        || expected_properties.iter().any(|(property, _)| !properties.contains_key(*property))
    {
        return Err("property set changed".to_owned());
    }
    for (property, expected_type) in expected_properties {
        if properties.get(*property).and_then(|value| value.get("type")).and_then(Value::as_str)
            != Some(*expected_type)
        {
            return Err(format!("property {property} changed type"));
        }
    }
    let expected_required: &[&str] = match name {
        "browser_navigate" => &["url"],
        "browser_wait_for" => &["selector"],
        "browser_search" => &["query"],
        "browser_evaluate" => &["expression"],
        "browser_set_storage_state" => &["state"],
        _ => &[],
    };
    let required = schema.get("required").and_then(Value::as_array);
    let required = required
        .map_or_else(Vec::new, |items| items.iter().filter_map(Value::as_str).collect::<Vec<_>>());
    if required != expected_required {
        return Err("required properties changed".to_owned());
    }
    if name == "browser_navigate"
        && properties.get("waitUntil").and_then(|value| value.get("enum"))
            != Some(&json!(["load", "domcontentloaded", "networkidle0"]))
    {
        return Err("waitUntil enum changed".to_owned());
    }
    Ok(())
}

fn bounded(value: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for character in value.chars() {
        if out.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn fake_mcp_script(protocol: &str) -> String {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {"protocolVersion": protocol, "capabilities": {}}
        });
        let tools = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {"tools": [
                {"name": "browser_navigate", "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "waitUntil": {"type": "string", "enum": ["load", "domcontentloaded", "networkidle0"]}
                    },
                    "required": ["url"]
                }},
                {"name": "browser_snapshot", "inputSchema": {
                    "type": "object", "properties": {"max_chars": {"type": "number"}}
                }},
                {"name": "browser_wait_for", "inputSchema": {
                    "type": "object",
                    "properties": {"selector": {"type": "string"}, "timeout": {"type": "number"}},
                    "required": ["selector"]
                }},
                {"name": "browser_search", "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "case_sensitive": {"type": "boolean"},
                        "limit": {"type": "number"},
                        "context_chars": {"type": "number"}
                    },
                    "required": ["query"]
                }},
                {"name": "browser_tab_list", "inputSchema": {
                    "type": "object", "properties": {}
                }},
                {"name": "browser_evaluate", "inputSchema": {
                    "type": "object",
                    "properties": {"expression": {"type": "string"}},
                    "required": ["expression"]
                }}
            ]}
        });
        let call = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": {"content": [{"type": "text", "text": "fake result"}]}
        });
        format!(
            "while IFS= read -r line; do case \"$line\" in *'\"method\":\"initialize\"'*) printf '%s\\n' '{initialize}';; *'\"method\":\"notifications/initialized\"'*) :;; *'\"method\":\"tools/list\"'*) printf '%s\\n' '{tools}';; *'\"method\":\"tools/call\"'*) printf '%s\\n' '{call}';; esac; done"
        )
    }

    #[test]
    fn schema_validation_rejects_changed_required_properties() {
        let schema = json!({
            "type": "object",
            "properties": {"url": {"type": "string"}, "waitUntil": {"type": "string", "enum": ["load", "domcontentloaded", "networkidle0"]}},
            "required": ["url", "waitUntil"]
        });
        assert!(validate_tool_schema("browser_navigate", Some(&schema)).is_err());
    }

    #[test]
    fn schema_validation_ignores_provider_descriptions_but_not_shape() {
        let schema = json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "untrusted"}, "waitUntil": {"type": "string", "enum": ["load", "domcontentloaded", "networkidle0"], "description": "untrusted"}},
            "required": ["url"]
        });
        assert!(validate_tool_schema("browser_navigate", Some(&schema)).is_ok());
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn fake_stdio_provider_completes_handshake_and_bounded_call() {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let profile = std::env::temp_dir().join(format!(
            "aether-obscura-protocol-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&profile).unwrap();
        let script = fake_mcp_script(crate::MCP_PROTOCOL_VERSION);
        let mut connection =
            McpConnection::spawn(Path::new("/bin/sh"), &["-c", script.as_str()], &profile)
                .await
                .unwrap();
        connection.handshake(crate::MCP_PROTOCOL_VERSION).await.unwrap();
        let result = connection
            .call_tool("browser_snapshot", json!({}), &CancellationFlag::new())
            .await
            .unwrap();
        assert_eq!(result.text, "fake result");
        connection.shutdown().await.unwrap();
        let _ = fs::remove_dir_all(profile);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn fake_stdio_provider_with_wrong_protocol_is_rejected() {
        static NEXT: AtomicU64 = AtomicU64::new(10_000);
        let profile = std::env::temp_dir().join(format!(
            "aether-obscura-protocol-wrong-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&profile).unwrap();
        let script = fake_mcp_script("1999-01-01");
        let mut connection =
            McpConnection::spawn(Path::new("/bin/sh"), &["-c", script.as_str()], &profile)
                .await
                .unwrap();
        assert!(matches!(
            connection.handshake(crate::MCP_PROTOCOL_VERSION).await,
            Err(ObscuraError::Protocol { .. })
        ));
        connection.shutdown().await.unwrap();
        let _ = fs::remove_dir_all(profile);
    }

    #[tokio::test]
    async fn framing_rejects_a_line_over_the_frame_limit() {
        let mut input = vec![b'x'; MAX_FRAME_BYTES + 1];
        input.push(b'\n');
        let mut reader = BufReader::new(input.as_slice());
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(ObscuraError::FrameLimit { limit: MAX_FRAME_BYTES })
        ));
    }
}
