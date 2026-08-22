use std::io;
use std::io::IsTerminal;

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::System::Console::{
    CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
    ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE,
    STD_OUTPUT_HANDLE, SetConsoleMode,
};

#[derive(Debug, Default)]
pub(crate) struct State {
    input: Option<(HANDLE, CONSOLE_MODE)>,
    output: Option<(HANDLE, CONSOLE_MODE)>,
}

pub(crate) fn enter() -> io::Result<State> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(State::default());
    }
    // SAFETY: GetStdHandle is called with the documented standard-handle constants.
    let input = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    // SAFETY: GetStdHandle is called with the documented standard-handle constants.
    let output = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let mut state = State::default();
    if !input.is_null() {
        let mut mode = 0;
        // SAFETY: input is the live standard input handle and mode is writable storage.
        if unsafe { GetConsoleMode(input, &mut mode) } != 0 {
            let raw_mode =
                (mode & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT)) | ENABLE_VIRTUAL_TERMINAL_INPUT;
            // SAFETY: input was returned by GetStdHandle and raw_mode contains console flags.
            if unsafe { SetConsoleMode(input, raw_mode) } == 0 {
                return Err(io::Error::last_os_error());
            }
            state.input = Some((input, mode));
        }
    }
    if !output.is_null() {
        let mut mode = 0;
        // SAFETY: output is the live standard output handle and mode is writable storage.
        if unsafe { GetConsoleMode(output, &mut mode) } != 0 {
            // SAFETY: output was returned by GetStdHandle and the mode preserves existing flags.
            if unsafe { SetConsoleMode(output, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING) } == 0 {
                let error = io::Error::last_os_error();
                let _ = restore(&mut state);
                return Err(error);
            }
            state.output = Some((output, mode));
        }
    }
    Ok(state)
}

pub(crate) fn restore(state: &mut State) -> io::Result<()> {
    let mut first_error = None;
    if let Some((handle, mode)) = state.input.take() {
        // SAFETY: `handle` and `mode` were returned by GetStdHandle/GetConsoleMode and remain
        // owned by the process for the lifetime of this guard.
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            first_error = Some(io::Error::last_os_error());
        }
    }
    if let Some((handle, mode)) = state.output.take() {
        // SAFETY: `handle` and `mode` were returned by GetStdHandle/GetConsoleMode and remain
        // owned by the process for the lifetime of this guard.
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            first_error.get_or_insert_with(io::Error::last_os_error);
        }
    }
    first_error.map_or(Ok(()), Err)
}
