use std::io::{self, IsTerminal, Write};

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
    stdout.write_all(b"aether> ")?;
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
