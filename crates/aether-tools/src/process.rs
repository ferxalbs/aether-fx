use std::{process::Stdio, sync::Arc, time::Duration};

use aether_core::{CancellationFlag, CoreError, PermissionClass, ToolCallId, ToolExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::sleep,
};

use crate::common::{
    MAX_PROCESS_READ_BYTES, PROCESS_STREAM_BUFFER_BYTES, ProcessHandle, ToolInternalError,
    Workspace, bounded_limit, resolve_existing_blocking,
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessOperation {
    Start,
    Read,
    Write,
    Signal,
    Kill,
    Status,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessStream {
    Stdout,
    Stderr,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProcessInput {
    pub operation: ProcessOperation,
    pub program: Option<String>,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub process_id: Option<u64>,
    pub stream: Option<ProcessStream>,
    pub data: Option<String>,
    pub signal: Option<String>,
    pub max_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProcessOutput {
    pub process_id: u64,
    pub operation: String,
    pub running: Option<bool>,
    pub exit_code: Option<i32>,
    pub data: Option<String>,
    pub eof: Option<bool>,
    pub truncated: Option<bool>,
    pub timed_out: Option<bool>,
    pub buffered_bytes: Option<usize>,
    pub dropped_bytes: Option<u64>,
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    let parsed: ProcessInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    if let Err(error) =
        workspace.require_permit(&context, &call_id, "process", PermissionClass::ProcessPersistent)
    {
        return workspace.result_error(call_id, error);
    }
    let cancellation = context.cancellation().clone();
    match parsed.operation {
        ProcessOperation::Start => start(workspace, call_id, parsed, cancellation).await,
        ProcessOperation::Read => read(workspace, call_id, parsed, cancellation).await,
        ProcessOperation::Write => write(workspace, call_id, parsed, cancellation).await,
        ProcessOperation::Signal => signal(workspace, call_id, parsed, cancellation).await,
        ProcessOperation::Kill => kill(workspace, call_id, parsed, cancellation).await,
        ProcessOperation::Status => status(workspace, call_id, parsed, cancellation).await,
    }
}

async fn start(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
        return workspace.result_error(call_id, error);
    }
    let program = match input.program {
        Some(value) if !value.is_empty() => value,
        _ => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("process start requires program".to_owned()),
            );
        }
    };
    let args = input.args.unwrap_or_default();
    if args.len() > 256 {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("process accepts at most 256 arguments".to_owned()),
        );
    }
    let (cwd_display, cwd) = match input.cwd.as_deref() {
        Some(path) => match resolve_existing_blocking(workspace, path.to_owned()).await {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        },
        None => (".".to_owned(), workspace.root().to_owned()),
    };
    if !cwd.is_dir() {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("process cwd is not a directory".to_owned()),
        );
    }
    if cancellation.is_cancelled() {
        return workspace.result_error(call_id, ToolInternalError::Core(CoreError::Cancelled));
    }
    let mut command = Command::new(&program);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error.into()),
    };
    let Some(stdout) = child.stdout.take() else {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("process stdout is unavailable".to_owned()),
        );
    };
    let Some(stderr) = child.stderr.take() else {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("process stderr is unavailable".to_owned()),
        );
    };
    let stdout_buffer = Arc::new(crate::common::OutputBuffer::new(PROCESS_STREAM_BUFFER_BYTES));
    let stderr_buffer = Arc::new(crate::common::OutputBuffer::new(PROCESS_STREAM_BUFFER_BYTES));
    tokio::spawn(drain_stream(stdout, stdout_buffer.clone()));
    tokio::spawn(drain_stream(stderr, stderr_buffer.clone()));
    let stdin = child.stdin.take();
    let handle = Arc::new(ProcessHandle {
        child: tokio::sync::Mutex::new(child),
        stdin: tokio::sync::Mutex::new(stdin),
        stdout: stdout_buffer,
        stderr: stderr_buffer,
        program: program.clone(),
        args: args.clone(),
        cwd: cwd.clone(),
    });
    if cancellation.is_cancelled() {
        handle.terminate().await;
        return workspace.result_error(call_id, ToolInternalError::Core(CoreError::Cancelled));
    }
    let process_id = workspace.allocate_process_id();
    if let Err(error) = workspace.processes().insert(process_id, handle.clone()) {
        handle.terminate().await;
        return workspace.result_error(call_id, error);
    }
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "start".to_owned(),
            running: Some(true),
            exit_code: None,
            data: Some(cwd_display),
            eof: None,
            truncated: None,
            timed_out: None,
            buffered_bytes: Some(0),
            dropped_bytes: Some(0),
        }),
        workspace.max_output_bytes(),
    )
}

async fn drain_stream<R>(mut reader: R, buffer: Arc<crate::common::OutputBuffer>)
where
    R: AsyncRead + Unpin,
{
    let mut bytes = [0_u8; 8192];
    loop {
        match reader.read(&mut bytes).await {
            Ok(0) | Err(_) => break,
            Ok(count) => buffer.push(&bytes[..count]),
        }
    }
    buffer.mark_eof();
}

