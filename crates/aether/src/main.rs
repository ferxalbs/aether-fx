use std::{
    ffi::OsString,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, ExitCode},
};

use std::sync::{Arc, Mutex};

use aether_agent::{
    Agent, AgentError, AgentRequest, AgentRunResult, AllowAllPermissionBroker, CancellationToken,
    ModelBackend, NoPermissionBroker, PermissionBroker, PersistCheckpoint, PersistTurn,
    ResumableSession, SessionPermissionBroker, SessionStore, SessionStoreError, TurnMode,
    persist_checkpoint, persist_turn,
};
use aether_core::{
    AgentEvent, BoundedText, ContextSnapshot, OpaqueContinuation, PermissionRequest, SessionId,
    SessionRecord, ToolExecutor, TurnId,
};
use aether_obscura::{
    ObscuraError, ObscuraInstallationStatus, ObscuraPaths, ObscuraStatus, ObscuraSupervisor,
    current_artifact, inspect_installation, install_obscura,
};
use aether_rainy::{ModelView, RainyBackend};
use aether_terminal::{ModelSelectionItem, Renderer, SelectorItem, SelectorOutcome};
use aether_tools::{ProcessShutdownReport, ToolRegistry};
use thiserror::Error;
use tokio::runtime::Builder;
use tokio::sync::mpsc;

