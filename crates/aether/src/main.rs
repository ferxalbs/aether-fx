use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use aether_agent::{
    Agent, AgentError, AgentRequest, AgentRunResult, CancellationToken, ModelBackend,
    NoPermissionBroker, PermissionBroker, SessionPermissionBroker,
};
use aether_core::{
    AgentEvent, BoundedText, OpaqueContinuation, PermissionRequest, SessionId, ToolExecutor, TurnId,
};
use aether_rainy::RainyBackend;
use aether_terminal::Renderer;
use aether_tools::{ProcessShutdownReport, ToolRegistry};
use thiserror::Error;
use tokio::runtime::Builder;
use tokio::sync::mpsc;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Error)]
enum AppError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Lexopt(#[from] lexopt::Error),
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error("runtime initialization failed: {0}")]
    Runtime(String),
}

enum Command {
    Shell,
    Prompt(String),
    Resume(String),
    Models,
    Doctor,
    Help,
    Version,
}

struct Cli {
    command: Command,
    model: Option<String>,
    root: PathBuf,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("AETHER Fx: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AppError> {
    let cli = parse_cli()?;
    match cli.command {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Version => {
            println!("AETHER Fx {VERSION}");
            Ok(())
        }
        Command::Doctor => doctor(&cli.root),
        Command::Models => with_runtime(|runtime| runtime.block_on(models())),
        Command::Resume(session) => Err(AppError::Message(format!(
            "resume is not implemented in bootstrap (requested session {session})"
        ))),
        Command::Shell => {
            with_runtime(|runtime| runtime.block_on(run_interactive(cli.model, cli.root)))
        }
        Command::Prompt(prompt) => {
            with_runtime(|runtime| runtime.block_on(run_prompt(prompt, cli.model, cli.root)))
        }
    }
}

fn parse_cli() -> Result<Cli, AppError> {
    let mut parser = lexopt::Parser::from_env();
    let mut positionals = Vec::<OsString>::new();
    let mut model = None;
    let mut root = std::env::current_dir()?;
    while let Some(argument) = parser.next()? {
        match argument {
            lexopt::Arg::Short('h') | lexopt::Arg::Long("help") => {
                return Ok(Cli { command: Command::Help, model, root });
            }
            lexopt::Arg::Short('V') | lexopt::Arg::Long("version") => {
                return Ok(Cli { command: Command::Version, model, root });
            }
            lexopt::Arg::Long("model") => {
                model =
                    Some(parser.value()?.into_string().map_err(|_| {
                        AppError::Message("--model must be valid UTF-8".to_owned())
                    })?);
            }
            lexopt::Arg::Long("root") => {
                root = PathBuf::from(parser.value()?);
            }
            lexopt::Arg::Value(value) => positionals.push(value),
            other => {
                return Err(AppError::Message(format!("unsupported argument: {other:?}")));
            }
        }
    }
    if positionals.is_empty() {
        return Ok(Cli { command: Command::Shell, model, root });
    }
    let first = positionals[0]
        .to_str()
        .ok_or_else(|| AppError::Message("command input must be valid UTF-8".to_owned()))?;
    let command = match first {
        "models" if positionals.len() == 1 => Command::Models,
        "doctor" if positionals.len() == 1 => Command::Doctor,
        "resume" if positionals.len() == 2 => Command::Resume(
            positionals[1]
                .to_str()
                .ok_or_else(|| AppError::Message("session id must be valid UTF-8".to_owned()))?
                .to_owned(),
        ),
        "resume" => return Err(AppError::Message("usage: aether resume <session>".to_owned())),
        _ => {
            let mut prompt = String::new();
            for (index, value) in positionals.iter().enumerate() {
                let value = value
                    .to_str()
                    .ok_or_else(|| AppError::Message("prompt must be valid UTF-8".to_owned()))?;
                if index > 0 {
                    prompt.push(' ');
                }
                prompt.push_str(value);
            }
            Command::Prompt(prompt)
        }
    };
    Ok(Cli { command, model, root })
}

fn print_help() {
    println!(
        "AETHER Fx {VERSION}\n\n\
         Usage:\n  aether [OPTIONS] [PROMPT]\n  aether resume <session>\n  aether models\n  aether doctor\n\n\
         Options:\n  --model <id>  Select a Rainy model\n  --root <dir>  Set the workspace root\n  -h, --help    Show help\n  -V, --version Show version"
    );
}

fn doctor(root: &Path) -> Result<(), AppError> {
    println!("AETHER Fx {VERSION}");
    println!("workspace: {}", root.display());
    println!(
        "RAINY_API_KEY: {}",
        if std::env::var_os("RAINY_API_KEY").is_some() { "present" } else { "missing" }
    );
    println!("telemetry: disabled");
    println!("provider integrations: Rainy SDK only");
    Ok(())
}

fn with_runtime(
    operation: impl FnOnce(&tokio::runtime::Runtime) -> Result<(), AppError>,
) -> Result<(), AppError> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    operation(&runtime)
}

