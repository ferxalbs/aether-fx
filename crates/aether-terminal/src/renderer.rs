use std::io::{self, Write};
use std::time::{Duration, Instant};

use aether_core::AgentEvent;

use crate::{Style, TerminalBuffer, sanitize_terminal_text};

const INITIAL_RENDER_INTERVAL: Duration = Duration::from_millis(8);

/// Incremental event renderer with a coalesced 8 ms render budget.
pub struct Renderer {
    buffer: TerminalBuffer,
    last_render: Instant,
    render_interval: Duration,
    interactive: bool,
    rendered_line_count: usize,
    rendered_last_line_bytes: usize,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    /// Construct an empty renderer.
    pub fn new() -> Self {
        Self::new_with_mode(true)
    }

    /// Construct a renderer with ANSI repainting enabled or disabled.
    pub fn new_with_mode(interactive: bool) -> Self {
        Self {
            buffer: TerminalBuffer::new(),
            last_render: Instant::now() - INITIAL_RENDER_INTERVAL,
            render_interval: INITIAL_RENDER_INTERVAL,
            interactive,
            rendered_line_count: 0,
            rendered_last_line_bytes: 0,
        }
    }

    /// Construct a plain renderer suitable for pipes and JSON-adjacent diagnostics.
    pub fn new_plain() -> Self {
        Self::new_with_mode(false)
    }

    /// Apply one agent event without writing to stdout.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TextDelta { text } => append_safe(&mut self.buffer, text.as_str()),
            AgentEvent::ToolStarted { name, .. } => {
                start_event_line(&mut self.buffer);
                let label = format!("[tool {}]", sanitize_terminal_text(name));
                if self.interactive {
                    self.buffer.append(&Style::new(crate::Intensity::Secondary).paint(&label));
                } else {
                    self.buffer.append(&label);
                }
            }
            AgentEvent::ToolOutput { output, .. } => {
                start_event_line(&mut self.buffer);
                append_safe(&mut self.buffer, output.as_str());
            }
            AgentEvent::ToolFinished { ok, .. } => {
                start_event_line(&mut self.buffer);
                self.buffer.append(if *ok { "[tool done]" } else { "[tool failed]" });
            }
            AgentEvent::PermissionRequested { .. } | AgentEvent::PermissionResolved { .. } => {}
            AgentEvent::Usage { .. } => {}
            AgentEvent::Warning { message } => {
                start_event_line(&mut self.buffer);
                append_safe(&mut self.buffer, message.as_str());
            }
            AgentEvent::Error { message } => {
                start_event_line(&mut self.buffer);
                append_safe(&mut self.buffer, message.as_str());
            }
            AgentEvent::Done => {
                start_event_line(&mut self.buffer);
            }
        }
    }

    /// Apply an event and render only when the coalescing interval has elapsed.
    pub fn handle<W: Write>(&mut self, event: &AgentEvent, writer: &mut W) -> io::Result<()> {
        if !self.interactive {
            self.apply(event);
            if let Some(text) = plain_event_text(event) {
                writer.write_all(text.as_bytes())?;
                writer.flush()?;
            }
            return Ok(());
        }
        self.apply(event);
        self.render_if_due(writer, false)
    }

    /// Render dirty lines when due, or all dirty lines when `force` is true.
    pub fn render_if_due<W: Write>(&mut self, writer: &mut W, force: bool) -> io::Result<()> {
        if !self.interactive {
            return Ok(());
        }
        if !force && self.last_render.elapsed() < self.render_interval {
            return Ok(());
        }
        if self.buffer.take_dirty().is_empty() {
            self.last_render = Instant::now();
            return Ok(());
        }

        // Agent output is append-only: text extends the last line, while tool
        // and diagnostic events add new lines. Emit only the new suffix instead
        // of repainting each accumulated line on every streamed delta.
        let lines = self.buffer.lines();
        let line_count = lines.len();
        let last_line_bytes = lines.last().map_or(0, String::len);
        let mut frame = Vec::new();
        if self.rendered_line_count == 0 {
            if !(lines.len() == 1 && lines[0].is_empty()) {
                for (index, line) in lines.iter().enumerate() {
                    if index > 0 {
                        frame.extend_from_slice(b"\n\r");
                    }
                    frame.extend_from_slice(line.as_bytes());
                }
            }
        } else {
            if let Some(line) = lines.get(self.rendered_line_count - 1)
                && line.len() > self.rendered_last_line_bytes
            {
                frame.extend_from_slice(&line.as_bytes()[self.rendered_last_line_bytes..]);
            }
            for line in lines.iter().skip(self.rendered_line_count) {
                frame.extend_from_slice(b"\n\r");
                frame.extend_from_slice(line.as_bytes());
            }
        }

        if !frame.is_empty() {
            writer.write_all(&frame)?;
            writer.flush()?;
            self.rendered_line_count = line_count;
            self.rendered_last_line_bytes = last_line_bytes;
        }
        self.last_render = Instant::now();
        Ok(())
    }

    /// Access the current buffer for tests and diagnostics.
    pub fn buffer(&self) -> &TerminalBuffer {
        &self.buffer
    }
}