mod config;
mod updater;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_SHELL_COMMANDS: usize = 32;

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
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Update(#[from] updater::UpdateError),
    #[error(transparent)]
    Obscura(#[from] ObscuraError),
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
    Doctor { network: bool },
    Config(ConfigCommand),
    Auth,
    Update,
    Help(Option<String>),
    Version,
}

enum ConfigCommand {
    Path,
    Show,
    Edit,
}

struct Cli {
    command: Command,
    model: Option<String>,
    root: PathBuf,
    json: bool,
    yolo: bool,
    noninteractive: bool,
    network: bool,
}

#[derive(Clone, Copy)]
struct SessionOptions {
    json_output: bool,
    yolo: bool,
    noninteractive: bool,
}

fn main() -> ExitCode {
    #[cfg(windows)]
    if let Some(exit_code) = updater::maybe_run_windows_helper() {
        return exit_code;
    }
    let json_output = std::env::args_os().any(|argument| argument == "--json");
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(AppError::Update(error)) if json_output => {
            println!("{}", error.json_line());
            ExitCode::FAILURE
        }
        Err(error) => {
            eprintln!("AETHER Fx: {}", safe_message(&error.to_string()));
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), AppError> {
    let cli = parse_cli()?;
    match cli.command {
        Command::Help(topic) => {
            if cli.json {
                print_help_json(topic.as_deref())?;
            } else {
                print_help(topic.as_deref());
            }
            Ok(())
        }
        Command::Version => {
            if cli.json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({ "version": VERSION }))
                        .map_err(|error| AppError::Message(error.to_string()))?
                );
            } else {
                println!("{VERSION}");
            }
            Ok(())
        }
        Command::Doctor { network } => doctor(&cli.root, cli.json, network),
        Command::Update => with_runtime(|runtime| runtime.block_on(update_command(cli.json))),
        Command::Models => {
            with_runtime(|runtime| runtime.block_on(models(&cli.root, cli.json, cli.network)))
        }
        Command::Sessions => sessions(&cli.root, cli.json),
        Command::Config(command) => config_command(&cli.root, command, cli.json),
        Command::Auth => with_runtime(|runtime| {
            runtime.block_on(auth_command(&cli.root, cli.json, cli.noninteractive))
        }),
        Command::Resume(session) => with_runtime(|runtime| {
            runtime.block_on(run_resume(
                session,
                cli.model,
                cli.root,
                cli.json,
                cli.yolo,
                cli.noninteractive,
            ))
        }),
        Command::ResumeLatest => with_runtime(|runtime| {
            runtime.block_on(run_resume_latest(
                cli.model,
                cli.root,
                cli.json,
                cli.yolo,
                cli.noninteractive,
            ))
        }),
        Command::Shell => with_runtime(|runtime| {
            runtime.block_on(run_session(
                cli.model,
                cli.root,
                None,
                None,
                false,
                SessionOptions {
                    json_output: cli.json,
                    yolo: cli.yolo,
                    noninteractive: cli.noninteractive,
                },
            ))
        }),
        Command::Prompt(prompt) => with_runtime(|runtime| {
            runtime.block_on(run_session(
                cli.model,
                cli.root,
                None,
                Some(prompt),
                true,
                SessionOptions {
                    json_output: cli.json,
                    yolo: cli.yolo,
                    noninteractive: cli.noninteractive,
                },
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
    let mut noninteractive = false;
    let mut network = false;
    let mut help_requested = false;
    let mut version_requested = false;
    while let Some(argument) = parser.next()? {
        match argument {
            lexopt::Arg::Short('h') | lexopt::Arg::Long("help") => {
                help_requested = true;
            }
            lexopt::Arg::Short('V') | lexopt::Arg::Long("version") => {
                version_requested = true;
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
            lexopt::Arg::Long("non-interactive") | lexopt::Arg::Long("no-interactive") => {
                noninteractive = true;
            }
            lexopt::Arg::Long("network") => network = true,
            lexopt::Arg::Value(value) => positionals.push(value),
            other => {
                return Err(AppError::Message(format!("unsupported argument: {other:?}")));
            }
        }
    }
    if version_requested {
        return Ok(Cli {
            command: Command::Version,
            model,
            root,
            json,
            yolo,
            noninteractive,
            network,
        });
    }
    if help_requested {
        return Ok(Cli {
            command: Command::Help(None),
            model,
            root,
            json,
            yolo,
            noninteractive,
            network,
        });
    }
    if positionals.is_empty() {
        return Ok(Cli {
            command: Command::Shell,
            model,
            root,
            json,
            yolo,
            noninteractive,
            network,
        });
    }
    let first = positionals[0]
        .to_str()
        .ok_or_else(|| AppError::Message("command input must be valid UTF-8".to_owned()))?;
    let command = match first {
        "models" if positionals.len() == 1 => Command::Models,
        "doctor" if positionals.len() == 1 => Command::Doctor { network },
        "update" if positionals.len() == 1 => Command::Update,
        "sessions" if positionals.len() == 1 => Command::Sessions,
        "config" if positionals.len() == 2 => match positionals[1].to_str() {
            Some("path") => Command::Config(ConfigCommand::Path),
            Some("show") => Command::Config(ConfigCommand::Show),
            Some("edit") => Command::Config(ConfigCommand::Edit),
            _ => {
                return Err(AppError::Message("usage: aether config path|show|edit".to_owned()));
            }
        },
        "config" => {
            return Err(AppError::Message("usage: aether config path|show|edit".to_owned()));
        }
        "auth" if positionals.len() == 1 => Command::Auth,
        "help" if positionals.len() == 1 => Command::Help(None),
        "help" if positionals.len() == 2 => Command::Help(Some(
            positionals[1]
                .to_str()
                .ok_or_else(|| AppError::Message("help topic must be valid UTF-8".to_owned()))?
                .to_owned(),
        )),
        "help" => return Err(AppError::Message("usage: aether help [command]".to_owned())),
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
    Ok(Cli { command, model, root, json, yolo, noninteractive, network })
}

fn print_help(topic: Option<&str>) {
    match topic {
        Some("model") => println!(
            "aether /model\n\nOpen the live Rainy model catalog. The catalog supplies access, privacy,\n\
             capability, context, and reasoning metadata. A selected model applies to future turns."
        ),
        Some("auth") => println!(
            "aether /auth or aether auth\n\nShow credential status or enter a Rainy key interactively.\n\
             Keys are never echoed, persisted, placed in history, or sent to the model."
        ),
        Some("config") => println!(
            "aether config path|show|edit\n\nManage non-secret preferences in private OS application-state storage."
        ),
        Some("sessions") => println!(
            "aether sessions /sessions\n\nList bounded local session summaries; corrupt entries are skipped."
        ),
        Some("resume") => println!(
            "aether resume <session>|--latest /resume\n\nRestore a local session after validating workspace binding and continuation state."
        ),
        Some("doctor") => println!(
            "aether doctor [--network] /doctor\n\nRun local diagnostics. Network diagnostics require explicit --network."
        ),
        Some("update") => println!(
            "aether update [--json] /update\n\nCheck the official AETHER Fx release and install a newer validated binary without elevation."
        ),
        Some("browser") => println!(
            "aether /browser [task|status|stop|update]\n\nInstall or start the pinned Obscura MCP browser after explicit consent. Browser research is read-only; the provider remains active until /browser stop, /clear, /exit, or normal session close."
        ),
        Some(topic) => println!("no help topic named {}; run aether help", safe_message(topic)),
        None => {
            println!(
                "AETHER Fx {VERSION}\n\n\
             Usage:\n  aether [OPTIONS] [PROMPT]\n  aether resume <session>|--latest\n  aether sessions\n  aether models [--json]\n  aether doctor [--network]\n  aether update [--json]\n  aether config path|show|edit\n  aether auth\n\n\
             Interactive commands:\n  /model     Choose a live Rainy model\n  /status    Show session, model, workflow, and permission state\n  /context   Show bounded context and workflow evidence\n  /config    Show or update local preferences\n  /auth      Show credential status or enter a key for this process\n  /sessions  Pick a local session\n  /resume    Restore a local session\n  /doctor    Run offline diagnostics\n  /update    Update AETHER Fx and leave the shell\n  /help      Show this help\n  /clear     Start a fresh local session\n  /exit      Finish and leave the shell\n\n\
             Options:\n  --model <id>       Select a Rainy model\n  --root <dir>       Set the workspace root\n  --json             Emit one JSON object per completed turn\n  --yolo             Allow tools without permission prompts\n  --non-interactive  Disable interactive selection and secret entry\n  --network          Allow doctor/model network discovery where applicable\n  --latest           Resume the newest valid local session\n  -h, --help         Show help\n  -V, --version      Show version\n\n\
             Sessions are stored as bounded JSONL in private OS application-state storage."
            );
            println!("  /browser   Install/start Obscura or run read-only web research");
        }
    }
}

fn print_help_json(topic: Option<&str>) -> Result<(), AppError> {
    let commands = [
        ("/model", "Choose a live Rainy model"),
        ("/status", "Show session and workflow state"),
        ("/context", "Show bounded context and evidence"),
        ("/config", "Show or update local preferences"),
        ("/auth", "Show credential status or enter a key"),
        ("/sessions", "Pick a local session"),
        ("/resume", "Pick and restore a local session"),
        ("/doctor", "Run offline diagnostics"),
        ("/update", "Update AETHER Fx and leave the shell"),
        ("/browser", "Install/start Obscura or run read-only web research"),
        ("/help", "Show local commands"),
        ("/clear", "Start a fresh local session"),
        ("/exit", "Finish and leave the shell"),
    ];
    let object = serde_json::json!({
        "version": VERSION,
        "topic": topic,
        "commands": commands
            .iter()
            .map(|(name, description)| {
                serde_json::json!({"name": name, "description": description})
            })
            .collect::<Vec<_>>(),
    });
    println!(
        "{}",
        serde_json::to_string(&object).map_err(|error| AppError::Message(error.to_string()))?
    );
    Ok(())
}

fn doctor(root: &Path, json_output: bool, network: bool) -> Result<(), AppError> {
    let workspace = SessionStore::canonical_workspace(root)?;
    let state = SessionStore::state_root()?;
    let sessions = SessionStore::directory_for(&workspace)?;
    let config = config::load(&workspace);
    let git = git_branch(&workspace);
    let update_status = updater::local_status_json();
    let update_supported =
        update_status.get("supported").and_then(serde_json::Value::as_bool).unwrap_or(false);
    let update_reason =
        update_status.get("reason").and_then(serde_json::Value::as_str).map(safe_message);
    let session_health = match SessionStore::summaries(&workspace) {
        Ok((summaries, issues)) => serde_json::json!({
            "status": "ok",
            "count": summaries.len(),
            "skipped_corrupt": issues.len(),
        }),
        Err(error) => serde_json::json!({
            "status": "failed",
            "message": safe_message(&error.to_string()),
        }),
    };
    let network_result = if network {
        match credential_for(&workspace) {
            Ok(Some(key)) => match RainyBackend::from_api_key(key) {
                Ok(backend) => match with_async_catalog(&backend) {
                    Ok(count) => serde_json::json!({ "status": "ok", "models": count }),
                    Err(error) => serde_json::json!({
                        "status": "failed",
                        "message": safe_message(&error.to_string()),
                    }),
                },
                Err(error) => serde_json::json!({
                    "status": "failed",
                    "message": safe_message(&error.to_string()),
                }),
            },
            Ok(None) => serde_json::json!({
                "status": "skipped",
                "message": "credentials not configured",
            }),
            Err(error) => serde_json::json!({
                "status": "failed",
                "message": safe_message(&error.to_string()),
            }),
        }
    } else {
        serde_json::json!({ "status": "skipped", "message": "use --network to probe Rainy" })
    };
    if json_output {
        let config_status = match &config {
            Ok(value) => serde_json::json!({
                "path": safe_path(&value.path),
                "default_model": value.config.default_model,
                "credential_helper": value.config.auth.credential_helper.is_some(),
            }),
            Err(error) => serde_json::json!({
                "status": "invalid",
                "message": safe_message(&error.to_string()),
            }),
        };
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "version": VERSION,
                "workspace": safe_path(&workspace),
                "state": safe_path(&state),
                "sessions": safe_path(&sessions),
                "telemetry": "disabled",
                "provider": "Rainy SDK only",
                "rainy_api_key": environment_variable_configured("RAINY_API_KEY"),
                "aether_model": environment_variable_configured("AETHER_MODEL"),
                "default_model_configured": config.as_ref().is_ok_and(|value| value.config.default_model.is_some()),
                "git": {
                    "available": git.is_some(),
                    "branch": git,
                },
                "terminal": {
                    "stdin_tty": io::stdin().is_terminal(),
                    "stdout_tty": io::stdout().is_terminal(),
                },
                "sessions_health": session_health,
                "config": config_status,
                "update": update_status.clone(),
                "network": network_result,
            }))
            .map_err(|error| AppError::Message(error.to_string()))?
        );
        return Ok(());
    }
    println!("AETHER Fx {VERSION}");
    println!("workspace: {}", safe_message(&workspace.display().to_string()));
    println!("state: {}", safe_message(&state.display().to_string()));
    println!("sessions: {}", safe_message(&sessions.display().to_string()));
    println!("telemetry: disabled");
    println!("provider integrations: Rainy SDK only");
    println!(
        "git: {}",
        git.as_deref().map_or("unavailable", |branch| {
            if branch == "HEAD" { "available (detached)" } else { "available" }
        })
    );
    println!(
        "terminal: stdin={} stdout={}",
        if io::stdin().is_terminal() { "tty" } else { "pipe" },
        if io::stdout().is_terminal() { "tty" } else { "pipe" }
    );
    println!(
        "RAINY_API_KEY: {}",
        if environment_variable_configured("RAINY_API_KEY") {
            "configured"
        } else {
            "not configured"
        }
    );
    println!(
        "AETHER_MODEL: {}",
        if environment_variable_configured("AETHER_MODEL") {
            "configured"
        } else {
            "not configured"
        }
    );
    println!("provider endpoint: SDK default HTTPS endpoint");
    println!(
        "config: {}",
        config
            .as_ref()
            .map(|value| safe_message(&value.path.display().to_string()))
            .unwrap_or_else(|error| format!("invalid ({})", safe_message(&error.to_string())))
    );
    println!(
        "credential helper: {}",
        config
            .as_ref()
            .ok()
            .and_then(|value| value.config.auth.credential_helper.as_ref())
            .map_or("not configured", |_| "configured")
    );
    println!(
        "default model: {}",
        if config.as_ref().is_ok_and(|value| value.config.default_model.is_some()) {
            "configured"
        } else {
            "not configured"
        }
    );
    println!(
        "session health: {}",
        session_health.get("status").and_then(serde_json::Value::as_str).unwrap_or("unknown")
    );
    if update_supported {
        println!("update: supported");
    } else if let Some(reason) = update_reason {
        println!("update: unavailable ({reason})");
    } else {
        println!("update: unavailable");
    }
    if network {
        println!(
            "Rainy catalog: {}",
            network_result.get("status").and_then(serde_json::Value::as_str).unwrap_or("unknown")
        );
    } else {
        println!("Rainy catalog: skipped (offline doctor)");
    }
    Ok(())
}

fn environment_variable_configured(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn sessions(root: &Path, json_output: bool) -> Result<(), AppError> {
    let (summaries, issues) = SessionStore::summaries(root)?;
    if json_output {
        let sessions = summaries
            .iter()
            .map(|summary| {
                serde_json::json!({
                    "id": summary.session_id,
                    "model": summary.model.as_deref().map(safe_message),
                    "last_turn": summary.last_turn,
                    "workspace": summary.workspace.as_deref().map(safe_message),
                    "modified_epoch": summary.modified.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs(),
                })
            })
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "sessions": sessions,
                "skipped_corrupt": issues.len(),
            }))
            .map_err(|error| AppError::Message(error.to_string()))?
        );
        for issue in issues {
            eprintln!("AETHER Fx: skipped corrupt session: {}", safe_message(&issue));
        }
        return Ok(());
    }
    for summary in summaries {
        let modified =
            summary.modified.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        println!(
            "{}\t{}\t{}\t{}\t{}",
            summary.session_id,
            summary.model.as_deref().map_or_else(|| "-".to_owned(), safe_message),
            summary.last_turn.as_ref().map_or("-", |turn| turn.as_str()),
            summary.workspace.as_deref().map_or_else(|| "-".to_owned(), safe_message),
            modified
        );
    }
    for issue in issues {
        eprintln!("AETHER Fx: skipped corrupt session: {}", safe_message(&issue));
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

async fn update_command(json_output: bool) -> Result<(), AppError> {
    let result = updater::run(json_output).await?;
    if json_output {
        println!("{}", result.json_line());
    }
    Ok(())
}

async fn models(root: &Path, json_output: bool, _network: bool) -> Result<(), AppError> {
    let workspace = SessionStore::canonical_workspace(root)?;
    let key = credential_for_async(workspace.clone())
        .await?
        .ok_or_else(|| AppError::Message("Rainy credentials are not configured".to_owned()))?;
    let backend =
        RainyBackend::from_api_key(key).map_err(|error| AppError::Message(error.to_string()))?;
    let models = backend
        .discover_model_views()
        .await
        .map_err(|error| AppError::Message(error.to_string()))?;
    if json_output {
        println!(
            "{}",
            serde_json::to_string(&models).map_err(|error| AppError::Message(error.to_string()))?
        );
        return Ok(());
    }
    println!("NAME\tID\tCONTEXT\tTOOLS\tREASONING\tACCESS");
    for model in models {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            model.display_name,
            model.id,
            model
                .effective_context_length
                .or(model.context_length)
                .map(format_model_context)
                .unwrap_or_else(|| "not reported".to_owned()),
            model.tools.as_str(),
            model.reasoning.as_str(),
            model.availability.as_str()
        );
    }
    Ok(())
}

