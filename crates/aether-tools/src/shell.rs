use std::{
    process::Stdio,
    time::{Duration, Instant},
};

use aether_core::{CoreError, PermissionClass, ToolCallId, ToolExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

use crate::common::{MAX_INPUT_BYTES, Workspace, bounded_limit, resolve_existing_blocking};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShellInput {
    pub program: String,
    pub args: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub max_output_bytes: Option<usize>,
    pub timeout_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShellOutput {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub exit_code: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u128,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

pub(crate) struct BoundedBytes {
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

pub(crate) async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> std::io::Result<BoundedBytes> {
    let limit = limit.clamp(1, MAX_INPUT_BYTES);
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut buffer = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        if bytes.len() < limit {
            let retained = (limit - bytes.len()).min(count);
            bytes.extend_from_slice(&buffer[..retained]);
            if retained < count {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    Ok(BoundedBytes { bytes, truncated })
}

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    let parsed: ShellInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    if parsed.program.is_empty() {
        return workspace.result_error(
            call_id,
            crate::common::ToolInternalError::Input("shell program must not be empty".to_owned()),
        );
    }
    if let Err(error) =
        workspace.require_permit(&context, &call_id, "shell", PermissionClass::ProcessExecute)
    {
        return workspace.result_error(call_id, error);
    }
    let args = parsed.args.unwrap_or_default();
    if args.len() > 256 {
        return workspace.result_error(
            call_id,
            crate::common::ToolInternalError::Input(
                "shell accepts at most 256 arguments".to_owned(),
            ),
        );
    }
    let (cwd_display, cwd) = match parsed.cwd.as_deref() {
        Some(path) => match resolve_existing_blocking(workspace, path.to_owned()).await {
            Ok(value) => value,
            Err(error) => return workspace.result_error(call_id, error),
        },
        None => (".".to_owned(), workspace.root().to_owned()),
    };
    if !cwd.is_dir() {
        return workspace.result_error(
            call_id,
            crate::common::ToolInternalError::Input("shell cwd is not a directory".to_owned()),
        );
    }
    if context.cancellation().is_cancelled() {
        return workspace
            .result_error(call_id, crate::common::ToolInternalError::Core(CoreError::Cancelled));
    }
    let limit = bounded_limit(parsed.max_output_bytes, workspace.max_output_bytes());
    let timeout_duration =
        Duration::from_millis(parsed.timeout_ms.unwrap_or(120_000).clamp(1, 600_000));
    let started = Instant::now();
    let mut command = Command::new(&parsed.program);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error.into()),
    };
    let stdout = match child.stdout.take() {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(
                    "process did not provide stdout".to_owned(),
                ),
            );
        }
    };
    let stderr = match child.stderr.take() {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Input(
                    "process did not provide stderr".to_owned(),
                ),
            );
        }
    };
    let stdout_task = tokio::spawn(read_bounded(stdout, limit));
    let stderr_task = tokio::spawn(read_bounded(stderr, limit));
    let deadline = tokio::time::Instant::now() + timeout_duration;
    let mut child_wait = Box::pin(child.wait());
    let status = loop {
        if context.cancellation().is_cancelled() {
            drop(child_wait);
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return workspace.result_error(
                call_id,
                crate::common::ToolInternalError::Core(CoreError::Cancelled),
            );
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            drop(child_wait);
            let _ = child.kill().await;
            let _ = child.wait().await;
            break (None, true);
        }
        tokio::select! {
            result = &mut child_wait => match result {
                Ok(status) => break (status.code(), false),
                Err(error) => return workspace.result_error(call_id, error.into()),
            },
            _ = tokio::time::sleep(remaining.min(Duration::from_millis(10))) => {}
        }
    };
    let stdout = match stdout_task.await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return workspace.result_error(call_id, error.into()),
        Err(error) => {
            return workspace
                .result_error(call_id, crate::common::ToolInternalError::Input(error.to_string()));
        }
    };
    let stderr = match stderr_task.await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return workspace.result_error(call_id, error.into()),
        Err(error) => {
            return workspace
                .result_error(call_id, crate::common::ToolInternalError::Input(error.to_string()));
        }
    };
    let timed_out = status.1;
    let output = ShellOutput {
        program: parsed.program,
        args,
        cwd: cwd_display,
        exit_code: status.0,
        success: !timed_out && status.0 == Some(0),
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        duration_ms: started.elapsed().as_millis(),
        timed_out,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
    };
    aether_core::ToolResult::success_json(
        call_id,
        serde_json::json!(output),
        workspace.max_output_bytes(),
    )
}
