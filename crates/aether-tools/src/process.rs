use std::{process::Stdio, time::Duration};

use aether_core::{PermissionClass, ToolCallId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::timeout,
};

use crate::{
    common::{MAX_PROCESS_READ_BYTES, ProcessEntry, ToolInternalError, Workspace, bounded_limit},
    shell::read_bounded,
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
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
) -> aether_core::ToolResult {
    let parsed: ProcessInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    if let Err(error) = workspace.authorize(
        PermissionClass::ProcessPersistent,
        "control persistent process",
        None,
        input,
    ) {
        return workspace.result_error(call_id, error);
    }
    match parsed.operation {
        ProcessOperation::Start => start(workspace, call_id, parsed).await,
        ProcessOperation::Read => read(workspace, call_id, parsed).await,
        ProcessOperation::Write => write(workspace, call_id, parsed).await,
        ProcessOperation::Signal => signal(workspace, call_id, parsed).await,
        ProcessOperation::Kill => kill(workspace, call_id, parsed).await,
        ProcessOperation::Status => status(workspace, call_id, parsed).await,
    }
}

async fn start(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
) -> aether_core::ToolResult {
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
        Some(path) => match workspace.resolve_existing(path) {
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
    let entry = ProcessEntry {
        stdin: child.stdin.take(),
        stdout: child.stdout.take(),
        stderr: child.stderr.take(),
        child,
        program,
        args,
        cwd,
    };
    let process_id = workspace.allocate_process_id();
    workspace.processes().lock().await.insert(process_id, entry);
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
        }),
        workspace.max_output_bytes(),
    )
}

async fn read(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
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
    let max_bytes = bounded_limit(input.max_bytes, MAX_PROCESS_READ_BYTES);
    let timeout_duration =
        Duration::from_millis(input.timeout_ms.unwrap_or(1000).clamp(1, 600_000));
    let mut processes = workspace.processes().lock().await;
    let entry = match processes.get_mut(&process_id) {
        Some(value) => value,
        None => {
            return workspace
                .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
        }
    };
    let read_result = match stream {
        ProcessStream::Stdout => {
            read_once(entry.stdout.as_mut(), max_bytes, timeout_duration).await
        }
        ProcessStream::Stderr => {
            read_once(entry.stderr.as_mut(), max_bytes, timeout_duration).await
        }
    };
    let value = match read_result {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "read".to_owned(),
            running: None,
            exit_code: None,
            data: Some(String::from_utf8_lossy(&value.0).into_owned()),
            eof: Some(value.0.is_empty() && !value.2),
            truncated: Some(value.1),
            timed_out: Some(value.2),
        }),
        workspace.max_output_bytes(),
    )
}

async fn read_once<R: AsyncRead + Unpin>(
    reader: Option<&mut R>,
    limit: usize,
    timeout_duration: Duration,
) -> Result<(Vec<u8>, bool, bool), ToolInternalError> {
    let Some(reader) = reader else {
        return Ok((Vec::new(), false, false));
    };
    let mut buffer = vec![0_u8; limit];
    match timeout(timeout_duration, reader.read(&mut buffer)).await {
        Ok(Ok(0)) => Ok((Vec::new(), false, false)),
        Ok(Ok(count)) => {
            buffer.truncate(count);
            Ok((buffer, count == limit, false))
        }
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Ok((Vec::new(), false, true)),
    }
}

async fn write(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
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
    let mut processes = workspace.processes().lock().await;
    let entry = match processes.get_mut(&process_id) {
        Some(value) => value,
        None => {
            return workspace
                .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
        }
    };
    let Some(stdin) = entry.stdin.as_mut() else {
        return workspace.result_error(
            call_id,
            ToolInternalError::Input("process stdin is unavailable".to_owned()),
        );
    };
    if let Err(error) = stdin.write_all(data.as_bytes()).await {
        return workspace.result_error(call_id, error.into());
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
        }),
        workspace.max_output_bytes(),
    )
}

async fn signal(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
) -> aether_core::ToolResult {
    if input.signal.as_deref() != Some("kill") {
        return workspace.result_error(
            call_id,
            ToolInternalError::Core(aether_core::CoreError::Unsupported {
                operation: "only kill signal is supported in v0.1".to_owned(),
            }),
        );
    }
    kill(workspace, call_id, input).await
}

async fn kill(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
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
    let mut entry = match workspace.processes().lock().await.remove(&process_id) {
        Some(value) => value,
        None => {
            return workspace
                .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
        }
    };
    if let Err(error) = entry.child.kill().await {
        return workspace.result_error(call_id, error.into());
    }
    let _ = entry.child.wait().await;
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
        }),
        workspace.max_output_bytes(),
    )
}

async fn status(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: ProcessInput,
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
    let mut processes = workspace.processes().lock().await;
    let entry = match processes.get_mut(&process_id) {
        Some(value) => value,
        None => {
            return workspace
                .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
        }
    };
    let status = match entry.child.try_wait() {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error.into()),
    };
    let command = if entry.args.is_empty() {
        entry.program.clone()
    } else {
        format!("{} {}", entry.program, entry.args.join(" "))
    };
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "status".to_owned(),
            running: Some(status.is_none()),
            exit_code: status.and_then(|value| value.code()),
            data: Some(format!("command={command}; cwd={}", entry.cwd.display())),
            eof: None,
            truncated: None,
            timed_out: None,
        }),
        workspace.max_output_bytes(),
    )
}

#[allow(dead_code)]
async fn _bounded_reader<R: AsyncRead + Unpin>(
    reader: R,
    limit: usize,
) -> std::io::Result<crate::shell::BoundedBytes> {
    read_bounded(reader, limit).await
}
