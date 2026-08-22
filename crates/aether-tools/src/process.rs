use std::{process::Stdio, sync::Arc, time::Duration};

use aether_core::{CancellationFlag, CoreError, PermissionClass, ToolCallId, ToolExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    process::Command,
    time::{sleep, timeout},
};

use crate::common::{
    MAX_PROCESS_READ_BYTES, PROCESS_STREAM_BUFFER_BYTES, ProcessHandle, ProcessState,
    ProcessTermination, ProcessTerminationError, ToolInternalError, Workspace, bounded_limit,
    resolve_existing_blocking,
};

pub(crate) const PROCESS_TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);
const PROCESS_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProcessShutdownReport {
    pub attempted: usize,
    pub terminated: usize,
    pub failures: Vec<ProcessShutdownFailure>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessShutdownFailure {
    pub process_id: u64,
    pub error: String,
}

impl ProcessShutdownReport {
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn failure_summary(&self) -> String {
        self.failures
            .iter()
            .map(|failure| format!("{}: {}", failure.process_id, failure.error))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

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
    let stdout_drain = tokio::spawn(drain_stream(stdout, stdout_buffer.clone()));
    let stderr_drain = tokio::spawn(drain_stream(stderr, stderr_buffer.clone()));
    let stdin = child.stdin.take();
    let handle = Arc::new(ProcessHandle {
        child: tokio::sync::Mutex::new(child),
        stdin: tokio::sync::Mutex::new(stdin),
        stdout: stdout_buffer,
        stderr: stderr_buffer,
        drains: tokio::sync::Mutex::new(vec![stdout_drain, stderr_drain]),
        state: std::sync::Mutex::new(ProcessState::Running),
        program: program.clone(),
        args: args.clone(),
        cwd: cwd.clone(),
        #[cfg(test)]
        test_termination: std::sync::Mutex::new(None),
    });
    let process_id = workspace.allocate_process_id();
    if let Err(error) = workspace.processes().insert(process_id, handle.clone()) {
        if let Err(termination_error) = handle.terminate().await {
            return workspace.result_error(
                call_id,
                ToolInternalError::ProcessTermination {
                    process_id,
                    error: termination_error.to_string(),
                },
            );
        }
        return workspace.result_error(call_id, error);
    }
    if cancellation.is_cancelled() {
        return finish_cancelled_start(workspace, call_id, process_id, handle).await;
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

async fn finish_cancelled_start(
    workspace: &Workspace,
    call_id: ToolCallId,
    process_id: u64,
    handle: Arc<ProcessHandle>,
) -> aether_core::ToolResult {
    match handle.terminate().await {
        Ok(_) => {
            workspace.processes().remove(process_id);
            workspace.result_error(call_id, ToolInternalError::Core(CoreError::Cancelled))
        }
        Err(error) => workspace.result_error(
            call_id,
            ToolInternalError::ProcessTermination { process_id, error: error.to_string() },
        ),
    }
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

async fn finish_drains(handle: &ProcessHandle) {
    let drains = {
        let mut drains = handle.drains.lock().await;
        std::mem::take(&mut *drains)
    };
    for mut drain in drains {
        if timeout(PROCESS_DRAIN_TIMEOUT, &mut drain).await.is_err() {
            drain.abort();
            let _ = drain.await;
        }
    }
}

fn set_process_state(handle: &ProcessHandle, state: ProcessState) {
    if let Ok(mut current) = handle.state.lock() {
        *current = state;
    }
}

pub(crate) async fn terminate_process(
    handle: &ProcessHandle,
) -> Result<ProcessTermination, ProcessTerminationError> {
    #[cfg(test)]
    {
        let mode = handle.test_termination.lock().ok().and_then(|mut mode| mode.take());
        if let Some(mode) = mode {
            set_process_state(handle, ProcessState::TerminationFailed);
            return match mode {
                crate::common::TestTerminationMode::Fail => {
                    Err(ProcessTerminationError::Test("simulated kill failure".to_owned()))
                }
                crate::common::TestTerminationMode::Timeout => {
                    Err(ProcessTerminationError::Timeout {
                        timeout_ms: PROCESS_TERMINATION_TIMEOUT.as_millis() as u64,
                    })
                }
            };
        }
    }

    let mut child = handle.child.lock().await;
    match child.try_wait() {
        Ok(Some(status)) => {
            set_process_state(handle, ProcessState::Exited);
            drop(child);
            finish_drains(handle).await;
            return Ok(ProcessTermination::AlreadyExited { exit_code: status.code() });
        }
        Ok(None) => {}
        Err(error) => {
            set_process_state(handle, ProcessState::TerminationFailed);
            return Err(ProcessTerminationError::Wait(error));
        }
    }

    set_process_state(handle, ProcessState::TerminationRequested);
    if let Err(error) = child.start_kill() {
        match child.try_wait() {
            Ok(Some(status)) => {
                set_process_state(handle, ProcessState::Exited);
                drop(child);
                finish_drains(handle).await;
                return Ok(ProcessTermination::AlreadyExited { exit_code: status.code() });
            }
            Ok(None) => {
                set_process_state(handle, ProcessState::TerminationFailed);
                return Err(ProcessTerminationError::Kill(error));
            }
            Err(wait_error) => {
                set_process_state(handle, ProcessState::TerminationFailed);
                return Err(ProcessTerminationError::Wait(wait_error));
            }
        }
    }

    match timeout(PROCESS_TERMINATION_TIMEOUT, child.wait()).await {
        Ok(Ok(status)) => {
            set_process_state(handle, ProcessState::Exited);
            drop(child);
            finish_drains(handle).await;
            Ok(ProcessTermination::Terminated { exit_code: status.code() })
        }
        Ok(Err(error)) => {
            set_process_state(handle, ProcessState::TerminationFailed);
            Err(ProcessTerminationError::Wait(error))
        }
        Err(_) => {
            set_process_state(handle, ProcessState::TerminationFailed);
            Err(ProcessTerminationError::Timeout {
                timeout_ms: PROCESS_TERMINATION_TIMEOUT.as_millis() as u64,
            })
        }
    }
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
    let Some(handle) = workspace.processes().get(process_id) else {
        return workspace
            .result_error(call_id, ToolInternalError::Input("unknown process_id".to_owned()));
    };
    let termination = match handle.terminate().await {
        Ok(value) => value,
        Err(error) => {
            return workspace.result_error(
                call_id,
                ToolInternalError::ProcessTermination { process_id, error: error.to_string() },
            );
        }
    };
    workspace.processes().remove(process_id);
    let exit_code = match termination {
        ProcessTermination::AlreadyExited { exit_code }
        | ProcessTermination::Terminated { exit_code } => exit_code,
    };
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(ProcessOutput {
            process_id,
            operation: "kill".to_owned(),
            running: Some(false),
            exit_code,
            data: Some(match termination {
                ProcessTermination::AlreadyExited { .. } => "already_exited".to_owned(),
                ProcessTermination::Terminated { .. } => "terminated".to_owned(),
            }),
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
    if status.is_some() {
        set_process_state(&handle, ProcessState::Exited);
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{OutputBuffer, TestTerminationMode};
    use std::sync::Mutex as StdMutex;

    async fn fixture_process(running: bool) -> Arc<ProcessHandle> {
        let (program, args) = if cfg!(windows) {
            if running {
                ("cmd", vec!["/C".to_owned(), "ping -n 30 127.0.0.1 >NUL".to_owned()])
            } else {
                ("cmd", vec!["/C".to_owned(), "exit 0".to_owned()])
            }
        } else if running {
            ("sh", vec!["-c".to_owned(), "sleep 30".to_owned()])
        } else {
            ("true", Vec::new())
        };
        let mut child = Command::new(program)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();
        let stdout_buffer = Arc::new(OutputBuffer::new(PROCESS_STREAM_BUFFER_BYTES));
        let stderr_buffer = Arc::new(OutputBuffer::new(PROCESS_STREAM_BUFFER_BYTES));
        let drains = vec![
            tokio::spawn(drain_stream(stdout, stdout_buffer.clone())),
            tokio::spawn(drain_stream(stderr, stderr_buffer.clone())),
        ];
        Arc::new(ProcessHandle {
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(None),
            stdout: stdout_buffer,
            stderr: stderr_buffer,
            drains: tokio::sync::Mutex::new(drains),
            state: StdMutex::new(ProcessState::Running),
            program: program.to_owned(),
            args,
            cwd: std::env::current_dir().unwrap(),
            test_termination: StdMutex::new(None),
        })
    }

    fn context(call_id: &str) -> CancellationFlag {
        let _ = call_id;
        CancellationFlag::new()
    }

    #[tokio::test]
    async fn terminate_running_process_confirms_exit_and_finishes_drains() {
        let handle = fixture_process(true).await;
        let result = handle.terminate().await.unwrap();
        assert!(matches!(result, ProcessTermination::Terminated { .. }));
        assert_eq!(*handle.state.lock().unwrap(), ProcessState::Exited);
        assert!(handle.drains.lock().await.is_empty());
    }

    #[tokio::test]
    async fn terminate_already_exited_process_is_successful_terminal_state() {
        let handle = fixture_process(false).await;
        {
            let mut child = handle.child.lock().await;
            child.wait().await.unwrap();
        }
        let result = handle.terminate().await.unwrap();
        assert!(matches!(result, ProcessTermination::AlreadyExited { .. }));
        assert_eq!(*handle.state.lock().unwrap(), ProcessState::Exited);
    }

    #[tokio::test]
    async fn kill_failure_remains_visible_in_registry() {
        let root =
            std::env::temp_dir().join(format!("aether-process-failure-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::new(&root).unwrap();
        let handle = fixture_process(true).await;
        *handle.test_termination.lock().unwrap() = Some(TestTerminationMode::Fail);
        workspace.processes().insert(7, handle.clone()).unwrap();
        let result = kill(
            &workspace,
            ToolCallId::new("kill-failure").unwrap(),
            ProcessInput {
                operation: ProcessOperation::Kill,
                program: None,
                args: None,
                cwd: None,
                process_id: Some(7),
                stream: None,
                data: None,
                signal: None,
                max_bytes: None,
                timeout_ms: None,
            },
            context("kill-failure"),
        )
        .await;
        assert!(!result.ok);
        assert_eq!(result.error.unwrap().code, "termination_failed");
        assert!(workspace.processes().get(7).is_some());

        *handle.test_termination.lock().unwrap() = None;
        handle.terminate().await.unwrap();
        workspace.processes().remove(7);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn termination_timeout_is_visible_and_does_not_remove_tracking() {
        let root =
            std::env::temp_dir().join(format!("aether-process-timeout-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::new(&root).unwrap();
        let handle = fixture_process(true).await;
        *handle.test_termination.lock().unwrap() = Some(TestTerminationMode::Timeout);
        workspace.processes().insert(8, handle.clone()).unwrap();
        let report = workspace.shutdown_processes().await;
        assert_eq!(report.attempted, 1);
        assert_eq!(report.terminated, 0);
        assert_eq!(report.failures[0].process_id, 8);
        assert!(workspace.processes().get(8).is_some());

        *handle.test_termination.lock().unwrap() = None;
        handle.terminate().await.unwrap();
        workspace.processes().remove(8);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shutdown_attempts_every_owned_process_and_aggregates_failures() {
        let root =
            std::env::temp_dir().join(format!("aether-process-shutdown-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let workspace = Workspace::new(&root).unwrap();
        let failed = fixture_process(true).await;
        let healthy = fixture_process(true).await;
        *failed.test_termination.lock().unwrap() = Some(TestTerminationMode::Fail);
        workspace.processes().insert(9, failed.clone()).unwrap();
        workspace.processes().insert(10, healthy).unwrap();

        let report = workspace.shutdown_processes().await;
        assert_eq!(report.attempted, 2);
        assert_eq!(report.terminated, 1);
        assert_eq!(report.failures.len(), 1);
        assert!(workspace.processes().get(9).is_some());
        assert!(workspace.processes().get(10).is_none());

        *failed.test_termination.lock().unwrap() = None;
        failed.terminate().await.unwrap();
        workspace.processes().remove(9);
        let _ = std::fs::remove_dir_all(root);
    }
}