async fn read(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    let process_id = match input.process_id {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("process read requires process_id".to_owned()),
            );
        }
    };
    let stream = input.stream.unwrap_or(ProcessStream::Stdout);
    // Leave room for JSON metadata and escaping so buffered_bytes/dropped_bytes remain visible
    // even when the stream contains control characters.
    let max_read_output = (workspace.max_output_bytes() / 8).max(1);
    let max_bytes = bounded_limit(input.max_bytes, MAX_PROCESS_READ_BYTES.min(max_read_output));
    let timeout_duration =
        Duration::from_millis(input.timeout_ms.unwrap_or(1000).clamp(1, 600_000));
    let Some(handle) = workspace.processes().get(process_id) else {
        return workspace
            .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
    };
    let buffer = match stream {
        ProcessStream::Stdout => handle.stdout.clone(),
        ProcessStream::Stderr => handle.stderr.clone(),
    };
    let started = tokio::time::Instant::now();
    loop {
        if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
            return workspace.result_error(call_id, error);
        }
        let value = buffer.take(max_bytes);
        if !value.bytes.is_empty() || value.eof {
            return process_read_result(workspace, call_id, process_id, value, false);
        }
        let remaining = timeout_duration.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return process_read_result(
                workspace,
                call_id,
                process_id,
                crate::common::BufferedRead {
                    bytes: Vec::new(),
                    eof: false,
                    dropped_bytes: value.dropped_bytes,
                    buffered_bytes: value.buffered_bytes,
                },
                true,
            );
        }
        let notified = buffer.notifier().notified();
        if buffer.is_ready() {
            continue;
        }
        tokio::select! {
            _ = sleep(remaining.min(Duration::from_millis(10))) => {
                if started.elapsed() >= timeout_duration {
                return process_read_result(
                    workspace,
                    call_id,
                    process_id,
                    crate::common::BufferedRead {
                        bytes: Vec::new(),
                        eof: false,
                        dropped_bytes: value.dropped_bytes,
                        buffered_bytes: value.buffered_bytes,
                    },
                    true,
                );
                }
            }
            _ = notified => {}
        }
    }
}

fn process_read_result(
    workspace: &Workspace,
    call_id: ToolCallId,
    process_id: u64,
    value: crate::common::BufferedRead,
    timed_out: bool,
) -> aether_core::ToolResult {
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "read".to_owned(),
            running: None,
            exit_code: None,
            data: Some(String::from_utf8_lossy(&value.bytes).into_owned()),
            eof: Some(value.eof),
            truncated: Some(false),
            timed_out: Some(timed_out),
            buffered_bytes: Some(value.buffered_bytes),
            dropped_bytes: Some(value.dropped_bytes),
        }),
        workspace.max_output_bytes(),
    )
}

async fn write(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    let process_id = match input.process_id {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("process write requires process_id".to_owned()),
            );
        }
    };
    let data = input.data.unwrap_or_default();
    if data.len() > MAX_PROCESS_READ_BYTES {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("process write exceeds the bounded input limit".to_owned()),
        );
    }
    let Some(handle) = workspace.processes().get(process_id) else {
        return workspace
            .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
    };
    let mut stdin = handle.stdin.lock().await;
    let Some(stdin) = stdin.as_mut() else {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("process stdin is unavailable".to_owned()),
        );
    };
    let mut write_future = Box::pin(stdin.write_all(data.as_bytes()));
    let write_result = loop {
        if cancellation.is_cancelled() {
            drop(write_future);
            break Err(ToolInternalError::Core(CoreError::Cancelled));
        }
        tokio::select! {
            result = &mut write_future => break result.map_err(ToolInternalError::Io),
            _ = sleep(Duration::from_millis(10)) => {}
        }
    };
    if let Err(error) = write_result {
        return workspace.result_error(call_id, error);
    }
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "write".to_owned(),
            running: Some(true),
            exit_code: None,
            data: Some(data),
            eof: None,
            truncated: Some(false),
            timed_out: Some(false),
            buffered_bytes: None,
            dropped_bytes: None,
        }),
        workspace.max_output_bytes(),
    )
}

async fn signal(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    if input.signal.as_deref() != Some("kill") {
        return workspace.result_error(
            call_id,
            ToolInternalError::Core(CoreError::Unsupported {
                operation: "only kill signal is supported in v0.1".to_owned(),
            }),
        );
    }
    kill(workspace, call_id, input, cancellation).await
}

async fn kill(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    let process_id = match input.process_id {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("process kill requires process_id".to_owned()),
            );
        }
    };
    if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
        return workspace.result_error(call_id, error);
    }
    let Some(handle) = workspace.processes().remove(process_id) else {
        return workspace
            .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
    };
    handle.terminate().await;
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "kill".to_owned(),
            running: Some(false),
            exit_code: None,
            data: None,
            eof: None,
            truncated: None,
            timed_out: None,
            buffered_bytes: None,
            dropped_bytes: None,
        }),
        workspace.max_output_bytes(),
    )
}

async fn status(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
    cancellation: CancellationFlag,
) -> aether_core::ToolResult {
    let process_id = match input.process_id {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("process status requires process_id".to_owned()),
            );
        }
    };
    if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
        return workspace.result_error(call_id, error);
    }
    let Some(handle) = workspace.processes().get(process_id) else {
        return workspace
            .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
    };
    let mut child = handle.child.lock().await;
    let status = match child.try_wait() {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error.into()),
    };
    let command = if handle.args.is_empty() {
        handle.program.clone()
    } else {
        format!("{} {}", handle.program, handle.args.join(" "))
    };
    let buffered = handle.stdout.take(0);
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "status".to_owned(),
            running: Some(status.is_none()),
            exit_code: status.and_then(|value| value.code()),
            data: Some(format!("command={command}; cwd={}", handle.cwd.display())),
            eof: None,
            truncated: None,
            timed_out: None,
            buffered_bytes: Some(buffered.buffered_bytes),
            dropped_bytes: Some(buffered.dropped_bytes),
        }),
        workspace.max_output_bytes(),
    )
}
