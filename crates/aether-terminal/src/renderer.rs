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
                self.buffer.newline();
                let label = format!("[tool {}]", sanitize_terminal_text(name));
                if self.interactive {
                    self.buffer.append(&Style::new(crate::Intensity::Secondary).paint(&label));
                } else {
                    self.buffer.append(&label);
                }
            }
            AgentEvent::ToolOutput { output, .. } => {
                self.buffer.newline();
                append_safe(&mut self.buffer, output.as_str());
            }
            AgentEvent::ToolFinished { ok, .. } => {
                self.buffer.newline();
                self.buffer.append(if *ok { "[tool done]" } else { "[tool failed]" });
            }
            AgentEvent::PermissionRequested { .. } | AgentEvent::PermissionResolved { .. } => {}
            AgentEvent::Usage { .. } => {}
            AgentEvent::Warning { message } => {
                self.buffer.newline();
                append_safe(&mut self.buffer, message.as_str());
            }
            AgentEvent::Error { message } => {
                self.buffer.newline();
                append_safe(&mut self.buffer, message.as_str());
            }
            AgentEvent::Done => {
                self.buffer.newline();
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
        let dirty = self.buffer.take_dirty();
        for index in dirty {
            if let Some(line) = self.buffer.lines().get(index) {
                writer.write_all(b"\r\x1b[2K")?;
                writer.write_all(line.as_bytes())?;
                if index + 1 < self.buffer.lines().len() {
                    writer.write_all(b"\n")?;
                }
            }
        }
        writer.flush()?;
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
        assert!(!second.is_empty());
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