fn config_command(root: &Path, command: ConfigCommand, json_output: bool) -> Result<(), AppError> {
    match command {
        ConfigCommand::Path => {
            let path = SessionStore::config_path(root)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({ "path": safe_path(&path) }))
                        .map_err(|error| AppError::Message(error.to_string()))?
                );
            } else {
                println!("{}", safe_message(&path.display().to_string()));
            }
        }
        ConfigCommand::Show => {
            let loaded = config::load(root)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "path": safe_path(&loaded.path),
                        "config": {
                            "default_model": loaded.config.default_model,
                            "credential_helper_configured":
                                loaded.config.auth.credential_helper.is_some(),
                        },
                    }))
                    .map_err(|error| AppError::Message(error.to_string()))?
                );
            } else {
                println!("path: {}", safe_message(&loaded.path.display().to_string()));
                println!(
                    "default model: {}",
                    loaded
                        .config
                        .default_model
                        .as_deref()
                        .map_or_else(|| "not configured".to_owned(), safe_message)
                );
                println!(
                    "credential helper: {}",
                    loaded
                        .config
                        .auth
                        .credential_helper
                        .as_ref()
                        .map_or("not configured", |_| "configured")
                );
            }
        }
        ConfigCommand::Edit => {
            let path = config::editor_command(root)?;
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string(
                        &serde_json::json!({ "path": safe_path(&path), "edited": true })
                    )
                    .map_err(|error| AppError::Message(error.to_string()))?
                );
            } else {
                println!("updated {}", safe_message(&path.display().to_string()));
            }
        }
    }
    Ok(())
}

async fn auth_command(
    root: &Path,
    json_output: bool,
    noninteractive: bool,
) -> Result<(), AppError> {
    let workspace = SessionStore::canonical_workspace(root)?;
    let loaded = config::load(&workspace)?;
    let env_configured = environment_variable_configured("RAINY_API_KEY");
    let helper_configured = loaded.config.auth.credential_helper.is_some();
    if json_output || noninteractive || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        if json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "environment": env_configured,
                    "credential_helper": helper_configured,
                    "interactive_entry": false,
                }))
                .map_err(|error| AppError::Message(error.to_string()))?
            );
        } else {
            println!(
                "RAINY_API_KEY: {}",
                if env_configured { "configured" } else { "not configured" }
            );
            println!(
                "credential helper: {}",
                if helper_configured { "configured" } else { "not configured" }
            );
            println!("interactive key entry: unavailable (not a TTY)");
        }
        return Ok(());
    }
    if env_configured || helper_configured {
        if !json_output {
            println!(
                "credential source: {}",
                if env_configured { "RAINY_API_KEY" } else { "credential helper" }
            );
        }
        return Ok(());
    }
    print!("Rainy API key (not stored; Enter to cancel): ");
    io::stdout().flush()?;
    let key = tokio::task::spawn_blocking(|| aether_terminal::read_secret(16 * 1024))
        .await
        .map_err(|error| AppError::Message(format!("secret input worker failed: {error}")))??;
    let Some(key) = key.filter(|value| !value.is_empty()) else {
        return Ok(());
    };
    RainyBackend::from_api_key(key)
        .map_err(|error| AppError::Message(format!("credential rejected: {error}")))?;
    println!(
        "credential accepted for this process only; use RAINY_API_KEY or config helper for future runs"
    );
    Ok(())
}

fn credential_for(root: &Path) -> Result<Option<String>, AppError> {
    if let Ok(value) = std::env::var("RAINY_API_KEY")
        && !value.is_empty()
    {
        return Ok(Some(value));
    }
    let loaded = config::load(root)?;
    config::helper_key(&loaded.config).map_err(AppError::from)
}

async fn credential_for_async(root: PathBuf) -> Result<Option<String>, AppError> {
    tokio::task::spawn_blocking(move || credential_for(&root))
        .await
        .map_err(|error| AppError::Message(format!("credential worker failed: {error}")))?
}

fn with_async_catalog(backend: &RainyBackend) -> Result<usize, AppError> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| AppError::Runtime(error.to_string()))?;
    runtime
        .block_on(backend.discover_model_views())
        .map(|models| models.len())
        .map_err(|error| AppError::Message(error.to_string()))
}

fn safe_message(value: &str) -> String {
    value
        .chars()
        .map(|character| if character.is_control() { '�' } else { character })
        .take(512)
        .collect()
}

fn safe_path(path: &Path) -> String {
    safe_message(&path.to_string_lossy())
}

