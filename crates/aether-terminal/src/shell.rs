use std::io::{self, IsTerminal, Read, Write};

use unicode_segmentation::UnicodeSegmentation;

use crate::selector::clear_line;
use crate::{SelectorItem, SelectorOutcome, TerminalGuard, input, read_event, select_from_items};

/// Result of one shell input operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShellInput {
    /// Ordinary user text that can be sent to the agent.
    Line(String),
    /// A local slash command selected from the palette.
    Command(String),
    /// Ctrl-D or end-of-file.
    EndOfInput,
    /// Ctrl-C cancelled the current line.
    Cancelled,
}

/// Read one shell line from raw streams. A leading slash opens the shared palette.
pub fn read_shell_input_from<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    commands: &[SelectorItem<String>],
) -> io::Result<ShellInput> {
    let mut line = String::new();
    loop {
        let Some(event) = read_event(reader)? else {
            return Ok(if line.is_empty() {
                ShellInput::EndOfInput
            } else {
                ShellInput::Line(line)
            });
        };
        match event {
            crate::InputEvent::Character('/') if line.is_empty() => {
                clear_line(writer)?;
                writer.flush()?;
                match select_from_items(reader, writer, "Commands", commands, None)? {
                    SelectorOutcome::Selected(command) => return Ok(ShellInput::Command(command)),
                    SelectorOutcome::EndOfInput => return Ok(ShellInput::EndOfInput),
                    SelectorOutcome::Cancelled | SelectorOutcome::NoSelection => {
                        return Ok(ShellInput::Cancelled);
                    }
                }
            }
            crate::InputEvent::Character(character) => {
                if line.len().saturating_add(character.len_utf8()) <= input::MAX_INPUT_LINE_BYTES {
                    line.push(character);
                    write_char(writer, character)?;
                }
            }
            crate::InputEvent::Enter => return Ok(ShellInput::Line(line)),
            crate::InputEvent::Backspace => {
                if let Some((index, _)) = line.grapheme_indices(true).next_back() {
                    line.truncate(index);
                    writer.write_all(b"\x08 \x08")?;
                }
            }
            crate::InputEvent::CtrlC => return Ok(ShellInput::Cancelled),
            crate::InputEvent::CtrlD => return Ok(ShellInput::EndOfInput),
            crate::InputEvent::Escape
            | crate::InputEvent::Up
            | crate::InputEvent::Down
            | crate::InputEvent::Tab => {}
        }
        writer.flush()?;
    }
}

/// Read one shell input operation, using raw mode only for an interactive terminal.
pub fn run_shell_input(commands: &[SelectorItem<String>]) -> io::Result<ShellInput> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let Some(line) = read_plain_line()? else {
            return Ok(ShellInput::EndOfInput);
        };
        return Ok(if line.starts_with('/') {
            ShellInput::Command(line)
        } else {
            ShellInput::Line(line)
        });
    }
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    clear_line(&mut stdout)?;
    stdout.write_all("\r› ".as_bytes())?;
    stdout.flush()?;
    let result = read_shell_input_from(&mut io::stdin().lock(), &mut stdout, commands);
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    guard.restore()?;
    result
}

/// Alias for callers that want the primitive's semantic name.
pub fn read_shell_input(commands: &[SelectorItem<String>]) -> io::Result<ShellInput> {
    run_shell_input(commands)
}

/// Read one line without a prompt or terminal escape sequence.
pub fn read_plain_line() -> io::Result<Option<String>> {
    read_plain_line_from(&mut io::stdin().lock())
}

/// Read one bounded newline-delimited line without emitting terminal controls.
pub fn read_plain_line_from<R: Read>(reader: &mut R) -> io::Result<Option<String>> {
    let mut bytes = Vec::with_capacity(input::MAX_INPUT_LINE_BYTES.min(256));
    let mut saw_input = false;
    loop {
        let mut byte = [0_u8; 1];
        if reader.read(&mut byte)? == 0 {
            break;
        }
        saw_input = true;
        if byte[0] == b'\n' {
            break;
        }
        if bytes.len() < input::MAX_INPUT_LINE_BYTES {
            bytes.push(byte[0]);
        }
    }
    if !saw_input {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
}

/// Read a secret from raw streams without echoing, masking, or persisting it.
pub fn read_secret_from<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    max_bytes: usize,
) -> io::Result<Option<String>> {
    let limit = max_bytes.clamp(1, 64 * 1024);
    let mut secret = String::new();
    loop {
        let Some(event) = read_event(reader)? else {
            return Ok(None);
        };
        match event {
            crate::InputEvent::Character(character) => {
                if secret.len().saturating_add(character.len_utf8()) <= limit {
                    secret.push(character);
                }
            }
            crate::InputEvent::Backspace => {
                if let Some((index, _)) = secret.grapheme_indices(true).next_back() {
                    secret.truncate(index);
                }
            }
            crate::InputEvent::Enter => return Ok(Some(secret)),
            crate::InputEvent::CtrlC | crate::InputEvent::CtrlD => return Ok(None),
            crate::InputEvent::Escape
            | crate::InputEvent::Up
            | crate::InputEvent::Down
            | crate::InputEvent::Tab => {}
        }
        writer.flush()?;
    }
}

