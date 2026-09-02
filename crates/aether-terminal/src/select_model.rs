use std::io::{self, IsTerminal, Read, Write};

use crate::{
    Intensity, Style,
    guard::TerminalGuard,
    selector::{SelectorItem, SelectorOutcome, select_from_items_with_renderer, visible_window},
    truncate_display,
};

const MAX_MODEL_SELECTOR_VISIBLE_ITEMS: usize = 5;

/// One model choice presented in the interactive model selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSelectionItem {
    /// Stable provider model identifier used for inference.
    pub id: String,
    /// Human-readable model name shown first in the picker.
    pub label: String,
    /// Compact capability information shown below the model identity.
    pub detail: Option<String>,
    /// Whether this model is visible but cannot be selected.
    pub unavailable: bool,
}

impl ModelSelectionItem {
    pub fn new(id: impl Into<String>, label: impl Into<String>, detail: Option<String>) -> Self {
        Self { id: id.into(), label: label.into(), detail, unavailable: false }
    }

    /// Mark this model as visible but unavailable for the current session.
    #[must_use]
    pub fn unavailable(mut self, unavailable: bool) -> Self {
        self.unavailable = unavailable;
        self
    }
}

fn shared_items(items: &[ModelSelectionItem]) -> Vec<SelectorItem<String>> {
    items
        .iter()
        .map(|item| {
            SelectorItem::new(item.id.clone(), item.label.clone(), item.detail.clone())
                .disabled(item.unavailable)
                .with_search_text(item.detail.as_deref().unwrap_or_default())
        })
        .collect()
}

fn select_model_from_shared<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    items: &[SelectorItem<String>],
    default_id: Option<&str>,
) -> io::Result<Option<String>> {
    let default = default_id
        .and_then(|id| items.iter().find(|item| item.value == id).map(|item| &item.value));
    let current_id = default_id.map(str::to_owned);
    let outcome = select_from_items_with_renderer(
        reader,
        writer,
        "Choose a model",
        items,
        default,
        MAX_MODEL_SELECTOR_VISIBLE_ITEMS,
        move |title, query, items, filtered, selected_position, no_selectable_match, _| {
            render_model_lines(
                title,
                query,
                items,
                filtered,
                selected_position,
                no_selectable_match,
                current_id.as_deref(),
            )
        },
    )?;
    match outcome {
        SelectorOutcome::Selected(value) => Ok(Some(value)),
        SelectorOutcome::Cancelled => Ok(None),
        SelectorOutcome::EndOfInput | SelectorOutcome::NoSelection => {
            Ok(default_id.map(str::to_owned))
        }
    }
}

/// Run the model selector against caller-provided reader and writer streams.
pub fn select_model_from_items<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    items: &[ModelSelectionItem],
    default_id: Option<&str>,
) -> io::Result<Option<String>> {
    let shared = shared_items(items);
    select_model_from_shared(reader, writer, &shared, default_id)
}

/// Interactively prompt the user to select a model when stdin/stdout are terminals.
pub fn select_model_interactively(
    items: &[ModelSelectionItem],
    default_id: Option<&str>,
) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(default_id.map(str::to_owned));
    }
    let shared = shared_items(items);
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    let mut stdin = io::stdin().lock();
    let result = select_model_from_shared(&mut stdin, &mut stdout, &shared, default_id);
    guard.restore()?;
    result
}