async fn models() -> Result<(), AppError> {
    let backend = RainyBackend::from_env().map_err(|error| AppError::Message(error.to_string()))?;
    let models =
        backend.discover_models().await.map_err(|error| AppError::Message(error.to_string()))?;
    for model in models {
        println!("{}", model.id);
    }
    Ok(())
}

async fn run_prompt(prompt: String, model: Option<String>, root: PathBuf) -> Result<(), AppError> {
    let model = configured_model(model)?;
    let backend = RainyBackend::from_env().map_err(|error| AppError::Message(error.to_string()))?;
    let tools = new_tool_registry(root.clone()).await?;
    let workspace = tools.workspace().clone();
    let agent = Agent::new(backend, tools);
    let session_id = SessionId::new(format!("session-{}", std::process::id()))
        .map_err(|error| AppError::Message(error.to_string()))?;
    let turn_id = TurnId::new(format!("turn-{}-1", std::process::id()))
        .map_err(|error| AppError::Message(error.to_string()))?;
    let mut request = AgentRequest::new(session_id, turn_id, prompt);
    request.model = Some(model);
    let mut renderer = Renderer::new();
    let broker = NoPermissionBroker;
    let turn_result = run_agent_turn(&agent, request, &broker, None, &mut renderer).await;
    let cleanup = workspace.shutdown_processes().await;
    if let Err(error) = turn_result {
        if !cleanup.is_clean() {
            report_shutdown_failure(&cleanup);
        }
        return Err(error);
    }
    finish_shutdown(cleanup)
}

/// Keep the prompt local and visible before constructing the network backend.
async fn run_interactive(model: Option<String>, root: PathBuf) -> Result<(), AppError> {
    let Some(mut prompt) = read_prompt().await? else {
        return Ok(());
    };
    let model = configured_model(model)?;
    let backend = RainyBackend::from_env().map_err(|error| AppError::Message(error.to_string()))?;
    let tools = new_tool_registry(root.clone()).await?;
    let workspace = tools.workspace().clone();
    let agent = Agent::new(backend, tools);
    let broker = SessionPermissionBroker::new();
    let session_id = SessionId::new(format!("session-{}", std::process::id()))
        .map_err(|error| AppError::Message(error.to_string()))?;
    let mut continuation: Option<OpaqueContinuation> = None;
    let mut turn_number = 0_u64;
    let mut renderer = Renderer::new();

    let session_result = async {
        loop {
            if prompt.trim().is_empty() {
                let Some(next) = read_prompt().await? else {
                    break;
                };
                prompt = next;
                continue;
            }

            turn_number = turn_number.saturating_add(1);
            let turn_id = TurnId::new(format!("turn-{}-{turn_number}", std::process::id()))
                .map_err(|error| AppError::Message(error.to_string()))?;
            let mut request = AgentRequest::new(session_id.clone(), turn_id, prompt);
            request.model = Some(model.clone());
            request.continuation = continuation.clone();
            if let Some(result) =
                run_agent_turn(&agent, request, &broker, Some(&broker), &mut renderer).await?
            {
                continuation = result.continuation;
            }

            let Some(next) = read_prompt().await? else {
                break;
            };
            prompt = next;
        }
        Ok::<(), AppError>(())
    }
    .await;
    let cleanup = workspace.shutdown_processes().await;
    if !cleanup.is_clean() {
        report_shutdown_failure(&cleanup);
    }
    match session_result {
        Err(error) => Err(error),
        Ok(()) if cleanup.is_clean() => Ok(()),
        Ok(()) => Err(AppError::Message("owned process cleanup failed".to_owned())),
    }
}

fn finish_shutdown(report: ProcessShutdownReport) -> Result<(), AppError> {
    if report.is_clean() {
        Ok(())
    } else {
        report_shutdown_failure(&report);
        Err(AppError::Message("owned process cleanup failed".to_owned()))
    }
}

fn report_shutdown_failure(report: &ProcessShutdownReport) {
    eprintln!(
        "AETHER Fx: HIGH-SEVERITY owned process cleanup failure ({} remaining): {}",
        report.failures.len(),
        report.failure_summary()
    );
}

async fn read_prompt() -> Result<Option<String>, AppError> {
    tokio::task::spawn_blocking(aether_terminal::run_minimal_shell)
        .await
        .map_err(|error| AppError::Message(format!("prompt worker failed: {error}")))?
        .map_err(AppError::Io)
}

async fn new_tool_registry(root: PathBuf) -> Result<ToolRegistry, AppError> {
    tokio::task::spawn_blocking(move || ToolRegistry::new(root))
        .await
        .map_err(|error| AppError::Message(format!("tool registry worker failed: {error}")))?
        .map_err(AppError::Message)
}