fn format_model_context(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

struct SessionRuntime {
    root: PathBuf,
    config: config::ConfigFile,
    model: Option<String>,
    model_view: Option<ModelView>,
    model_name: Option<String>,
    backend: Option<RainyBackend>,
    agent: Option<Agent<RainyBackend, ToolRegistry>>,
    workspace: Option<Arc<aether_tools::Workspace>>,
    obscura: Option<Arc<ObscuraSupervisor>>,
    store: Option<Arc<Mutex<SessionStore>>>,
    session_id: Option<SessionId>,
    continuation: Option<OpaqueContinuation>,
    context_seed: Option<ContextSnapshot>,
    turn_number: u64,
    session_started: bool,
    broker: SessionPermissionBroker,
    json_output: bool,
    yolo: bool,
    noninteractive: bool,
    interactive_ui: bool,
}

enum ShellControl {
    Continue,
    Exit,
}

impl SessionRuntime {
    fn new(
        cli_model: Option<String>,
        root: PathBuf,
        resume: Option<ResumableSession>,
        json_output: bool,
        yolo: bool,
        noninteractive: bool,
    ) -> Result<Self, AppError> {
        let requested_root =
            resume.as_ref().map(|session| session.workspace_root.clone()).unwrap_or(root);
        let root = SessionStore::canonical_workspace(requested_root)?;
        let loaded = config::load(&root)?;
        let cli_model = cli_model
            .map(|model| {
                clean_model(Some(model)).ok_or_else(|| {
                    AppError::Message(
                        "--model must be non-empty, bounded, and free of whitespace or controls"
                            .to_owned(),
                    )
                })
            })
            .transpose()?;
        let model = cli_model
            .or_else(|| resume.as_ref().and_then(|session| clean_model(session.model.clone())))
            .or_else(|| clean_model(loaded.config.default_model.clone()))
            .or_else(|| configured_model_from(None, std::env::var("AETHER_MODEL").ok()).ok());
        let session_id = resume.as_ref().map(|session| session.session_id.clone());
        let model_changed =
            resume.as_ref().is_some_and(|session| resume_model_changed(session, model.as_deref()));
        let mut context_seed = resume.as_ref().map(|session| session.context.clone());
        let mut continuation = resume.as_ref().and_then(|session| session.continuation.clone());
        if model_changed {
            continuation = None;
            if let Some(context) = context_seed.as_mut() {
                context.continuation = None;
                context.model = model.clone();
            }
        }
        let session_started = resume.is_some();
        let interactive_ui = io::stdin().is_terminal()
            && io::stdout().is_terminal()
            && !json_output
            && !noninteractive;
        Ok(Self {
            root,
            config: loaded.config,
            model,
            model_view: None,
            model_name: None,
            backend: None,
            agent: None,
            workspace: None,
            obscura: None,
            store: None,
            session_id,
            continuation,
            context_seed,
            turn_number: 0,
            session_started,
            broker: SessionPermissionBroker::new(),
            json_output,
            yolo,
            noninteractive,
            interactive_ui,
        })
    }

    async fn run(
        &mut self,
        initial_prompt: Option<String>,
        single_pass: bool,
    ) -> Result<(), AppError> {
        if let Some(prompt) = initial_prompt
            && !prompt.trim().is_empty()
        {
            self.run_turn_or_report(prompt).await?;
            if single_pass {
                return Ok(());
            }
        } else if single_pass {
            return Ok(());
        }

        loop {
            match self.read_input().await? {
                aether_terminal::ShellInput::Line(prompt) => {
                    if !prompt.trim().is_empty() {
                        self.run_turn_or_report(prompt).await?;
                    }
                }
                aether_terminal::ShellInput::Command(command) => {
                    match self.run_command(&command).await {
                        Ok(ShellControl::Exit) => break,
                        Ok(ShellControl::Continue) => {}
                        Err(error)
                            if self.interactive_ui && !matches!(&error, AppError::Update(_)) =>
                        {
                            eprintln!("AETHER Fx: {}", safe_message(&error.to_string()));
                        }
                        Err(error) => return Err(error),
                    }
                }
                aether_terminal::ShellInput::Cancelled => {}
                aether_terminal::ShellInput::EndOfInput => break,
            }
        }
        Ok(())
    }

    async fn run_turn_or_report(&mut self, prompt: String) -> Result<(), AppError> {
        self.run_turn_or_report_mode(prompt, TurnMode::Repository).await
    }

    async fn run_turn_or_report_mode(
        &mut self,
        prompt: String,
        mode: TurnMode,
    ) -> Result<(), AppError> {
        match self.run_turn_mode(prompt, mode).await {
            Err(error) if self.interactive_ui => {
                eprintln!("AETHER Fx: {}", safe_message(&error.to_string()));
                Ok(())
            }
            result => result,
        }
    }

    async fn read_input(&self) -> Result<aether_terminal::ShellInput, AppError> {
        if self.interactive_ui {
            let commands = command_items();
            tokio::task::spawn_blocking(move || aether_terminal::run_shell_input(&commands))
                .await
                .map_err(|error| AppError::Message(format!("shell input worker failed: {error}")))?
                .map_err(AppError::Io)
        } else {
            let line = tokio::task::spawn_blocking(aether_terminal::read_plain_line)
                .await
                .map_err(|error| AppError::Message(format!("shell input worker failed: {error}")))?
                .map_err(AppError::Io)?;
            Ok(match line {
                None => aether_terminal::ShellInput::EndOfInput,
                Some(line) if line.starts_with('/') => aether_terminal::ShellInput::Command(line),
                Some(line) => aether_terminal::ShellInput::Line(line),
            })
        }
    }

    async fn run_command(&mut self, command: &str) -> Result<ShellControl, AppError> {
        if is_browser_command(command) {
            self.run_browser_command(command).await?;
            return Ok(ShellControl::Continue);
        }
        match command {
            "/help" => {
                if self.json_output {
                    print_help_json(None)?;
                } else {
                    print_help(None);
                }
            }
            "/status" => self.print_status()?,
            "/context" => self.print_context()?,
            "/config" => self.run_config_command()?,
            "/auth" => self.run_auth_command().await?,
            "/model" => self.choose_model(false).await?,
            "/sessions" | "/resume" => self.choose_session().await?,
            "/doctor" => doctor(&self.root, self.json_output, false)?,
            "/update" => {
                let result = updater::run(self.json_output).await?;
                if self.json_output {
                    println!("{}", result.json_line());
                }
                return Ok(ShellControl::Exit);
            }
            "/clear" => self.clear_session().await?,
            "/exit" | "/quit" => return Ok(ShellControl::Exit),
            value => {
                if self.json_output {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "error": "unknown_local_command",
                            "command": value,
                        }))
                        .map_err(|error| AppError::Message(error.to_string()))?
                    );
                } else {
                    println!("unknown local command {}; use /help", safe_message(value));
                }
            }
        }
        Ok(ShellControl::Continue)
    }

    async fn run_browser_command(&mut self, command: &str) -> Result<(), AppError> {
        let argument = command.strip_prefix("/browser").unwrap_or_default().trim();
        match argument {
            "" => {
                self.ensure_obscura().await?;
                self.print_browser_status().await
            }
            "status" => self.print_browser_status().await,
            "stop" => self.stop_obscura().await,
            "update" => self.update_obscura().await,
            task => {
                self.ensure_obscura().await?;
                self.run_turn_or_report_mode(task.to_owned(), TurnMode::ExternalResearch).await
            }
        }
    }

    fn obscura_paths(&self) -> Result<ObscuraPaths, AppError> {
        let artifact = current_artifact()?;
        let config_path = SessionStore::config_path(&self.root)?;
        let state_root = config_path
            .parent()
            .ok_or_else(|| AppError::Message("AETHER state root has no parent".to_owned()))?;
        let workspace_id = SessionStore::workspace_id(&self.root)?;
        Ok(ObscuraPaths::for_workspace(state_root, workspace_id, artifact))
    }

    async fn ensure_obscura(&mut self) -> Result<(), AppError> {
        if let Some(provider) = self.obscura.as_ref()
            && provider.status().await?.active
            && provider.is_healthy()
        {
            return Ok(());
        }
        if let Some(provider) = self.obscura.take() {
            let _ = provider.shutdown().await;
        }

        let paths = self.obscura_paths()?;
        let status = inspect_installation(&paths).await?;
        if status.installed_version.is_some() && !status.version_matches {
            return Err(AppError::Message(format!(
                "the installed Obscura pin does not match AETHER's fixed {} pin; run /browser update before using it",
                paths.artifact().version
            )));
        }
        if !status.installed {
            self.confirm_obscura_install(paths.artifact()).await?;
            let report = install_obscura(&paths).await?;
            if !self.json_output {
                println!(
                    "installed Obscura {} for {}",
                    safe_message(&report.version),
                    safe_message(&report.target)
                );
            }
        }

        let had_agent = self.agent.is_some();
        let provider = ObscuraSupervisor::launch(paths).await?;
        if had_agent && let Err(error) = self.reset_agent_for_model_switch().await {
            let _ = provider.shutdown().await;
            return Err(error);
        }
        self.obscura = Some(provider);
        if had_agent {
            self.ensure_agent().await?;
        }
        if !self.json_output {
            println!("Obscura {} is active", safe_message(self.obscura_version()));
        }
        Ok(())
    }

    async fn confirm_obscura_install(
        &self,
        artifact: &'static aether_obscura::ObscuraArtifact,
    ) -> Result<(), AppError> {
        if !self.json_output {
            println!("Obscura is not installed for this AETHER build.");
            println!("  version: {}", safe_message(artifact.version));
            println!("  target: {}", safe_message(artifact.target));
            println!("  asset: {}", safe_message(artifact.asset_name));
            println!("  size: {} bytes", artifact.archive_size);
            println!("  source: {}", safe_message(artifact.url));
            println!("  SHA-256: {}", safe_message(artifact.sha256));
            println!("  MCP: {}", safe_message(artifact.mcp_protocol_version));
        }
        if !self.interactive_ui {
            return Err(AppError::Message(
                "Obscura installation requires explicit interactive consent; no files or process were created"
                    .to_owned(),
            ));
        }
        let prompt =
            format!("Install verified Obscura {} for {}?", artifact.version, artifact.target);
        let accepted =
            tokio::task::spawn_blocking(move || aether_terminal::confirm(&prompt, false))
                .await
                .map_err(|error| {
                    AppError::Message(format!("installation confirmation worker failed: {error}"))
                })?
                .map_err(AppError::Io)?
                .unwrap_or(false);
        if !accepted {
            return Err(AppError::Message(
                "Obscura installation was not approved; no files or process were created"
                    .to_owned(),
            ));
        }
        Ok(())
    }

    async fn print_browser_status(&mut self) -> Result<(), AppError> {
        let status = if let Some(provider) = self.obscura.as_ref() {
            provider.status().await?
        } else {
            let paths = self.obscura_paths()?;
            let installation = inspect_installation(&paths).await?;
            obscura_status_from_installation(&paths, installation)
        };
        let object = serde_json::json!({
            "provider": "obscura",
            "target": current_artifact()?.target,
            "fixed_version": status.expected_version,
            "installed": status.installed,
            "installed_version": status.installed_version,
            "active": status.active,
            "healthy": status.healthy,
            "mcp_protocol": status.mcp_protocol_version,
            "profile": status.profile_id,
        });
        if self.json_output {
            println!(
                "{}",
                serde_json::to_string(&object)
                    .map_err(|error| AppError::Message(error.to_string()))?
            );
        } else {
            println!(
                "Obscura: installed={} active={} healthy={} version={} MCP={} profile={}",
                if status.installed { "yes" } else { "no" },
                if status.active { "yes" } else { "no" },
                if status.healthy { "yes" } else { "no" },
                safe_message(&status.expected_version),
                status
                    .mcp_protocol_version
                    .as_deref()
                    .map_or_else(|| "inactive".to_owned(), safe_message),
                safe_message(&status.profile_id)
            );
            if let Some(installed) = status.installed_version.as_deref()
                && installed != status.expected_version
            {
                println!("installed pin: {installed} (run /browser update)");
            }
        }
        Ok(())
    }

    async fn stop_obscura(&mut self) -> Result<(), AppError> {
        let provider = self.obscura.take();
        let had_agent = self.agent.is_some();
        let shutdown_error =
            if let Some(provider) = provider { provider.shutdown().await.err() } else { None };
        if had_agent {
            self.reset_agent_for_model_switch().await?;
            self.ensure_agent().await?;
        }
        if let Some(error) = shutdown_error {
            return Err(AppError::Obscura(error));
        }
        if !self.json_output {
            println!("Obscura stopped; the private browser profile was preserved");
        }
        Ok(())
    }

    async fn update_obscura(&mut self) -> Result<(), AppError> {
        if let Some(provider) = self.obscura.as_ref()
            && provider.status().await?.active
        {
            return Err(AppError::Message(
                "Obscura is active; run /browser stop before /browser update".to_owned(),
            ));
        }
        if let Some(provider) = self.obscura.take() {
            let _ = provider.shutdown().await;
        }
        let paths = self.obscura_paths()?;
        let status = inspect_installation(&paths).await?;
        if status.installed && status.version_matches {
            if !self.json_output {
                println!("Obscura {} is already installed", paths.artifact().version);
            }
            return Ok(());
        }
        self.confirm_obscura_install(paths.artifact()).await?;
        let report = install_obscura(&paths).await?;
        if self.json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "provider": "obscura",
                    "updated": report.updated,
                    "version": report.version,
                    "target": report.target,
                }))
                .map_err(|error| AppError::Message(error.to_string()))?
            );
        } else {
            println!("Obscura {} is ready; run /browser to start it", report.version);
        }
        Ok(())
    }

    fn obscura_version(&self) -> &str {
        self.obscura.as_ref().map_or("unknown", |provider| provider.artifact().version)
    }

    async fn ensure_backend(&mut self) -> Result<&RainyBackend, AppError> {
        if self.backend.is_none() {
            let mut key = credential_for_async(self.root.clone()).await?;
            if key.is_none() && self.interactive_ui {
                print!("Rainy API key (not stored; Enter to cancel): ");
                io::stdout().flush()?;
                key = tokio::task::spawn_blocking(|| aether_terminal::read_secret(16 * 1024))
                    .await
                    .map_err(|error| {
                        AppError::Message(format!("secret input worker failed: {error}"))
                    })??;
                if key.as_deref().is_none_or(str::is_empty) {
                    return Err(AppError::Message(
                        "Rainy credentials are required for this operation".to_owned(),
                    ));
                }
            }
            let key = key.ok_or_else(|| {
                AppError::Message(
                    "RAINY_API_KEY is not configured; set RAINY_API_KEY or configure a credential helper"
                        .to_owned(),
                )
            })?;
            self.backend = Some(
                RainyBackend::from_api_key(key)
                    .map_err(|error| AppError::Message(error.to_string()))?,
            );
        }
        Ok(self.backend.as_ref().expect("backend initialized"))
    }

    async fn ensure_model(&mut self) -> Result<String, AppError> {
        if let Some(model) = self.model.clone() {
            return Ok(model);
        }
        self.choose_model(true).await?;
        self.model.clone().ok_or_else(|| {
            AppError::Message(
                "no model selected; pass --model, set AETHER_MODEL, or choose one from /model"
                    .to_owned(),
            )
        })
    }

    async fn choose_model(&mut self, initial: bool) -> Result<(), AppError> {
        if self.noninteractive || !self.interactive_ui {
            return Err(AppError::Message(
                "no model selected; pass --model <id>, set AETHER_MODEL, or set default_model in config"
                    .to_owned(),
            ));
        }
        let backend = match self.ensure_backend().await {
            Ok(backend) => backend.clone(),
            Err(error) if !initial => {
                eprintln!("AETHER Fx: {}", safe_message(&error.to_string()));
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if self.interactive_ui {
            println!("Fetching available models…");
        }
        let views = match backend.discover_model_views().await {
            Ok(views) => views,
            Err(error) if !initial => {
                eprintln!("AETHER Fx: {}", safe_message(&error.to_string()));
                return Ok(());
            }
            Err(error) => return Err(AppError::Message(error.to_string())),
        };
        if views.is_empty() {
            if initial {
                return Err(AppError::Message("Rainy returned no selectable models".to_owned()));
            }
            eprintln!("AETHER Fx: Rainy returned no selectable models");
            return Ok(());
        }
        let current = self.model.clone();
        let items = views
            .iter()
            .map(|view| {
                ModelSelectionItem::new(
                    view.id.clone(),
                    view.display_name.clone(),
                    Some(view.detail()),
                )
                .unavailable(!view.selectable())
            })
            .collect::<Vec<_>>();
        let outcome = tokio::task::spawn_blocking(move || {
            let default = current.as_deref();
            aether_terminal::select_model_interactively(&items, default)
        })
        .await
        .map_err(|error| AppError::Message(format!("model selector worker failed: {error}")))?
        .map_err(AppError::Io)?;
        let selected = match outcome {
            Some(value) => value,
            None => {
                if initial {
                    return Err(AppError::Message("model selection cancelled".to_owned()));
                }
                return Ok(());
            }
        };
        let view = views.iter().find(|view| view.id == selected).ok_or_else(|| {
            AppError::Message("selected model disappeared from catalog".to_owned())
        })?;
        if !view.selectable() {
            return Err(AppError::Message(
                "selected model is not available for tools or the current access policy".to_owned(),
            ));
        }
        if self.model.as_deref() == Some(view.id.as_str()) {
            self.model_view = Some(view.clone());
            self.model_name = Some(view.display_name.clone());
            println!(
                "model: {} ({}) · transport {} · already active",
                safe_message(&view.display_name),
                safe_message(&view.id),
                view.protocol.as_str()
            );
            return Ok(());
        }
        if view.has_unknown_metadata() || view.disclosure_required {
            let reason = if view.disclosure_required {
                "Rainy requires a data-disclosure acknowledgement for this model"
            } else {
                "Rainy did not report complete capability or access metadata; verify before using this model"
            };
            let policy_detail = if view.disclosure_required {
                format!(
                    " policy={} training_with_inputs={} zdr={} retention_days={}{}",
                    view.policy_status.as_deref().unwrap_or("unknown"),
                    view.training_with_inputs
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                    view.zdr_available
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                    view.retention_days
                        .map_or_else(|| "unknown".to_owned(), |value| value.to_string()),
                    view.policy_notice
                        .as_deref()
                        .map_or_else(String::new, |notice| format!(" notice={notice}")),
                )
            } else {
                String::new()
            };
            let identity = format_model_identity(Some(view.id.as_str()), Some(view), None);
            let prompt = format!("{reason}: {identity}.{} Continue?", policy_detail);
            let confirmed =
                tokio::task::spawn_blocking(move || aether_terminal::confirm(&prompt, false))
                    .await
                    .map_err(|error| {
                        AppError::Message(format!("confirmation worker failed: {error}"))
                    })?
                    .map_err(AppError::Io)?
                    .unwrap_or(false);
            if !confirmed {
                if initial {
                    return Err(AppError::Message("model selection not acknowledged".to_owned()));
                }
                return Ok(());
            }
        }
        self.reset_agent_for_model_switch().await?;
        self.model = Some(view.id.clone());
        self.model_view = Some(view.clone());
        self.model_name = Some(view.display_name.clone());
        self.continuation = None;
        if let Some(context) = self.context_seed.as_mut() {
            context.continuation = None;
            context.model = self.model.clone();
        }
        println!(
            "model: {} ({}) · transport {} · ready for the next turn",
            safe_message(&view.display_name),
            safe_message(&view.id),
            view.protocol.as_str()
        );
        Ok(())
    }

    async fn ensure_agent(&mut self) -> Result<(), AppError> {
        let model = self.ensure_model().await?;
        if self.agent.is_some() {
            return Ok(());
        }
        let backend = self.ensure_backend().await?.clone();
        let tools = new_tool_registry(self.root.clone(), self.obscura.clone()).await?;
        self.workspace = Some(Arc::new(tools.workspace().clone()));
        self.agent = Some(Agent::new(backend, tools));
        let session_id = self.session_id.clone().unwrap_or(new_session_id()?);
        let store = open_session_store(&self.root, &session_id).await?;
        if !self.session_started {
            persist_started(&store, &self.root, Some(model)).await?;
            self.session_started = true;
        }
        self.session_id = Some(session_id);
        self.store = Some(store);
        Ok(())
    }

    async fn run_turn_mode(&mut self, prompt: String, mode: TurnMode) -> Result<(), AppError> {
        self.ensure_agent().await?;
        self.turn_number = self.turn_number.saturating_add(1);
        let session_id = self.session_id.clone().expect("session initialized");
        let turn_id = TurnId::new(format!("turn-{}-{}", std::process::id(), self.turn_number))
            .map_err(|error| AppError::Message(error.to_string()))?;
        let model = self.model.clone().expect("model initialized");
        let mut request = AgentRequest::new(session_id, turn_id.clone(), prompt);
        request.model = Some(model);
        request.max_steps = configured_max_steps()?;
        request.continuation = self.continuation.clone();
        request.workspace_root = Some(self.root.display().to_string());
        request.context_seed = self.context_seed.clone();
        request.metrics = self.json_output;
        request.mode = mode;
        let no_permissions = NoPermissionBroker;
        let allow_all = AllowAllPermissionBroker;
        let broker: &dyn PermissionBroker = if self.yolo {
            &allow_all
        } else if self.interactive_ui {
            &self.broker
        } else {
            &no_permissions
        };
        let resolver = (!self.yolo && self.interactive_ui).then_some(&self.broker);
        let interactive_ui = self.interactive_ui;
        let turn_result = {
            let agent = self.agent.as_ref().expect("agent initialized");
            let mut renderer = Renderer::new_with_mode(interactive_ui);
            run_agent_turn(
                agent,
                request,
                broker,
                resolver,
                &mut renderer,
                self.json_output,
                interactive_ui,
            )
            .await
        };
        let store = self.store.as_ref().expect("session store initialized");
        match turn_result {
            Ok(Some(result)) => {
                if self.json_output {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "text": result.text.as_str(),
                            "metrics": result.metrics,
                            "model": result.context.model,
                            "session": self.session_id,
                        }))
                        .map_err(|error| AppError::Message(error.to_string()))?
                    );
                } else if self.interactive_ui {
                    let model_identity = self.model_identity();
                    print_turn_summary(&turn_id, &result.context, &model_identity);
                }
                persist_completed_turn(store, &turn_id, &result).await?;
                self.continuation = result.continuation.clone();
                self.context_seed = Some(result.context);
            }
            Ok(None) => {
                let agent = self.agent.as_ref().expect("agent initialized");
                persist_incomplete_turn(store, &turn_id, agent, true).await?;
                self.context_seed = Some(agent.context_snapshot());
                self.continuation =
                    self.context_seed.as_ref().and_then(|context| context.continuation.clone());
            }
            Err(error) => {
                let agent = self.agent.as_ref().expect("agent initialized");
                persist_incomplete_turn(store, &turn_id, agent, false).await?;
                self.detach_unhealthy_obscura().await?;
                return Err(error);
            }
        }
        self.detach_unhealthy_obscura().await?;
        Ok(())
    }

    async fn detach_unhealthy_obscura(&mut self) -> Result<(), AppError> {
        let unhealthy = self.obscura.as_ref().is_some_and(|provider| !provider.is_healthy());
        if !unhealthy {
            return Ok(());
        }
        if let Some(provider) = self.obscura.take() {
            let _ = provider.shutdown().await;
        }
        if self.agent.is_some() {
            self.reset_agent_for_model_switch().await?;
            self.ensure_agent().await?;
        }
        Ok(())
    }

    fn print_status(&self) -> Result<(), AppError> {
        let context = self.current_context();
        let object = serde_json::json!({
            "workspace": safe_path(&self.root),
            "session": self.session_id,
            "model": self.model,
            "model_protocol": self.model_view.as_ref().map(|view| view.protocol.as_str()),
            "model_name": self
                .model_view
                .as_ref()
                .map(|view| view.display_name.clone())
                .or_else(|| self.model_name.clone()),
            "model_context": self.model_view.as_ref().and_then(|view| {
                view.effective_context_length.or(view.context_length)
            }),
            "model_context_model": self
                .model_view
                .as_ref()
                .and_then(|view| view.model_context_length),
            "model_context_plan": self
                .model_view
                .as_ref()
                .and_then(|view| view.plan_context_length),
            "model_tier": self.model_view.as_ref().and_then(|view| view.tier.clone()),
            "model_tools": self.model_view.as_ref().map(|view| view.tools.as_str()),
            "model_reasoning": self.model_view.as_ref().map(|view| view.reasoning.as_str()),
            "model_reasoning_modes": self
                .model_view
                .as_ref()
                .map(|view| view.reasoning_modes.clone()),
            "model_reasoning_values": self
                .model_view
                .as_ref()
                .map(|view| view.reasoning_values.clone()),
            "model_reasoning_parameters": self
                .model_view
                .as_ref()
                .map(|view| view.reasoning_parameters.clone()),
            "model_thinking_budget_min": self
                .model_view
                .as_ref()
                .and_then(|view| view.thinking_budget_min),
            "model_thinking_budget_max": self
                .model_view
                .as_ref()
                .and_then(|view| view.thinking_budget_max),
            "model_thinking_budget_dynamic_value": self
                .model_view
                .as_ref()
                .and_then(|view| view.thinking_budget_dynamic_value),
            "model_thinking_budget_disable_value": self
                .model_view
                .as_ref()
                .and_then(|view| view.thinking_budget_disable_value),
            "model_input_modalities": self
                .model_view
                .as_ref()
                .map(|view| view.input_modalities.clone()),
            "model_output_modalities": self
                .model_view
                .as_ref()
                .map(|view| view.output_modalities.clone()),
            "model_modalities_reported": self
                .model_view
                .as_ref()
                .map(|view| view.modalities_reported),
            "model_access": self.model_view.as_ref().map(|view| view.availability.as_str()),
            "permission_mode": if self.yolo { "yolo" } else if self.interactive_ui { "prompt" } else { "deny" },
            "backend_initialized": self.backend.is_some(),
            "session_initialized": self.agent.is_some(),
            "continuation": self.continuation.is_some(),
            "workflow_phase": context.workflow.phase.as_str(),
            "director_phase": context.workflow.director.phase.as_str(),
            "verification": context.workflow.verification.as_str(),
            "inspected": context.inspected.len(),
            "modified": context.modified.len(),
            "unresolved_failures": context.workflow.unresolved_failures.len(),
        });
        if self.json_output {
            println!(
                "{}",
                serde_json::to_string(&object)
                    .map_err(|error| AppError::Message(error.to_string()))?
            );
        } else {
            println!("workspace: {}", safe_message(&self.root.display().to_string()));
            println!("session: {}", self.session_id.as_ref().map_or("pending", SessionId::as_str));
            if let Some(view) = self.model_view.as_ref() {
                println!("model: {}", safe_message(&view.display_name));
                if let Some(model) = self.model.as_deref()
                    && view.display_name != model
                {
                    println!("model id: {}", safe_message(model));
                }
                println!("model transport: {}", view.protocol.as_str());
                println!(
                    "model capabilities: {} · tools {} · reasoning {} · access {}",
                    view.effective_context_length
                        .or(view.context_length)
                        .map_or_else(|| "context not reported".to_owned(), format_model_context),
                    view.tools.as_str(),
                    view.reasoning.as_str(),
                    view.availability.as_str()
                );
            } else {
                println!("model: {}", self.model_identity());
            }
            println!(
                "permission mode: {}",
                if self.yolo {
                    "yolo"
                } else if self.interactive_ui {
                    "prompt"
                } else {
                    "deny"
                }
            );
            println!("backend: {}", if self.backend.is_some() { "ready" } else { "deferred" });
            println!(
                "workflow: {} / {}",
                context.workflow.phase.as_str(),
                context.workflow.director.phase.as_str()
            );
            println!("verification: {}", context.workflow.verification.as_str());
            println!(
                "context: inspected={} modified={} failures={} continuation={}",
                context.inspected.len(),
                context.modified.len(),
                context.workflow.unresolved_failures.len(),
                if self.continuation.is_some() { "attached" } else { "none" }
            );
        }
        Ok(())
    }

    fn current_context(&self) -> ContextSnapshot {
        self.agent
            .as_ref()
            .map(Agent::context_snapshot)
            .or_else(|| self.context_seed.clone())
            .unwrap_or_else(|| {
                ContextSnapshot::new(self.root.display().to_string(), self.model.clone())
            })
    }

    fn model_identity(&self) -> String {
        format_model_identity(
            self.model.as_deref(),
            self.model_view.as_ref(),
            self.model_name.as_deref(),
        )
    }

    fn print_context(&self) -> Result<(), AppError> {
        let context = self.current_context();
        if self.json_output {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "workspace": safe_message(context.workspace_root.as_str()),
                    "task": safe_message(context.current_task.as_str()),
                    "inspected": context.inspected.iter().map(|file| safe_message(&file.path)).collect::<Vec<_>>(),
                    "modified": context.modified.iter().map(|path| safe_message(path)).collect::<Vec<_>>(),
                    "user_modified": context.user_modified.iter().map(|path| safe_message(path)).collect::<Vec<_>>(),
                    "workflow": context.workflow,
                    "continuation": context.continuation.is_some(),
                }))
                .map_err(|error| AppError::Message(error.to_string()))?
            );
            return Ok(());
        }
        println!("task: {}", safe_message(context.current_task.as_str()));
        println!(
            "evidence: inspected={} excerpts={} tool summaries={}",
            context.inspected.len(),
            context.excerpts.len(),
            context.tool_summaries.len()
        );
        println!(
            "files: modified={} user-modified={} workspace-changed={}",
            context.modified.len(),
            context.user_modified.len(),
            context.workspace_changed
        );
        println!(
            "workflow: phase={} director={} verification={} revision={}",
            context.workflow.phase.as_str(),
            context.workflow.director.phase.as_str(),
            context.workflow.verification.as_str(),
            context.workflow.workspace_revision
        );
        for path in context.modified.iter().take(16) {
            println!("  modified: {}", safe_message(path));
        }
        Ok(())
    }

    fn run_config_command(&mut self) -> Result<(), AppError> {
        config_command(&self.root, ConfigCommand::Show, self.json_output)?;
        if let Some(model) = self.model.clone()
            && self.interactive_ui
        {
            print!("save {model} as the default model? ");
            io::stdout().flush()?;
            let accepted = aether_terminal::confirm("Save default model", false)
                .map_err(AppError::Io)?
                .unwrap_or(false);
            if accepted {
                self.config.default_model = Some(model);
                let _ = config::save(&self.root, self.config.clone())?;
                println!("default model updated");
            }
        }
        Ok(())
    }

    async fn run_auth_command(&mut self) -> Result<(), AppError> {
        if self.backend.is_none()
            && !environment_variable_configured("RAINY_API_KEY")
            && self.config.auth.credential_helper.is_none()
            && !self.interactive_ui
        {
            if self.json_output {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "environment": false,
                        "credential_helper": false,
                        "active": false,
                    }))
                    .map_err(|error| AppError::Message(error.to_string()))?
                );
            } else {
                println!("RAINY_API_KEY: not configured");
                println!("credential helper: not configured");
                println!("interactive key entry: unavailable");
            }
            return Ok(());
        }
        let _ = self.ensure_backend().await?;
        if !self.json_output {
            println!("credential active for this process only; no key was persisted");
        } else {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "active": true,
                    "persisted": false,
                }))
                .map_err(|error| AppError::Message(error.to_string()))?
            );
        }
        Ok(())
    }

    async fn choose_session(&mut self) -> Result<(), AppError> {
        if !self.interactive_ui {
            return Err(AppError::Message(
                "session selection requires a TTY; use aether sessions or aether resume <id>"
                    .to_owned(),
            ));
        }
        let (summaries, issues) = SessionStore::summaries(&self.root)?;
        for issue in issues {
            eprintln!("AETHER Fx: skipped corrupt session: {}", safe_message(&issue));
        }
        if summaries.is_empty() {
            println!("no resumable sessions");
            return Ok(());
        }
        let items = summaries
            .iter()
            .map(|summary| {
                let label = summary.session_id.to_string();
                let detail = format!(
                    "{} · {}",
                    summary.model.as_deref().map_or_else(|| "model ?".to_owned(), safe_message),
                    summary.last_turn.as_ref().map_or("no turns", TurnId::as_str)
                );
                SelectorItem::new(summary.session_id.to_string(), label, Some(detail))
            })
            .collect::<Vec<_>>();
        let default = self.session_id.as_ref().map(ToString::to_string);
        let outcome = tokio::task::spawn_blocking(move || {
            aether_terminal::select_interactively("Sessions", &items, default.as_ref())
        })
        .await
        .map_err(|error| AppError::Message(format!("session selector worker failed: {error}")))?
        .map_err(AppError::Io)?;
        let selected = match outcome {
            SelectorOutcome::Selected(value) => value,
            SelectorOutcome::Cancelled
            | SelectorOutcome::EndOfInput
            | SelectorOutcome::NoSelection => return Ok(()),
        };
        let session_id =
            SessionId::new(selected).map_err(|error| AppError::Message(error.to_string()))?;
        let restored = tokio::task::spawn_blocking({
            let root = self.root.clone();
            let session_id = session_id.clone();
            move || SessionStore::restore(root, session_id)
        })
        .await
        .map_err(|error| AppError::Message(format!("session restore worker failed: {error}")))??;
        self.close_current().await?;
        self.root = SessionStore::canonical_workspace(restored.workspace_root.clone())?;
        self.config = config::load(&self.root)?.config;
        self.backend = None;
        self.model = clean_model(restored.model.clone()).or_else(|| self.model.clone());
        self.model_view = None;
        self.model_name = None;
        self.context_seed = Some(restored.context);
        self.continuation = restored.continuation;
        self.session_id = Some(restored.session_id);
        self.turn_number = 0;
        self.session_started = true;
        for warning in restored.warnings {
            eprintln!("AETHER Fx: {}", safe_message(&warning));
        }
        println!("resumed session {}", self.session_id.as_ref().expect("session set"));
        Ok(())
    }

    async fn clear_session(&mut self) -> Result<(), AppError> {
        self.close_current().await?;
        self.session_id = None;
        self.context_seed = None;
        self.continuation = None;
        self.turn_number = 0;
        self.session_started = false;
        if !self.json_output {
            println!("started a fresh local session; model selection is preserved");
        }
        Ok(())
    }

    async fn close_current(&mut self) -> Result<(), AppError> {
        let mut first_error = None;
        if let Some(provider) = self.obscura.take()
            && let Err(error) = provider.shutdown().await
        {
            first_error = Some(AppError::Obscura(error));
        }
        if let Some(store) = self.store.take()
            && let Err(error) = persist_finished(&store).await
        {
            first_error = Some(error);
        }
        self.agent = None;
        if let Some(workspace) = self.workspace.take() {
            let cleanup = workspace.shutdown_processes().await;
            if !cleanup.is_clean() {
                report_shutdown_failure(&cleanup);
                if first_error.is_none() {
                    first_error =
                        Some(AppError::Message("owned process cleanup failed".to_owned()));
                }
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        Ok(())
    }

    async fn reset_agent_for_model_switch(&mut self) -> Result<(), AppError> {
        let Some(workspace) = self.workspace.clone() else {
            self.agent = None;
            return Ok(());
        };
        let cleanup = workspace.shutdown_processes().await;
        if !cleanup.is_clean() {
            report_shutdown_failure(&cleanup);
            return Err(AppError::Message(
                "cannot switch models while owned process cleanup is incomplete".to_owned(),
            ));
        }
        self.agent = None;
        self.workspace = None;
        Ok(())
    }
}

fn is_browser_command(command: &str) -> bool {
    command == "/browser"
        || command
            .strip_prefix("/browser")
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
}

fn obscura_status_from_installation(
    paths: &ObscuraPaths,
    installation: ObscuraInstallationStatus,
) -> ObscuraStatus {
    ObscuraStatus {
        installed: installation.installed,
        active: false,
        healthy: false,
        expected_version: paths.artifact().version.to_owned(),
        installed_version: installation.installed_version,
        mcp_protocol_version: None,
        profile_id: paths.profile_id().to_owned(),
    }
}

fn clean_model(model: Option<String>) -> Option<String> {
    model.filter(|value| {
        !value.trim().is_empty()
            && value.len() <= 256
            && !value.chars().any(char::is_control)
            && !value.chars().any(char::is_whitespace)
    })
}

fn resume_model_changed(session: &ResumableSession, selected: Option<&str>) -> bool {
    let previous =
        clean_model(session.model.clone()).or_else(|| clean_model(session.context.model.clone()));
    match (selected, previous.as_deref()) {
        (Some(selected), Some(previous)) => selected != previous,
        (Some(_), None) => true,
        _ => false,
    }
}

fn command_items() -> Vec<SelectorItem<String>> {
    [
        ("/model", "Model", "Choose a live Rainy model"),
        ("/status", "Status", "Show session and workflow state"),
        ("/context", "Context", "Show bounded evidence and workflow context"),
        ("/config", "Config", "Show or update local preferences"),
        ("/auth", "Auth", "Show credential status or enter a key"),
        ("/sessions", "Sessions", "Pick a local session"),
        ("/resume", "Resume", "Pick and restore a local session"),
        ("/doctor", "Doctor", "Run offline diagnostics"),
        ("/update", "Update", "Update AETHER Fx and leave the shell"),
        ("/browser", "Browser", "Install/start Obscura or run read-only web research"),
        ("/help", "Help", "Show local commands"),
        ("/clear", "Clear", "Finish this session and start a fresh one"),
        ("/exit", "Exit", "Finish this session and leave the shell"),
    ]
    .into_iter()
    .take(MAX_SHELL_COMMANDS)
    .map(|(value, label, detail)| {
        SelectorItem::new(value.to_owned(), label, Some(detail.to_owned()))
    })
    .collect()
}

fn print_turn_summary(turn_id: &TurnId, context: &ContextSnapshot, model: &str) {
    println!(
        "turn {} · model {} · phase {} · verification {} · inspected {} · modified {}",
        turn_id,
        model,
        context.workflow.director.phase.as_str(),
        context.workflow.verification.as_str(),
        context.inspected.len(),
        context.modified.len()
    );
}

fn print_identity(state: &SessionRuntime) {
    println!(
        "AETHER Fx {VERSION} · workspace {} · branch {}",
        safe_message(&compact_path(&state.root)),
        git_branch(&state.root).as_deref().unwrap_or("none")
    );
    println!(
        "model: {} · permissions: {} · change model: /model",
        state.model_identity(),
        if state.yolo { "yolo" } else { "prompt" }
    );
    println!("type / for local commands; startup is offline");
}

fn format_model_identity(
    model: Option<&str>,
    view: Option<&ModelView>,
    fallback_name: Option<&str>,
) -> String {
    let name = view
        .map(|view| view.display_name.as_str())
        .or(fallback_name)
        .or(model)
        .map(safe_message)
        .unwrap_or_else(|| "not selected".to_owned());
    let id = model.map(safe_message);
    match id {
        Some(id) if id != name => format!("{name} ({id})"),
        _ => name,
    }
}

fn compact_path(path: &Path) -> String {
    let value = path.display().to_string();
    if let Ok(home) = std::env::var("HOME")
        && value == home
    {
        return "~".to_owned();
    }
    if let Ok(home) = std::env::var("HOME")
        && let Some(suffix) = value.strip_prefix(&(home.clone() + "/"))
    {
        return format!("~/{suffix}");
    }
    value
}

fn git_branch(root: &Path) -> Option<String> {
    let output = ProcessCommand::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    let branch = safe_message(branch.trim());
    (!branch.is_empty()).then_some(branch)
}

async fn run_session(
    model: Option<String>,
    root: PathBuf,
    resume: Option<ResumableSession>,
    initial_prompt: Option<String>,
    single_pass: bool,
    options: SessionOptions,
) -> Result<(), AppError> {
    let mut state = SessionRuntime::new(
        model,
        root,
        resume.clone(),
        options.json_output,
        options.yolo,
        options.noninteractive,
    )?;
    if single_pass {
        state.interactive_ui = false;
    }
    if resume.is_some() && !state.interactive_ui && initial_prompt.is_none() {
        let _ = state.ensure_backend().await?;
    }
    if !options.json_output && state.interactive_ui {
        print_identity(&state);
        if let Some(resume) = resume.as_ref() {
            for warning in &resume.warnings {
                eprintln!("AETHER Fx: {}", safe_message(warning));
            }
        }
    }
    let result = state.run(initial_prompt, single_pass).await;
    let cleanup = state.close_current().await;
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
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
    noninteractive: bool,
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
            safe_message(&restored.workspace_root.display().to_string())
        )));
    }
    run_session(
        model,
        root,
        Some(restored),
        None,
        false,
        SessionOptions { json_output, yolo, noninteractive },
    )
    .await
}

