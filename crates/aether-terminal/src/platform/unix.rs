use std::io;
use std::io::IsTerminal;
use std::os::fd::AsFd;

use rustix::termios::{OptionalActions, Termios, tcgetattr, tcsetattr};

#[derive(Debug)]
pub(crate) struct State {
    original: Option<Termios>,
}

pub(crate) fn enter() -> io::Result<State> {
    let stdin = io::stdin();
    if !stdin.is_terminal() {
        return Ok(State { original: None });
    }
    let fd = stdin.as_fd();
    let original = tcgetattr(fd).map_err(io::Error::from)?;
    let mut raw = original.clone();
    raw.make_raw();
    tcsetattr(fd, OptionalActions::Now, &raw).map_err(io::Error::from)?;
    Ok(State { original: Some(original) })
}

pub(crate) fn restore(state: &mut State) -> io::Result<()> {
    let Some(original) = state.original.as_ref() else {
        return Ok(());
    };
    let stdin = io::stdin();
    let result = tcsetattr(stdin.as_fd(), OptionalActions::Now, original).map_err(io::Error::from);
    if result.is_ok() {
        state.original = None;
    }
    result
}
