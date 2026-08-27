use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use std::sync::{Arc, Mutex};

use aether_agent::{
    Agent, AgentError, AgentRequest, AgentRunResult, AllowAllPermissionBroker, CancellationToken,
    ModelBackend, PermissionBroker, PersistCheckpoint, PersistTurn, SessionPermissionBroker,
    SessionStore, SessionStoreError, persist_checkpoint, persist_turn,
};
use aether_core::{
    AgentEvent, BoundedText, PermissionRequest, SessionId, SessionRecord, ToolExecutor, TurnId,
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
    #[error(transparent)]
    Session(#[from] SessionStoreError),
    #[error("runtime initialization failed: {0}")]
    Runtime(String),
}

enum Command {
    Shell,
    Prompt(String),
    Resume(String),
    ResumeLatest,
    Sessions,
    Models,
    Doctor,
    Help,
    Version,
}

struct Cli {
    command: Command,
    model: Option<String>,
    root: PathBuf,
    json: bool,
    yolo: bool,
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
        Command::Sessions => sessions(&cli.root),
        Command::Resume(session) => with_runtime(|runtime| {
            runtime.block_on(run_resume(session, cli.model, cli.root, cli.json, cli.yolo))
        }),
        Command::ResumeLatest => with_runtime(|runtime| {
            runtime.block_on(run_resume_latest(cli.model, cli.root, cli.json, cli.yolo))
        }),
        Command::Shell => with_runtime(|runtime| {
            runtime.block_on(run_interactive(
                cli.model, cli.root, None, None, false, cli.json, cli.yolo,
            ))
        }),
        Command::Prompt(prompt) => with_runtime(|runtime| {
            runtime.block_on(run_interactive(
                cli.model,
                cli.root,
                None,
                Some(prompt),
                true,
                cli.json,
                cli.yolo,
            ))
        }),
    }
}

