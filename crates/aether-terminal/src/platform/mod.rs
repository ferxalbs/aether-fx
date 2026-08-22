use std::io;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{State, enter, restore};

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{State, enter, restore};

#[cfg(not(any(unix, windows)))]
mod fallback {
    use std::io;

    #[derive(Debug, Default)]
    pub(crate) struct State;

    pub(crate) fn enter() -> io::Result<State> {
        Ok(State)
    }

    pub(crate) fn restore(_state: &mut State) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) use fallback::{State, enter, restore};

#[allow(dead_code)]
fn _platform_result_type(_: io::Result<State>) {}
