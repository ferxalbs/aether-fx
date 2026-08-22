use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use aether_agent::{Agent, AgentError, AgentRequest, CancellationToken, ModelBackend};
use aether_core::{SessionId, TurnId};
use aether_rainy::RainyBackend;
use aether_terminal::Renderer;
use aether_tools::ToolRegistry;
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
            let prompt = aether_terminal::run_minimal_shell()?;
            if let Some(prompt) = prompt.filter(|value| !value.trim().is_empty()) {
                with_runtime(|runtime| runtime.block_on(run_prompt(prompt, None, cli.root)))
            } else {
                Ok(())
            }
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
    let backend = RainyBackend::from_env().map_err(|error| AppError::Message(error.to_string()))?;
    let tools = ToolRegistry::new(&root).map_err(AppError::Message)?;
    let agent = Agent::new(backend, tools);
    let session_id = SessionId::new(format!("session-{}", std::process::id()))
        .map_err(|error| AppError::Message(error.to_string()))?;
    let turn_id = TurnId::new(format!("turn-{}", std::process::id()))
        .map_err(|error| AppError::Message(error.to_string()))?;
    let mut request = AgentRequest::new(session_id, turn_id, prompt);
    request.model = model;
    let (sender, mut receiver) = mpsc::channel(64);
    let cancellation = CancellationToken::new();
    let mut run = Box::pin(agent.run(request, sender, cancellation.clone()));
    let mut renderer = Renderer::new();
    let mut stdout = io::stdout().lock();
    let mut interrupted = false;
    let result = loop {
        tokio::select! {
            result = &mut run => break result,
            event = receiver.recv() => {
                if let Some(event) = event {
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
    renderer.render_if_due(&mut stdout, true)?;
    result?;
    stdout.flush()?;
    Ok(())
}