fn append_safe(buffer: &mut TerminalBuffer, text: &str) {
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            buffer.newline();
        }
        buffer.append(&sanitize_terminal_text(line));
    }
}

fn start_event_line(buffer: &mut TerminalBuffer) {
    if buffer.lines().last().is_some_and(|line| !line.is_empty()) {
        buffer.newline();
    }
}

fn plain_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character == '\n' {
                character
            } else if character.is_control() {
                '�'
            } else {
                character
            }
        })
        .collect()
}

fn plain_event_text(event: &AgentEvent) -> Option<String> {
    match event {
        AgentEvent::TextDelta { text } => Some(plain_text(text.as_str())),
        AgentEvent::ToolStarted { name, .. } => {
            Some(format!("[tool {}]\n", sanitize_terminal_text(name)))
        }
        AgentEvent::ToolOutput { output, .. } => Some(format!("{}\n", plain_text(output.as_str()))),
        AgentEvent::ToolFinished { ok, .. } => {
            Some(if *ok { "[tool done]\n".to_owned() } else { "[tool failed]\n".to_owned() })
        }
        AgentEvent::Warning { message } => Some(format!("{}\n", plain_text(message.as_str()))),
        AgentEvent::Error { message } => Some(format!("{}\n", plain_text(message.as_str()))),
        AgentEvent::Done => Some("\n".to_owned()),
        AgentEvent::PermissionRequested { .. }
        | AgentEvent::PermissionResolved { .. }
        | AgentEvent::Usage { .. } => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_core::BoundedText;

    #[test]
    fn renderer_coalesces_until_forced() {
        let mut renderer = Renderer::new();
        let event = AgentEvent::TextDelta { text: BoundedText::new("hello", 64) };
        let mut first = Vec::new();
        renderer.handle(&event, &mut first).unwrap();
        assert!(!first.is_empty());
        let mut second = Vec::new();
        renderer.handle(&event, &mut second).unwrap();
        assert!(second.is_empty());
        renderer.render_if_due(&mut second, true).unwrap();
        assert_eq!(String::from_utf8(second).unwrap(), "hello");
    }

    #[test]
    fn renderer_appends_only_the_new_streaming_suffix() {
        let mut renderer = Renderer::new();
        let first_event = AgentEvent::TextDelta { text: BoundedText::new("hello", 64) };
        let mut output = Vec::new();
        renderer.handle(&first_event, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "hello");

        renderer.apply(&AgentEvent::TextDelta { text: BoundedText::new(" world", 64) });
        let mut suffix = Vec::new();
        renderer.render_if_due(&mut suffix, true).unwrap();
        assert_eq!(String::from_utf8(suffix).unwrap(), " world");
    }

    #[test]
    fn renderer_advances_to_new_event_lines_without_repainting_prior_text() {
        let mut renderer = Renderer::new();
        let mut output = Vec::new();
        renderer
            .handle(&AgentEvent::TextDelta { text: BoundedText::new("answer", 64) }, &mut output)
            .unwrap();

        renderer.apply(&AgentEvent::Warning { message: BoundedText::new("warning", 64) });
        let mut next_line = Vec::new();
        renderer.render_if_due(&mut next_line, true).unwrap();
        assert_eq!(String::from_utf8(next_line).unwrap(), "\n\rwarning");
    }

    #[test]
    fn plain_renderer_writes_only_plain_event_content() {
        let mut renderer = Renderer::new_plain();
        let event = AgentEvent::TextDelta { text: BoundedText::new("hello\x1b[31m\nworld", 64) };
        let mut output = Vec::new();
        renderer.handle(&event, &mut output).unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "hello�[31m\nworld");
    }
}
