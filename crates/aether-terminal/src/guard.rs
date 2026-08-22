use std::io::{self, IsTerminal, Write};

use aether_core::{PermissionDecision, PermissionRequest};

use crate::{input, platform};

/// RAII guard that restores terminal modes on every normal/unwinding exit.
pub struct TerminalGuard {
    state: Option<platform::State>,
}

impl TerminalGuard {
    /// Enter native raw/VT mode when stdin and stdout are terminals.
    pub fn enter() -> io::Result<Self> {
        Ok(Self { state: Some(platform::enter()?) })
    }

    /// Whether the process has an interactive terminal attached.
    pub fn is_interactive(&self) -> bool {
        io::stdin().is_terminal() && io::stdout().is_terminal()
    }

    /// Restore terminal state immediately. Drop repeats safely if needed.
    pub fn restore(&mut self) -> io::Result<()> {
        let Some(mut state) = self.state.take() else {
            return Ok(());
        };
        let result = platform::restore(&mut state);
        if result.is_err() {
            self.state = Some(state);
        }
        result
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Run the intentionally small initial terminal shell and return one entered line.
pub fn run_minimal_shell() -> io::Result<Option<String>> {
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    stdout.write_all("\x1b[2K\r› ".as_bytes())?;
    stdout.flush()?;
    let line = if guard.is_interactive() {
        input::read_line_from(&mut io::stdin().lock(), &mut stdout)?
    } else {
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        Some(line.trim_end_matches(['\r', '\n']).to_owned())
    };
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    guard.restore()?;
    Ok(line)
}

/// Present one structured permission request using the same monochrome terminal substrate.
pub fn prompt_permission(request: &PermissionRequest) -> io::Result<Option<PermissionDecision>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(None);
    }
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    write!(stdout, "\n{}", request.operation)?;
    if let Some(target) = request.target.as_deref() {
        write!(stdout, " {}", target)?;
    }
    if !request.details.is_null() {
        let details = serde_json::to_string(&request.details).unwrap_or_else(|_| "{}".to_owned());
        write!(stdout, "\n  {}", details)?;
    }
    stdout.write_all(b"\n\n[y] allow once\n[s] allow session\n[n] deny\n")?;
    stdout.flush()?;
    let decision = loop {
        match input::read_event(&mut io::stdin().lock())? {
            Some(input::InputEvent::Character('y' | 'Y')) => {
                break Some(PermissionDecision::AllowOnce);
            }
            Some(input::InputEvent::Character('s' | 'S')) => {
                break Some(PermissionDecision::AllowSession);
            }
            Some(input::InputEvent::Character('n' | 'N'))
            | Some(input::InputEvent::CtrlC)
            | Some(input::InputEvent::CtrlD)
            | None => break Some(PermissionDecision::Deny),
            Some(input::InputEvent::Enter)
            | Some(input::InputEvent::Backspace)
            | Some(input::InputEvent::Escape)
            | Some(input::InputEvent::Character(_)) => {}
        }
    };
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    guard.restore()?;
    Ok(decision)
}