fn render_model_lines<T>(
    title: &str,
    query: &str,
    items: &[SelectorItem<T>],
    filtered: &[usize],
    selected_position: usize,
    no_selectable_match: bool,
    current_id: Option<&str>,
) -> Vec<String>
where
    T: std::fmt::Display,
{
    let width = crate::terminal_width();
    let mut lines =
        Vec::with_capacity(MAX_MODEL_SELECTOR_VISIBLE_ITEMS.saturating_mul(2).saturating_add(9));
    let title = truncate_display(title, width);
    lines.push(Style::new(Intensity::Primary).paint(&title));
    lines.push(truncate_display(
        "  Choose what handles your next turn. Search by model name or ID.",
        width,
    ));

    let current = current_id.map(|id| {
        items
            .iter()
            .find(|item| item.display_value() == id)
            .map(|item| format!("  Current: {}  ({})", item.label, item.display_value()))
            .unwrap_or_else(|| format!("  Current: {id}  (not in catalog)"))
    });
    lines.push(truncate_display(current.as_deref().unwrap_or("  Current: none selected"), width));
    lines.push(format!("  {}", "─".repeat(width.saturating_sub(2))));

    let filter = if query.is_empty() { "type to filter by name or ID" } else { query };
    lines.push(truncate_display(&format!("  Filter: {filter}"), width));

    if filtered.is_empty() {
        lines.push(truncate_display(
            if query.is_empty() {
                "  No models are available."
            } else {
                "  No models match. Press backspace to clear the filter."
            },
            width,
        ));
    } else {
        let (visible_start, visible_end) =
            visible_window(selected_position, filtered.len(), MAX_MODEL_SELECTOR_VISIBLE_ITEMS);
        if visible_start > 0 {
            lines.push(truncate_display(&format!("  … {visible_start} earlier"), width));
        }
        for (position, index) in
            filtered.iter().enumerate().skip(visible_start).take(visible_end - visible_start)
        {
            let item = &items[*index];
            let selected = position == selected_position;
            let current = current_id.is_some_and(|id| item.display_value() == id);
            let status = if current {
                "  current"
            } else if item.disabled {
                "  unavailable"
            } else {
                ""
            };
            let prefix = format!("{} {:>2}. ", if selected { "›" } else { " " }, position + 1);
            let name_width = width
                .saturating_sub(display_width(&prefix))
                .saturating_sub(display_width(status))
                .max(1);
            let name = truncate_display(&item.label, name_width);
            let name = Style::new(if selected { Intensity::Primary } else { Intensity::Normal })
                .paint(&name);
            lines.push(format!("{prefix}{name}{status}"));

            let mut identity = item.display_value();
            if let Some(detail) = item.detail.as_deref().filter(|detail| !detail.is_empty()) {
                identity.push_str(" · ");
                identity.push_str(detail);
            }
            let identity = format!("    {identity}");
            let identity = truncate_display(&identity, width);
            let intensity =
                if item.disabled { Intensity::Structural } else { Intensity::Secondary };
            lines.push(Style::new(intensity).paint(&identity));
        }
        if visible_end < filtered.len() {
            lines.push(truncate_display(
                &format!("  … {} more", filtered.len() - visible_end),
                width,
            ));
        }
    }

    if no_selectable_match {
        lines.push(truncate_display("  No selectable model matches this filter.", width));
    } else {
        lines.push(truncate_display(
            &format!(
                "  ↑↓/j/k move · type filter · 1-{} quick select · enter apply · esc cancel",
                MAX_MODEL_SELECTOR_VISIBLE_ITEMS.min(9)
            ),
            width,
        ));
    }
    lines
}

fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
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
        let mut input = b"o3\r".as_slice();
        let mut output = Vec::new();
        let selected = select_model_from_items(&mut input, &mut output, &items, None).unwrap();
        assert_eq!(selected.as_deref(), Some("o3-mini"));
    }

    #[test]
    fn select_model_supports_visible_number_quick_select() {
        let items = vec![
            ModelSelectionItem::new("model-a", "Model A", None),
            ModelSelectionItem::new("model-b", "Model B", None),
        ];
        let mut input = b"2".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            select_model_from_items(&mut input, &mut output, &items, None).unwrap(),
            Some("model-b".to_owned())
        );
    }

    #[test]
    fn select_model_prioritizes_name_then_id_and_capabilities() {
        let items = vec![ModelSelectionItem::new(
            "provider/frontier",
            "Frontier Pro",
            Some("128k context · tools supported · reasoning supported".to_owned()),
        )];
        let mut input = b"\r".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            select_model_from_items(&mut input, &mut output, &items, None).unwrap(),
            Some("provider/frontier".to_owned())
        );
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Frontier Pro"), "{rendered}");
        assert!(rendered.contains("provider/frontier"), "{rendered}");
        assert!(rendered.contains("tools supported"), "{rendered}");
        assert!(!rendered.contains(" · tier"), "{rendered}");
    }

    #[test]
    fn select_model_marks_current_and_keeps_unavailable_visible() {
        let items = vec![
            ModelSelectionItem::new("current", "Current Model", Some("1M context".to_owned())),
            ModelSelectionItem::new(
                "blocked",
                "Blocked Model",
                Some("tools unsupported".to_owned()),
            )
            .unavailable(true),
        ];
        let mut input = b"\r".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            select_model_from_items(&mut input, &mut output, &items, Some("current")).unwrap(),
            Some("current".to_owned())
        );
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("Current: Current Model  (current)"), "{rendered}");
        assert!(rendered.contains("Blocked Model"), "{rendered}");
        assert!(rendered.contains("unavailable"), "{rendered}");
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
