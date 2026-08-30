use std::io::{self, IsTerminal, Read, Write};

use crate::guard::TerminalGuard;
use crate::input::{InputEvent, read_event};

/// One model choice presented in the interactive model selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelectionItem {
    pub id: String,
    pub label: String,
    pub detail: Option<String>,
}

impl ModelSelectionItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>, detail: Option<String>) -> Self {
        Self { id: id.into(), label: label.into(), detail }
    }
}

/// Run the interactive model selection loop against raw reader and writer streams.
pub fn select_model_from_items<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    items: &[ModelSelectionItem],
    default_id: Option<&str>,
) -> io::Result<Option<String>> {
    if items.is_empty() {
        return Ok(default_id.map(str::to_owned));
    }

    let mut query = String::new();
    let mut selected_index =
        default_id.and_then(|def| items.iter().position(|item| item.id == def)).unwrap_or(0);
    let mut previous_line_count = 0;

    loop {
        let filtered = items
            .iter()
            .filter(|item| {
                if query.is_empty() {
                    true
                } else {
                    let q = query.to_ascii_lowercase();
                    item.id.to_ascii_lowercase().contains(&q)
                        || item.label.to_ascii_lowercase().contains(&q)
                        || item
                            .detail
                            .as_deref()
                            .is_some_and(|d| d.to_ascii_lowercase().contains(&q))
                }
            })
            .collect::<Vec<_>>();

        if selected_index >= filtered.len() {
            selected_index = filtered.len().saturating_sub(1);
        }

        if previous_line_count > 0 {
            for _ in 0..previous_line_count {
                writer.write_all(b"\x1b[2K\x1b[1A")?;
            }
            writer.write_all(b"\x1b[2K\r")?;
        }

        let mut lines = Vec::new();
        lines.push(
            "Select model (↑/↓ or j/k to navigate, type to filter, Enter to select, Esc to cancel):"
                .to_owned(),
        );
        lines.push(format!("› {query}"));

        if filtered.is_empty() {
            lines.push("  (no matching models)".to_owned());
        } else {
            for (idx, item) in filtered.iter().enumerate().take(8) {
                let pointer = if idx == selected_index { ">" } else { " " };
                let is_default = default_id.is_some_and(|def| def == item.id);
                let default_tag = if is_default { " [default]" } else { "" };
                let detail_str =
                    item.detail.as_deref().map(|d| format!(" ({d})")).unwrap_or_default();
                lines.push(format!(
                    "{pointer} [{}] {}{detail_str}{default_tag}",
                    idx + 1,
                    item.label
                ));
            }
            if filtered.len() > 8 {
                lines.push(format!("  ... and {} more", filtered.len() - 8));
            }
        }

        previous_line_count = lines.len();
        for (i, line) in lines.iter().enumerate() {
            writer.write_all(line.as_bytes())?;
            if i + 1 < lines.len() {
                writer.write_all(b"\n\r")?;
            }
        }
        writer.flush()?;

        let Some(event) = read_event(reader)? else {
            return Ok(default_id.map(str::to_owned));
        };

        match event {
            InputEvent::Up => {
                selected_index = selected_index.saturating_sub(1);
            }
            InputEvent::Down => {
                if !filtered.is_empty() {
                    selected_index = (selected_index + 1).min(filtered.len() - 1);
                }
            }
            InputEvent::Enter => {
                let selected = filtered
                    .get(selected_index)
                    .map(|item| item.id.clone())
                    .or_else(|| default_id.map(str::to_owned));
                writer.write_all(b"\n\r")?;
                writer.flush()?;
                return Ok(selected);
            }
            InputEvent::Backspace => {
                query.pop();
                selected_index = 0;
            }
            InputEvent::Character(c) => {
                if query.is_empty() && (c == 'k' || c == 'K') {
                    selected_index = selected_index.saturating_sub(1);
                } else if query.is_empty() && (c == 'j' || c == 'J') {
                    if !filtered.is_empty() {
                        selected_index = (selected_index + 1).min(filtered.len() - 1);
                    }
                } else if query.is_empty() && c.is_ascii_digit() && c != '0' {
                    let num = (c as usize) - ('0' as usize);
                    if num <= filtered.len() {
                        selected_index = num - 1;
                        let selected = filtered.get(selected_index).map(|item| item.id.clone());
                        writer.write_all(b"\n\r")?;
                        writer.flush()?;
                        return Ok(selected);
                    }
                } else if !c.is_control() {
                    query.push(c);
                    selected_index = 0;
                }
            }
            InputEvent::CtrlC | InputEvent::CtrlD | InputEvent::Escape => {
                writer.write_all(b"\n\r")?;
                writer.flush()?;
                return Ok(None);
            }
        }
    }
}

/// Interactively prompt the user to select a model when stdin/stdout are interactive.
pub fn select_model_interactively(
    items: &[ModelSelectionItem],
    default_id: Option<&str>,
) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(default_id.map(str::to_owned));
    }
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    let result = select_model_from_items(&mut io::stdin().lock(), &mut stdout, items, default_id)?;
    guard.restore()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_model_navigates_and_confirms() {
        let items = vec![
            ModelSelectionItem::new("model-a", "Model A", Some("fast".to_owned())),
            ModelSelectionItem::new("model-b", "Model B", Some("accurate".to_owned())),
        ];
        // Down arrow then Enter selects model-b
        let mut input = b"\x1b[B\r".as_slice();
        let mut output = Vec::new();
        let selected =
            select_model_from_items(&mut input, &mut output, &items, Some("model-a")).unwrap();
        assert_eq!(selected.as_deref(), Some("model-b"));
    }

    #[test]
    fn select_model_filters_query() {
        let items = vec![
            ModelSelectionItem::new("claude-3-7-sonnet", "Claude 3.7", None),
            ModelSelectionItem::new("o3-mini", "o3-mini", None),
        ];
        // Type 'o3' then Enter selects o3-mini
        let mut input = b"o3\r".as_slice();
        let mut output = Vec::new();
        let selected = select_model_from_items(&mut input, &mut output, &items, None).unwrap();
        assert_eq!(selected.as_deref(), Some("o3-mini"));
    }

    #[test]
    fn select_model_escape_cancels() {
        let items = vec![ModelSelectionItem::new("model-a", "Model A", None)];
        let mut input = b"\x1b".as_slice();
        let mut output = Vec::new();
        let selected =
            select_model_from_items(&mut input, &mut output, &items, Some("model-a")).unwrap();
        assert_eq!(selected, None);
    }
}