async fn run_resume_latest(
    model: Option<String>,
    root: PathBuf,
    json_output: bool,
    yolo: bool,
    noninteractive: bool,
) -> Result<(), AppError> {
    let (restored, issues) = tokio::task::spawn_blocking({
        let root = root.clone();
        move || SessionStore::restore_latest(root)
    })
    .await
    .map_err(|error| AppError::Message(format!("session worker failed: {error}")))??;
    for issue in issues {
        eprintln!("AETHER Fx: skipped corrupt session: {}", safe_message(&issue));
    }
    let Some(restored) = restored else {
        return Err(AppError::Message("session not found".to_owned()));
    };
    run_session(
        model,
        root,
        Some(restored),
        None,
        false,
        SessionOptions { json_output, yolo, noninteractive },
    )
    .await
}

fn report_shutdown_failure(report: &ProcessShutdownReport) {
    eprintln!(
        "AETHER Fx: HIGH-SEVERITY owned process cleanup failure ({} remaining): {}",
        report.failures.len(),
        safe_message(&report.failure_summary())
    );
}

async fn new_tool_registry(
    root: PathBuf,
    obscura: Option<Arc<ObscuraSupervisor>>,
) -> Result<ToolRegistry, AppError> {
    tokio::task::spawn_blocking(move || {
        let workspace = aether_tools::Workspace::new(root).map_err(|error| error.to_string())?;
        Ok::<_, String>(match obscura {
            Some(obscura) => ToolRegistry::with_obscura(workspace, obscura),
            None => ToolRegistry::with_workspace(workspace),
        })
    })
    .await
    .map_err(|error| AppError::Message(format!("tool registry worker failed: {error}")))?
    .map_err(AppError::Message)
}

