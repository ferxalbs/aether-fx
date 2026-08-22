use std::io::{self, IsTerminal, Write};

use aether_core::{PermissionDecision, PermissionRequest};

use crate::{input, platform};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionPromptOutcome {
    Decision(PermissionDecision),
    CancelTurn,
    EndOfInput,
}

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
pub fn prompt_permission(request: &PermissionRequest) -> io::Result<PermissionPromptOutcome> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(PermissionPromptOutcome::EndOfInput);
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
    let decision = read_permission_outcome(&mut io::stdin().lock())?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    guard.restore()?;
    Ok(decision)
}

fn read_permission_outcome<R: io::Read>(reader: &mut R) -> io::Result<PermissionPromptOutcome> {
    loop {
        match input::read_event(reader)? {
            Some(input::InputEvent::Character('y' | 'Y')) => {
                return Ok(PermissionPromptOutcome::Decision(PermissionDecision::AllowOnce));
            }
            Some(input::InputEvent::Character('s' | 'S')) => {
                return Ok(PermissionPromptOutcome::Decision(PermissionDecision::AllowSession));
            }
            Some(input::InputEvent::Character('n' | 'N')) => {
                return Ok(PermissionPromptOutcome::Decision(PermissionDecision::Deny));
            }
            Some(input::InputEvent::CtrlC) => return Ok(PermissionPromptOutcome::CancelTurn),
            Some(input::InputEvent::CtrlD) | None => {
                return Ok(PermissionPromptOutcome::EndOfInput);
            }
            Some(input::InputEvent::Enter)
            | Some(input::InputEvent::Backspace)
            | Some(input::InputEvent::Escape)
            | Some(input::InputEvent::Character(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_ctrl_c_is_terminal_turn_cancellation() {
        assert_eq!(
            read_permission_outcome(&mut b"\x03".as_slice()).unwrap(),
            PermissionPromptOutcome::CancelTurn
        );
    }

    #[test]
    fn permission_deny_remains_distinct_from_ctrl_c() {
        assert_eq!(
            read_permission_outcome(&mut b"n".as_slice()).unwrap(),
            PermissionPromptOutcome::Decision(PermissionDecision::Deny)
        );
    }
}