fn parse_cli() -> Result<Cli, AppError> {
    let mut parser = lexopt::Parser::from_env();
    let mut positionals = Vec::<OsString>::new();
    let mut model = None;
    let mut latest = false;
    let mut root = std::env::current_dir()?;
    let mut json = false;
    let mut yolo = false;
    while let Some(argument) = parser.next()? {
        match argument {
            lexopt::Arg::Short('h') | lexopt::Arg::Long("help") => {
                return Ok(Cli { command: Command::Help, model, root, json, yolo });
            }
            lexopt::Arg::Short('V') | lexopt::Arg::Long("version") => {
                return Ok(Cli { command: Command::Version, model, root, json, yolo });
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
            lexopt::Arg::Long("latest") => latest = true,
            lexopt::Arg::Long("json") => json = true,
            lexopt::Arg::Long("yolo") => yolo = true,
            lexopt::Arg::Value(value) => positionals.push(value),
            other => {
                return Err(AppError::Message(format!("unsupported argument: {other:?}")));
            }
        }
    }
    if positionals.is_empty() {
        return Ok(Cli { command: Command::Shell, model, root, json, yolo });
    }
    let first = positionals[0]
        .to_str()
        .ok_or_else(|| AppError::Message("command input must be valid UTF-8".to_owned()))?;
    let command = match first {
        "models" if positionals.len() == 1 => Command::Models,
        "doctor" if positionals.len() == 1 => Command::Doctor,
        "sessions" if positionals.len() == 1 => Command::Sessions,
        "resume" if latest && positionals.len() == 1 => Command::ResumeLatest,
        "resume" if !latest && positionals.len() == 2 => Command::Resume(
            positionals[1]
                .to_str()
                .ok_or_else(|| AppError::Message("session id must be valid UTF-8".to_owned()))?
                .to_owned(),
        ),
        "resume" => {
            return Err(AppError::Message(
                "usage: aether resume <session> | aether resume --latest".to_owned(),
            ));
        }
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
    Ok(Cli { command, model, root, json, yolo })
}

fn print_help() {
    println!(
        "AETHER Fx {VERSION}\n\n\
         Usage:\n  aether [OPTIONS] [PROMPT]\n  aether resume <session>|--latest\n  aether sessions\n  aether models\n  aether doctor\n\n\
         Options:\n  --model <id>  Select a Rainy model\n  --root <dir>  Set the workspace root\n  --json        Emit one JSON object per completed turn\n  --yolo        Allow tools without permission prompts\n  --latest      Resume the newest valid local session\n  -h, --help    Show help\n  -V, --version Show version\n\n\
         Sessions are stored as bounded JSONL in private OS application-state storage."
    );
}

fn doctor(root: &Path) -> Result<(), AppError> {
    let workspace = SessionStore::canonical_workspace(root)?;
    let state = SessionStore::state_root()?;
    let sessions = SessionStore::directory_for(&workspace)?;
    println!("AETHER Fx {VERSION}");
    println!("workspace: {}", workspace.display());
    println!("state: {}", state.display());
    println!("sessions: {}", sessions.display());
    println!("telemetry: disabled");
    println!("provider integrations: Rainy SDK only");
    Ok(())
}

fn sessions(root: &Path) -> Result<(), AppError> {
    let (summaries, issues) = SessionStore::summaries(root)?;
    for summary in summaries {
        let modified =
            summary.modified.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        println!(
            "{}\t{}\t{}\t{}\t{}",
            summary.session_id,
            summary.model.as_deref().unwrap_or("-"),
            summary.last_turn.as_ref().map_or("-", |turn| turn.as_str()),
            summary.workspace.as_deref().unwrap_or("-"),
            modified
        );
    }
    for issue in issues {
        eprintln!("AETHER Fx: skipped corrupt session: {issue}");
    }
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

/// Keep the prompt local and visible before constructing the network backend.
async fn run_interactive(
    model: Option<String>,
    root: PathBuf,
    resume: Option<aether_agent::ResumableSession>,
    initial_prompt: Option<String>,
    single_pass: bool,
    json_output: bool,
    yolo: bool,
) -> Result<(), AppError> {
    let first_prompt = if initial_prompt.is_some() {
        initial_prompt
    } else if resume.is_none() {
        let Some(prompt) = read_prompt().await? else {
            return Ok(());
        };
        Some(prompt)
    } else {
        None
    };
    let model = match &resume {
        Some(session) => {
            configured_model(model.or_else(|| session.model.clone())).or_else(|_| {
                session.model.clone().ok_or_else(|| {
                    AppError::Message(
                        "no model selected; pass --model <id> or set AETHER_MODEL".to_owned(),
                    )
                })
            })?
        }
        None => configured_model(model)?,
    };
    let backend = RainyBackend::from_env().map_err(|error| AppError::Message(error.to_string()))?;
    let requested_root =
        resume.as_ref().map(|session| session.workspace_root.clone()).unwrap_or(root);
    let live_root = SessionStore::canonical_workspace(requested_root)?;
    let tools = new_tool_registry(live_root.clone()).await?;
    let workspace = tools.workspace().clone();
    let agent = Agent::new(backend, tools);
    let broker = SessionPermissionBroker::new();
    let allow_all = AllowAllPermissionBroker;
    let session_id = match &resume {
        Some(session) => session.session_id.clone(),
        None => new_session_id()?,
    };
    let store = open_session_store(&live_root, &session_id).await?;
    if resume.is_none() {
        persist_started(&store, &live_root, Some(model.clone())).await?;
    }
    if !json_output {
        announce_session(&session_id, &live_root);
    }
    if let Some(session) = &resume {
        for warning in &session.warnings {
            eprintln!("AETHER Fx: {warning}");
        }
    }
    let mut continuation = resume.as_ref().and_then(|session| session.continuation.clone());
    let mut context_seed = resume.as_ref().map(|session| session.context.clone());
    let mut turn_number = 0_u64;
    let mut renderer = Renderer::new();
    let mut prompt = if let Some(prompt) = first_prompt {
        prompt
    } else {
        match read_prompt().await? {
            Some(value) => value,
            None => {
                let _ = persist_finished(&store).await;
                let cleanup = workspace.shutdown_processes().await;
                return finish_shutdown(cleanup);
            }
        }
    };

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
            let mut request =
                AgentRequest::new(session_id.clone(), turn_id.clone(), prompt.clone());
            request.model = Some(model.clone());
            request.max_steps = configured_max_steps()?;
            request.continuation = continuation.clone();
            request.workspace_root = Some(live_root.display().to_string());
            request.context_seed = context_seed.clone();
            request.metrics = json_output;
            let turn_result = run_agent_turn(
                &agent,
                request,
                if yolo { &allow_all } else { &broker },
                (!yolo).then_some(&broker),
                &mut renderer,
                json_output,
            )
            .await;
            match turn_result {
                Ok(Some(result)) => {
                    if json_output {
                        println!(
                            "{}",
                            serde_json::to_string(&serde_json::json!({
                                "text": result.text.as_str(),
                                "metrics": &result.metrics,
                            }))
                            .map_err(|error| AppError::Message(error.to_string()))?
                        );
                    }
                    persist_completed_turn(&store, &turn_id, &result).await?;
                    continuation = result.continuation;
                    context_seed = Some(result.context);
                }
                Ok(None) => {
                    persist_incomplete_turn(&store, &turn_id, &agent, true).await?;
                    context_seed = Some(agent.context_snapshot());
                    continuation =
                        context_seed.as_ref().and_then(|context| context.continuation.clone());
                }
                Err(error) => {
                    persist_incomplete_turn(&store, &turn_id, &agent, false).await?;
                    return Err(error);
                }
            }

            if single_pass {
                break;
            }

            let Some(next) = read_prompt().await? else {
                break;
            };
            prompt = next;
        }
        Ok::<(), AppError>(())
    }
    .await;
    let _ = persist_finished(&store).await;
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

fn configured_max_steps() -> Result<u16, AppError> {
    match std::env::var("AETHER_MAX_AGENT_STEPS") {
        Ok(value) => value.parse::<u16>().ok().filter(|steps| *steps > 0).ok_or_else(|| {
            AppError::Message(
                "AETHER_MAX_AGENT_STEPS must be an integer from 1 to 65535".to_owned(),
            )
        }),
        Err(std::env::VarError::NotPresent) => Ok(16),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(AppError::Message("AETHER_MAX_AGENT_STEPS must be valid UTF-8".to_owned()))
        }
    }
}

async fn run_resume(
    session: String,
    model: Option<String>,
    root: PathBuf,
    json_output: bool,
    yolo: bool,
) -> Result<(), AppError> {
    let session_id =
        SessionId::new(session).map_err(|error| AppError::Message(error.to_string()))?;
    let restored = tokio::task::spawn_blocking({
        let root = root.clone();
        let session_id = session_id.clone();
        move || SessionStore::restore(root, session_id)
    })
    .await
    .map_err(|error| AppError::Message(format!("session restore worker failed: {error}")))??;
    if !restored.workspace_root.exists() {
        return Err(AppError::Message(format!(
            "workspace {} no longer exists",
            restored.workspace_root.display()
        )));
    }
    run_interactive(model, root, Some(restored), None, false, json_output, yolo).await
}

async fn run_resume_latest(
    model: Option<String>,
    root: PathBuf,
    json_output: bool,
    yolo: bool,
) -> Result<(), AppError> {
    let (restored, issues) = tokio::task::spawn_blocking({
        let root = root.clone();
        move || SessionStore::restore_latest(root)
    })
    .await
    .map_err(|error| AppError::Message(format!("session worker failed: {error}")))??;
    for issue in issues {
        eprintln!("AETHER Fx: skipped corrupt session: {issue}");
    }
    let Some(restored) = restored else {
        return Err(AppError::Message("session not found".to_owned()));
    };
    run_interactive(model, root, Some(restored), None, false, json_output, yolo).await
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

fn new_session_id() -> Result<SessionId, AppError> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    SessionId::new(format!("s-{timestamp}-{}", std::process::id()))
        .map_err(|error| AppError::Message(error.to_string()))
}

fn announce_session(session_id: &SessionId, root: &Path) {
    println!(
        "session {session_id}  (resume: aether resume {session_id} --root {})",
        root.display()
    );
}

async fn open_session_store(
    root: &Path,
    session_id: &SessionId,
) -> Result<Arc<Mutex<SessionStore>>, AppError> {
    let root = root.to_path_buf();
    let session_id = session_id.clone();
    let store = tokio::task::spawn_blocking(move || SessionStore::open(root, session_id))
        .await
        .map_err(|error| AppError::Message(format!("session worker failed: {error}")))??;
    Ok(Arc::new(Mutex::new(store)))
}

async fn persist_started(
    store: &Arc<Mutex<SessionStore>>,
    root: &Path,
    model: Option<String>,
) -> Result<(), AppError> {
    let store = Arc::clone(store);
    let workspace_root = root.display().to_string();
    tokio::task::spawn_blocking(move || {
        store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .append(None, SessionRecord::Started { workspace_root, model })
    })
    .await
    .map_err(|error| AppError::Message(format!("session worker failed: {error}")))??;
    Ok(())
}

async fn persist_completed_turn(
    store: &Arc<Mutex<SessionStore>>,
    turn_id: &TurnId,
    result: &AgentRunResult,
) -> Result<(), AppError> {
    let store = Arc::clone(store);
    let turn_id = turn_id.clone();
    let steps = result.steps;
    let cancelled = result.cancelled;
    let context = result.context.clone();
    let continuation = result.continuation.clone();
    tokio::task::spawn_blocking(move || {
        persist_turn(
            &mut store.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            PersistTurn { turn_id: &turn_id, steps, cancelled, context: &context, continuation },
        )
    })
    .await
    .map_err(|error| AppError::Message(format!("session worker failed: {error}")))??;
    Ok(())
}

async fn persist_incomplete_turn<B, T>(
    store: &Arc<Mutex<SessionStore>>,
    turn_id: &TurnId,
    agent: &Agent<B, T>,
    cancelled: bool,
) -> Result<(), AppError>
where
    B: ModelBackend + 'static,
    T: ToolExecutor + 'static,
{
    let store = Arc::clone(store);
    let turn_id = turn_id.clone();
    let context = agent.context_snapshot();
    let continuation = context.continuation.clone();
    tokio::task::spawn_blocking(move || {
        persist_checkpoint(
            &mut store.lock().unwrap_or_else(|poisoned| poisoned.into_inner()),
            PersistCheckpoint {
                turn_id: &turn_id,
                steps: 0,
                cancelled,
                context: &context,
                continuation,
            },
        )
    })
    .await
    .map_err(|error| AppError::Message(format!("session worker failed: {error}")))??;
    Ok(())
}

async fn persist_finished(store: &Arc<Mutex<SessionStore>>) -> Result<(), AppError> {
    let store = Arc::clone(store);
    tokio::task::spawn_blocking(move || {
        store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .append(None, SessionRecord::Finished)
    })
    .await
    .map_err(|error| AppError::Message(format!("session worker failed: {error}")))??;
    Ok(())
}

async fn run_agent_turn<B, T>(
    agent: &Agent<B, T>,
    request: AgentRequest,
    broker: &dyn PermissionBroker,
    resolver: Option<&SessionPermissionBroker>,
    renderer: &mut Renderer,
    json_output: bool,
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
                } else if !json_output {
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
    if !json_output {
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