fn configured_model_from(
    cli_model: Option<String>,
    env_model: Option<String>,
) -> Result<String, AppError> {
    let model = cli_model.or(env_model);
    let Some(model) = clean_model(model) else {
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
    interactive_ui: bool,
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
                    if !interactive_ui || resolver.is_none() {
                        cancellation.cancel();
                        broker.cancel(&request.call_id);
                        continue;
                    }
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
            if json_output {
                return Ok(None);
            }
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

    #[test]
    fn browser_commands_are_local_and_do_not_match_similar_prompt_text() {
        assert!(is_browser_command("/browser"));
        assert!(is_browser_command("/browser status"));
        assert!(is_browser_command("/browser investigate the docs"));
        assert!(!is_browser_command("/browsering"));
        assert!(!is_browser_command("tell me /browser status"));
    }

    #[test]
    fn resumed_model_change_invalidates_provider_continuation() {
        let session = ResumableSession {
            session_id: SessionId::new("resume-model").unwrap(),
            workspace_root: PathBuf::from("/workspace"),
            model: Some("old-model".to_owned()),
            continuation: Some(OpaqueContinuation(serde_json::json!({
                "previous_response_id": "response-old"
            }))),
            context: ContextSnapshot::new("/workspace", Some("old-model".to_owned())),
            workspace_changed: false,
            warnings: Vec::new(),
        };
        assert!(!resume_model_changed(&session, Some("old-model")));
        assert!(resume_model_changed(&session, Some("new-model")));
        assert!(!resume_model_changed(&session, None));

        let mut modelless = session;
        modelless.model = None;
        assert!(resume_model_changed(&modelless, Some("new-model")));
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
