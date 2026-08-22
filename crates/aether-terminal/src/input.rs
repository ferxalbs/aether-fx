use std::io::{self, Read, Write};

use unicode_segmentation::UnicodeSegmentation;

/// A small raw-input vocabulary used by the initial shell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InputEvent {
    /// A decoded Unicode scalar.
    Character(char),
    /// Enter/return.
    Enter,
    /// Backspace/delete backwards.
    Backspace,
    /// Ctrl+C.
    CtrlC,
    /// Ctrl+D.
    CtrlD,
    /// Escape or an unsupported control sequence.
    Escape,
}

/// Read one simple raw input event.
pub fn read_event<R: Read>(reader: &mut R) -> io::Result<Option<InputEvent>> {
    let mut first = [0_u8; 1];
    if reader.read(&mut first)? == 0 {
        return Ok(None);
    }
    let event = match first[0] {
        b'\r' | b'\n' => InputEvent::Enter,
        0x03 => InputEvent::CtrlC,
        0x04 => InputEvent::CtrlD,
        0x08 | 0x7F => InputEvent::Backspace,
        0x1B => InputEvent::Escape,
        byte if byte.is_ascii() && byte >= 0x20 => InputEvent::Character(byte as char),
        byte @ 0xC2..=0xDF => decode_utf8(reader, byte, 2)?,
        byte @ 0xE0..=0xEF => decode_utf8(reader, byte, 3)?,
        byte @ 0xF0..=0xF4 => decode_utf8(reader, byte, 4)?,
        _ => InputEvent::Escape,
    };
    Ok(Some(event))
}

fn decode_utf8<R: Read>(reader: &mut R, first: u8, width: usize) -> io::Result<InputEvent> {
    let mut bytes = [0_u8; 4];
    bytes[0] = first;
    reader.read_exact(&mut bytes[1..width])?;
    Ok(std::str::from_utf8(&bytes[..width])
        .ok()
        .and_then(|text| text.chars().next())
        .map(InputEvent::Character)
        .unwrap_or(InputEvent::Escape))
}

/// Read a line from a raw reader and echo only the necessary terminal controls.
pub fn read_line_from<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
) -> io::Result<Option<String>> {
    let mut line = String::new();
    loop {
        let Some(event) = read_event(reader)? else {
            return Ok(if line.is_empty() { None } else { Some(line) });
        };
        match event {
            InputEvent::Character(character) => {
                line.push(character);
                write_char(writer, character)?;
            }
            InputEvent::Enter => return Ok(Some(line)),
            InputEvent::Backspace => {
                if let Some((index, _)) = line.grapheme_indices(true).next_back() {
                    line.truncate(index);
                    writer.write_all(b"\x08 \x08")?;
                    writer.flush()?;
                }
            }
            InputEvent::CtrlC | InputEvent::CtrlD => return Ok(None),
            InputEvent::Escape => {}
        }
        writer.flush()?;
    }
}

fn write_char<W: Write>(writer: &mut W, character: char) -> io::Result<()> {
    let mut buffer = [0_u8; 4];
    writer.write_all(character.encode_utf8(&mut buffer).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_input_handles_grapheme_backspace() {
        let mut input = "é\x08\n".as_bytes();
        let mut output = Vec::new();
        assert_eq!(read_line_from(&mut input, &mut output).unwrap(), Some(String::new()));
        assert_eq!(output, "é\x08 \x08".as_bytes());
    }
}
