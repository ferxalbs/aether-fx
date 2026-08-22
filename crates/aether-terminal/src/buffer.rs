use std::collections::BTreeSet;

use unicode_width::UnicodeWidthStr;

/// Return terminal cell width for a Unicode string.
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// A small line buffer that tracks dirty lines instead of repainting the screen.
#[derive(Clone, Debug, Default)]
pub struct TerminalBuffer {
    lines: Vec<String>,
    dirty: BTreeSet<usize>,
}

impl TerminalBuffer {
    /// Construct an empty buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append text to the current last line.
    pub fn append(&mut self, text: &str) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        let index = self.lines.len() - 1;
        self.lines[index].push_str(text);
        self.dirty.insert(index);
    }

    /// Finish the current line and start a new one.
    pub fn newline(&mut self) {
        self.lines.push(String::new());
        self.dirty.insert(self.lines.len() - 1);
    }

    /// Return all currently buffered lines.
    pub fn lines(&self) -> &[String] {
        &self.lines
    }

    /// Return and clear the set of dirty line indices.
    pub fn take_dirty(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.dirty).into_iter().collect()
    }

    /// Return the displayed width of one line.
    pub fn line_width(&self, index: usize) -> Option<usize> {
        self.lines.get(index).map(|line| display_width(line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_uses_terminal_cells() {
        assert_eq!(display_width("a界"), 3);
    }
}