fn configured_model(cli_model: Option<String>) -> Result<String, AppError> {
    configured_model_from(cli_model, std::env::var("AETHER_MODEL").ok())
}

fn configured_model_from(
    cli_model: Option<String>,
    env_model: Option<String>,
) -> Result<String, AppError> {
    let model = cli_model.or(env_model);
    let Some(model) = model.filter(|value| !value.trim().is_empty()) else {
        return Err(AppError::Message(
            "no model selected; pass --model <id> or set AETHER_MODEL".to_owned(),
        ));
    };
    Ok(model)
}

async fn run_agent_turn<B, T>(
    agent: &Agent<B, T>,
    request: AgentRequest,
    broker: &dyn PermissionBroker,
    resolver: Option<&SessionPermissionBroker>,
    renderer: &mut Renderer,
) -> Result<Option<AgentRunResult>, AppError>
where
    B: ModelBackend + 'static,
    T: ToolExecutor + 'static,
{
    let (sender, mut receiver) = mpsc::channel(64);
    let cancellation = CancellationToken::new();
    let mut run = Box::pin(agent.run_with_broker(request, sender, cancellation.clone(), broker));
    let mut interrupted = false;
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            event = receiver.recv() => {
                let Some(event) = event else { continue; };
                if let AgentEvent::PermissionRequested { request } = &event {
                    {
                        let mut stdout = io::stdout().lock();
                        renderer.render_if_due(&mut stdout, true)?;
                    }
                    let request_for_prompt = request.clone();
                    let outcome = tokio::task::spawn_blocking(move || {
                        aether_terminal::prompt_permission(&request_for_prompt)
                    })
                    .await
                    .map_err(|error| AppError::Message(format!("permission worker failed: {error}")))??;
                    handle_permission_outcome(
                        outcome,
                        request,
                        broker,
                        resolver,
                        &cancellation,
                    );
                } else {
                    let mut stdout = io::stdout().lock();
                    renderer.handle(&event, &mut stdout)?;
                }
            }
            signal = tokio::signal::ctrl_c(), if !interrupted => {
                if signal.is_ok() {
                    cancellation.cancel();
                    interrupted = true;
                }
            }
        }
    };
    {
        let mut stdout = io::stdout().lock();
        renderer.render_if_due(&mut stdout, true)?;
        stdout.flush()?;
    }
    match result {
        Ok(result) => Ok(Some(result)),
        Err(AgentError::Cancelled) => {
            let mut stdout = io::stdout().lock();
            renderer.handle(
                &AgentEvent::Warning { message: BoundedText::new("turn cancelled", 1024) },
                &mut stdout,
            )?;
            renderer.render_if_due(&mut stdout, true)?;
            stdout.flush()?;
            Ok(None)
        }
        Err(error) => Err(AppError::Agent(error)),
    }
}

fn handle_permission_outcome(
    outcome: aether_terminal::PermissionPromptOutcome,
    request: &PermissionRequest,
    broker: &dyn PermissionBroker,
    resolver: Option<&SessionPermissionBroker>,
    cancellation: &CancellationToken,
) {
    match outcome {
        aether_terminal::PermissionPromptOutcome::Decision(decision) => {
            if let Some(resolver) = resolver {
                let _ = resolver.resolve(&request.call_id, decision);
            }
        }
        aether_terminal::PermissionPromptOutcome::CancelTurn
        | aether_terminal::PermissionPromptOutcome::EndOfInput => {
            cancellation.cancel();
            broker.cancel(&request.call_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_selection_is_explicit_and_precedence_is_stable() {
        assert_eq!(
            configured_model_from(Some("cli-model".to_owned()), Some("env-model".to_owned()))
                .unwrap(),
            "cli-model"
        );
        assert_eq!(configured_model_from(None, Some("env-model".to_owned())).unwrap(), "env-model");
        let error = configured_model_from(None, None).unwrap_err().to_string();
        assert!(error.contains("--model") && error.contains("AETHER_MODEL"));
    }

    #[tokio::test]
    async fn permission_ctrl_c_cancels_turn_and_removes_pending_request() {
        let broker = SessionPermissionBroker::new();
        let request = PermissionRequest {
            call_id: aether_core::ToolCallId::new("permission-cancel").unwrap(),
            tool: "write".to_owned(),
            class: aether_core::PermissionClass::WorkspaceWrite,
            operation: "write".to_owned(),
            target: Some("file.txt".to_owned()),
            details: Default::default(),
        };
        let decision = broker.decide(request.clone());
        let cancellation = CancellationToken::new();
        handle_permission_outcome(
            aether_terminal::PermissionPromptOutcome::CancelTurn,
            &request,
            &broker,
            Some(&broker),
            &cancellation,
        );
        assert!(cancellation.is_cancelled());
        assert!(!broker.resolve(&request.call_id, aether_core::PermissionDecision::AllowOnce));
        assert_eq!(decision.await, aether_core::PermissionDecision::Deny);
    }
}
