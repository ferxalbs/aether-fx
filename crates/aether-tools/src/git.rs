use std::{process::Stdio, time::Duration};

use aether_core::{CoreError, PermissionClass, ToolCallId, ToolExecutionContext};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{process::Command, time::sleep};

use crate::{
    common::{ToolInternalError, Workspace, bounded_limit},
    shell::{ShellOutput, read_bounded},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    Status,
    Diff,
    Show,
    Log,
    Branches,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GitInput {
    pub operation: GitOperation,
    pub reference: Option<String>,
    pub path: Option<String>,
    pub staged: Option<bool>,
    pub max_entries: Option<usize>,
}

pub type GitOutput = ShellOutput;

pub(crate) async fn execute(
    workspace: &Workspace,
    call_id: ToolCallId,
    input: Value,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    let parsed: GitInput = match workspace.parse(&input) {
        Ok(value) => value,
        Err(error) => return workspace.result_error(call_id, error),
    };
    execute_parsed(workspace, call_id, parsed, context).await
}

pub(crate) async fn execute_parsed(
    workspace: &Workspace,
    call_id: ToolCallId,
    parsed: GitInput,
    context: ToolExecutionContext,
) -> aether_core::ToolResult {
    if let Err(error) =
        workspace.require_permit(&context, &call_id, "git", PermissionClass::ReadOnly)
    {
        return workspace.result_error(call_id, error);
    }
    let cancellation = context.cancellation().clone();
    let args = match tokio::task::spawn_blocking({
        let workspace = workspace.clone();
        let parsed = parsed.clone();
        move || git_args(&workspace, &parsed)
    })
    .await
    {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return workspace.result_error(call_id, error),
        Err(error) => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input(format!("git path worker failed: {error}")),
            );
        }
    };
    if let Err(error) = cancellation.check().map_err(ToolInternalError::Core) {
        return workspace.result_error(call_id, error);
    }
    let limit = bounded_limit(None, workspace.max_output_bytes());
    let mut command = Command::new("git");
    command
        .args(&args)
        .current_dir(workspace.root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = match command.spawn() {
        Ok(value) => value,
        Err(error) => {
            return workspace
                .result_error(call_id, crate::common::ToolInternalError::Input(error.to_string()));
        }
    };
    let stdout = match child.stdout.take() {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("git stdout unavailable".to_owned()),
            );
        }
    };
    let stderr = match child.stderr.take() {
        Some(value) => value,
        None => {
            return workspace.result_error(
                call_id,
                ToolInternalError::Input("git stderr unavailable".to_owned()),
            );
        }
    };
    let stdout_task = tokio::spawn(read_bounded(stdout, limit));
    let stderr_task = tokio::spawn(read_bounded(stderr, limit));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
    let mut child_wait = Box::pin(child.wait());
    let (status, timed_out) = loop {
        if cancellation.is_cancelled() {
            drop(child_wait);
            let _ = child.kill().await;
            let _ = child.wait().await;
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return workspace.result_error(call_id, ToolInternalError::Core(CoreError::Cancelled));
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
                Ok(status) => break (Some(status), false),
                Err(error) => return workspace.result_error(call_id, error.into()),
            },
            _ = sleep(remaining.min(Duration::from_millis(10))) => {}
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
    let output = ShellOutput {
        program: "git".to_owned(),
        args,
        cwd: ".".to_owned(),
        exit_code: status.as_ref().and_then(std::process::ExitStatus::code),
        success: !timed_out && status.is_some_and(|status| status.success()),
        stdout: String::from_utf8_lossy(&stdout.bytes).into_owned(),
        stderr: String::from_utf8_lossy(&stderr.bytes).into_owned(),
        duration_ms: 0,
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

fn git_args(workspace: &Workspace, input: &GitInput) -> Result<Vec<String>, ToolInternalError> {
    let mut args = match input.operation {
        GitOperation::Status => {
            vec!["status".to_owned(), "--short".to_owned(), "--branch".to_owned()]
        }
        GitOperation::Diff => {
            let mut args = vec!["diff".to_owned()];
            if input.staged.unwrap_or(false) {
                args.push("--cached".to_owned());
            }
            args
        }
        GitOperation::Show => vec!["show".to_owned()],
        GitOperation::Log => vec![
            "log".to_owned(),
            "--oneline".to_owned(),
            format!("--max-count={}", input.max_entries.unwrap_or(20).clamp(1, 1000)),
        ],
        GitOperation::Branches => {
            vec!["branch".to_owned(), "--list".to_owned(), "--no-color".to_owned()]
        }
    };
    if let Some(reference) = input.reference.as_deref() {
        validate_git_argument(reference, "reference")?;
        if !matches!(input.operation, GitOperation::Show | GitOperation::Log) {
            return Err(ToolInternalError::Input(
                "reference is only valid for show or log".to_owned(),
            ));
        }
        args.push(reference.to_owned());
    }
    if let Some(path) = input.path.as_deref() {
        let (_, path_buf) = workspace.resolve_for_write(path)?;
        let relative = path_buf
            .strip_prefix(workspace.root())
            .map_err(|_| ToolInternalError::Input("git path escapes workspace".to_owned()))?;
        validate_git_argument(path, "path")?;
        if matches!(input.operation, GitOperation::Diff | GitOperation::Show) {
            args.push("--".to_owned());
            args.push(relative.to_string_lossy().into_owned());
        } else {
            return Err(ToolInternalError::Input("path is only valid for diff or show".to_owned()));
        }
    }
    Ok(args)
}

fn validate_git_argument(value: &str, label: &str) -> Result<(), ToolInternalError> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(|character| character == '\0' || character.is_whitespace())
    {
        return Err(ToolInternalError::Input(format!("git {label} contains an unsafe argument")));
    }
    Ok(())
}
