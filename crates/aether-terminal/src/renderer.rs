use std::io::{self, Write};
use std::time::{Duration, Instant};

use aether_core::AgentEvent;

use crate::{Style, TerminalBuffer};

const INITIAL_RENDER_INTERVAL: Duration = Duration::from_millis(8);

/// Incremental event renderer with a coalesced 8 ms render budget.
pub struct Renderer {
    buffer: TerminalBuffer,
    last_render: Instant,
    render_interval: Duration,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    /// Construct an empty renderer.
    pub fn new() -> Self {
        Self {
            buffer: TerminalBuffer::new(),
            last_render: Instant::now() - INITIAL_RENDER_INTERVAL,
            render_interval: INITIAL_RENDER_INTERVAL,
        }
    }

    /// Apply one agent event without writing to stdout.
    pub fn apply(&mut self, event: &AgentEvent) {
        match event {
            AgentEvent::TextDelta { text } => self.buffer.append(text.as_str()),
            AgentEvent::ToolStarted { name, .. } => {
                self.buffer.newline();
                self.buffer.append(
                    &Style::new(crate::Intensity::Secondary).paint(&format!("[tool {name}]")),
                );
            }
            AgentEvent::ToolOutput { output, .. } => {
                self.buffer.newline();
                self.buffer.append(output.as_str());
            }
            AgentEvent::ToolFinished { ok, .. } => {
                self.buffer.newline();
                self.buffer.append(if *ok { "[tool done]" } else { "[tool failed]" });
            }
            AgentEvent::PermissionRequested { .. } | AgentEvent::PermissionResolved { .. } => {}
            AgentEvent::Usage { .. } => {}
            AgentEvent::Warning { message } => {
                self.buffer.newline();
                self.buffer.append(message.as_str());
            }
            AgentEvent::Error { message } => {
                self.buffer.newline();
                self.buffer.append(message.as_str());
            }
            AgentEvent::Done => {
                self.buffer.newline();
            }
        }
    }

    /// Apply an event and render only when the coalescing interval has elapsed.
    pub fn handle<W: Write>(&mut self, event: &AgentEvent, writer: &mut W) -> io::Result<()> {
        self.apply(event);
        self.render_if_due(writer, false)
    }

    /// Render dirty lines when due, or all dirty lines when `force` is true.
    pub fn render_if_due<W: Write>(&mut self, writer: &mut W, force: bool) -> io::Result<()> {
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
}