/// Read a secret while the terminal is in raw mode. The prompt is caller-owned.
pub fn read_secret(max_bytes: usize) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(None);
    }
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    let result = read_secret_from(&mut io::stdin().lock(), &mut stdout, max_bytes);
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    guard.restore()?;
    result
}

/// Read a bounded yes/no confirmation from raw streams.
pub fn confirm_from<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    default: bool,
) -> io::Result<Option<bool>> {
    loop {
        let Some(event) = read_event(reader)? else {
            return Ok(None);
        };
        match event {
            crate::InputEvent::Character('y' | 'Y') => return Ok(Some(true)),
            crate::InputEvent::Character('n' | 'N') => return Ok(Some(false)),
            crate::InputEvent::Enter => return Ok(Some(default)),
            crate::InputEvent::CtrlC | crate::InputEvent::CtrlD | crate::InputEvent::Escape => {
                return Ok(None);
            }
            crate::InputEvent::Backspace
            | crate::InputEvent::Up
            | crate::InputEvent::Down
            | crate::InputEvent::Tab
            | crate::InputEvent::Character(_) => {}
        }
        writer.flush()?;
    }
}

/// Prompt for a bounded yes/no confirmation while raw mode is active.
pub fn confirm(prompt: &str, default: bool) -> io::Result<Option<bool>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(None);
    }
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    write!(stdout, "{} [{}] ", prompt, if default { "Y/n" } else { "y/N" })?;
    stdout.flush()?;
    let result = confirm_from(&mut io::stdin().lock(), &mut stdout, default);
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    guard.restore()?;
    result
}

fn write_char<W: Write>(writer: &mut W, character: char) -> io::Result<()> {
    let mut buffer = [0_u8; 4];
    writer.write_all(character.encode_utf8(&mut buffer).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands() -> Vec<SelectorItem<String>> {
        vec![
            SelectorItem::new("/help".to_owned(), "Help", None),
            SelectorItem::new("/status".to_owned(), "Status", None),
        ]
    }

    #[test]
    fn slash_palette_returns_a_local_command() {
        let mut input = b"/sta\r".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            read_shell_input_from(&mut input, &mut output, &commands()).unwrap(),
            ShellInput::Command("/status".to_owned())
        );
    }

    #[test]
    fn escape_cancels_palette_without_sending_a_slash() {
        let mut input = b"/\x1b".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            read_shell_input_from(&mut input, &mut output, &commands()).unwrap(),
            ShellInput::Cancelled
        );
    }

    #[test]
    fn ordinary_slashes_inside_text_are_ordinary_text() {
        let mut input = b"read /tmp/file\r".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            read_shell_input_from(&mut input, &mut output, &commands()).unwrap(),
            ShellInput::Line("read /tmp/file".to_owned())
        );
    }

    #[test]
    fn secret_input_never_echoes_bytes() {
        let mut input = "sëcret\r".as_bytes();
        let mut output = Vec::new();
        assert_eq!(
            read_secret_from(&mut input, &mut output, 4096).unwrap(),
            Some("sëcret".to_owned())
        );
        assert!(output.is_empty());
    }

    #[test]
    fn plain_line_input_is_bounded_and_discards_the_remainder_of_one_line() {
        let payload = format!("{}overflow\nnext\n", "x".repeat(input::MAX_INPUT_LINE_BYTES));
        let mut input = payload.as_bytes();
        assert_eq!(
            read_plain_line_from(&mut input).unwrap().map(|line| line.len()),
            Some(input::MAX_INPUT_LINE_BYTES)
        );
        assert_eq!(read_plain_line_from(&mut input).unwrap(), Some("next".to_owned()));
    }
}
