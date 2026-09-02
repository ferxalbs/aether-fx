use std::{
    fmt,
    fmt::Write as _,
    io::{self, IsTerminal, Read, Write},
};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::guard::TerminalGuard;
use crate::input::{InputEvent, read_event};

/// Maximum number of entries retained by one selector interaction.
pub const MAX_SELECTOR_ITEMS: usize = 4096;
/// Maximum number of rows painted for a selector.
pub const MAX_SELECTOR_VISIBLE_ITEMS: usize = 8;
/// Maximum query size accepted by a selector.
pub const MAX_SELECTOR_QUERY_BYTES: usize = 256;
/// Maximum bytes retained for one selector label, detail, or search field.
pub const MAX_SELECTOR_TEXT_BYTES: usize = 4096;

/// A bounded, reusable choice presented by a selector or command palette.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectorItem<T> {
    /// Value returned when this item is selected.
    pub value: T,
    /// Primary human-readable label.
    pub label: String,
    /// Optional compact secondary information.
    pub detail: Option<String>,
    /// Disabled items remain visible but cannot be selected.
    pub disabled: bool,
    search_text: String,
}

impl<T> SelectorItem<T> {
    /// Construct an enabled selector item.
    pub fn new(value: T, label: impl Into<String>, detail: Option<String>) -> Self
    where
        T: std::fmt::Display,
    {
        let label = bounded_text(&label.into(), MAX_SELECTOR_TEXT_BYTES);
        let detail = detail.map(|detail| bounded_text(&detail, MAX_SELECTOR_TEXT_BYTES));
        let value_text = bounded_display(&value, MAX_SELECTOR_TEXT_BYTES);
        let search_text = bounded_text(
            &format!("{} {}", value_text, label),
            MAX_SELECTOR_TEXT_BYTES.saturating_mul(2),
        )
        .to_lowercase();
        Self { value, label, detail, disabled: false, search_text }
    }

    /// Mark this item as visible but unavailable.
    #[must_use]
    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Add an extra field to the bounded search surface.
    #[must_use]
    pub fn with_search_text(mut self, text: impl AsRef<str>) -> Self {
        if self.search_text.len() < MAX_SELECTOR_TEXT_BYTES.saturating_mul(2) {
            self.search_text.push(' ');
            self.search_text.push_str(
                &bounded_text(
                    text.as_ref(),
                    MAX_SELECTOR_TEXT_BYTES
                        .saturating_mul(2)
                        .saturating_sub(self.search_text.len()),
                )
                .to_lowercase(),
            );
        }
        self
    }

    pub(crate) fn search_text(&self) -> &str {
        &self.search_text
    }

    pub(crate) fn display_value(&self) -> String
    where
        T: fmt::Display,
    {
        bounded_display(&self.value, MAX_SELECTOR_TEXT_BYTES)
    }
}

/// Outcome of a selector interaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectorOutcome<T> {
    /// An enabled item was confirmed.
    Selected(T),
    /// The user cancelled with Escape, Ctrl-C, or Ctrl-D.
    Cancelled,
    /// The input stream ended before a choice was made.
    EndOfInput,
    /// No item was available to select.
    NoSelection,
}

/// A reusable line-oriented selector. It never switches to an alternate screen buffer.
pub struct Selector<T> {
    title: String,
    items: Vec<SelectorItem<T>>,
    default: Option<T>,
}

impl<T> Selector<T>
where
    T: Clone + PartialEq + std::fmt::Display,
{
    /// Construct a selector with a bounded item list and optional current value.
    pub fn new(
        title: impl Into<String>,
        items: impl IntoIterator<Item = SelectorItem<T>>,
        default: Option<T>,
    ) -> Self {
        Self {
            title: bounded_text(&title.into(), MAX_SELECTOR_TEXT_BYTES),
            items: items.into_iter().take(MAX_SELECTOR_ITEMS).collect(),
            default,
        }
    }

    /// Run the selector against caller-provided streams.
    pub fn run<R: Read, W: Write>(
        &self,
        reader: &mut R,
        writer: &mut W,
    ) -> io::Result<SelectorOutcome<T>> {
        select_from_items(reader, writer, &self.title, &self.items, self.default.as_ref())
    }
}

/// Run one bounded selector against raw input/output streams.
pub fn select_from_items<R: Read, W: Write, T>(
    reader: &mut R,
    writer: &mut W,
    title: &str,
    items: &[SelectorItem<T>],
    default: Option<&T>,
) -> io::Result<SelectorOutcome<T>>
where
    T: Clone + PartialEq + std::fmt::Display,
{
    select_from_items_with_renderer(
        reader,
        writer,
        title,
        items,
        default,
        MAX_SELECTOR_VISIBLE_ITEMS,
        render_lines,
    )
}

/// Run a selector with a caller-provided bounded renderer.
pub(crate) fn select_from_items_with_renderer<R: Read, W: Write, T, F>(
    reader: &mut R,
    writer: &mut W,
    title: &str,
    items: &[SelectorItem<T>],
    default: Option<&T>,
    visible_items: usize,
    render: F,
) -> io::Result<SelectorOutcome<T>>
where
    T: Clone + PartialEq + std::fmt::Display,
    F: Fn(&str, &str, &[SelectorItem<T>], &[usize], usize, bool, usize) -> Vec<String>,
{
    let items = &items[..items.len().min(MAX_SELECTOR_ITEMS)];
    if items.is_empty() {
        return Ok(SelectorOutcome::NoSelection);
    }
    let visible_items = visible_items.max(1);

    let mut query = String::new();
    let mut selected_position = 0_usize;
    if let Some(default) = default
        && let Some(index) = items.iter().position(|item| &item.value == default)
    {
        selected_position = index;
    }
    let mut previous_line_count = 0_usize;

    loop {
        let filtered = filtered_indices(items, &query);
        if filtered.is_empty() {
            selected_position = 0;
        } else if selected_position >= filtered.len() {
            selected_position = filtered.len() - 1;
        }
        let no_selectable_match =
            !filtered.is_empty() && !filtered.iter().any(|index| !items[*index].disabled);
        if !no_selectable_match {
            selected_position = selected_or_next_enabled(items, &filtered, selected_position)
                .unwrap_or(selected_position);
        }

        clear_previous_lines(writer, previous_line_count)?;
        let lines = render(
            title,
            &query,
            items,
            &filtered,
            selected_position,
            no_selectable_match,
            visible_items,
        );
        previous_line_count = lines.len();
        for (index, line) in lines.iter().enumerate() {
            writer.write_all(line.as_bytes())?;
            if index + 1 < lines.len() {
                writer.write_all(b"\n\r")?;
            }
        }
        writer.flush()?;

        let Some(event) = read_event(reader)? else {
            return Ok(SelectorOutcome::EndOfInput);
        };
        match event {
            InputEvent::Up => {
                if let Some(position) = nearest_enabled(items, &filtered, selected_position, -1) {
                    selected_position = position;
                }
            }
            InputEvent::Down => {
                if let Some(position) = nearest_enabled(items, &filtered, selected_position, 1) {
                    selected_position = position;
                }
            }
            InputEvent::Enter => {
                if let Some(item) = filtered
                    .get(selected_position)
                    .and_then(|index| items.get(*index))
                    .filter(|item| !item.disabled)
                {
                    finish_selector(writer)?;
                    return Ok(SelectorOutcome::Selected(item.value.clone()));
                }
                if filtered.is_empty() {
                    finish_selector(writer)?;
                    return Ok(SelectorOutcome::NoSelection);
                }
                if no_selectable_match {
                    finish_selector(writer)?;
                    return Ok(SelectorOutcome::NoSelection);
                }
            }
            InputEvent::Tab => {
                if let Some(index) = filtered.iter().copied().find(|index| !items[*index].disabled)
                    && filtered.iter().filter(|candidate| !items[**candidate].disabled).count() == 1
                {
                    finish_selector(writer)?;
                    return Ok(SelectorOutcome::Selected(items[index].value.clone()));
                }
            }
            InputEvent::Backspace => {
                if let Some((index, _)) = query.grapheme_indices(true).next_back() {
                    query.truncate(index);
                    selected_position = 0;
                }
            }
            InputEvent::Character(character) => {
                if query.is_empty() && matches!(character, 'j' | 'J') {
                    if let Some(position) = nearest_enabled(items, &filtered, selected_position, 1)
                    {
                        selected_position = position;
                    }
                } else if query.is_empty() && matches!(character, 'k' | 'K') {
                    if let Some(position) = nearest_enabled(items, &filtered, selected_position, -1)
                    {
                        selected_position = position;
                    }
                } else if query.is_empty()
                    && character.is_ascii_digit()
                    && character != '0'
                    && let Some(position) = (character as usize)
                        .checked_sub('1' as usize)
                        .and_then(|position| {
                            visible_window(selected_position, filtered.len(), visible_items)
                                .0
                                .checked_add(position)
                                .and_then(|position| filtered.get(position).copied())
                        })
                        .filter(|index| {
                            let start =
                                visible_window(selected_position, filtered.len(), visible_items).0;
                            filtered.iter().position(|candidate| candidate == index).is_some_and(
                                |position| position >= start && position < start + visible_items,
                            )
                        })
                        .filter(|index| !items[*index].disabled)
                {
                    finish_selector(writer)?;
                    return Ok(SelectorOutcome::Selected(items[position].value.clone()));
                } else if !character.is_control()
                    && query.len().saturating_add(character.len_utf8()) <= MAX_SELECTOR_QUERY_BYTES
                {
                    query.push(character);
                    selected_position = 0;
                }
            }
            InputEvent::CtrlC | InputEvent::CtrlD | InputEvent::Escape => {
                finish_selector(writer)?;
                return Ok(SelectorOutcome::Cancelled);
            }
        }
    }
}

/// Run a selector when both standard streams are terminals.
pub fn select_interactively<T>(
    title: &str,
    items: &[SelectorItem<T>],
    default: Option<&T>,
) -> io::Result<SelectorOutcome<T>>
where
    T: Clone + PartialEq + std::fmt::Display,
{
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(SelectorOutcome::EndOfInput);
    }
    let mut guard = TerminalGuard::enter()?;
    let mut stdout = io::stdout().lock();
    let mut stdin = io::stdin().lock();
    let result = select_from_items(&mut stdin, &mut stdout, title, items, default);
    guard.restore()?;
    result
}

/// Return terminal-safe text with controls replaced by a visible replacement marker.
pub fn sanitize_terminal_text(text: &str) -> String {
    text.chars().map(|character| if character.is_control() { '�' } else { character }).collect()
}

/// Truncate terminal text by display cells without splitting UTF-8 or grapheme clusters.
pub fn truncate_display(text: &str, width: usize) -> String {
    let text = sanitize_terminal_text(text);
    if UnicodeWidthStr::width(text.as_str()) <= width {
        return text;
    }
    if width == 0 {
        return String::new();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut output = String::new();
    let mut used = 0_usize;
    for grapheme in text.graphemes(true) {
        let cells = UnicodeWidthStr::width(grapheme);
        if used.saturating_add(cells) > width - 1 {
            break;
        }
        output.push_str(grapheme);
        used = used.saturating_add(cells);
    }
    output.push('…');
    output
}

/// Read a bounded terminal width, falling back to a normal 80-column terminal.
pub fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(80)
        .clamp(20, 240)
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    for character in text.chars() {
        let character = if character.is_control() { '�' } else { character };
        if output.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        output.push(character);
    }
    output
}

fn bounded_display<T: fmt::Display>(value: &T, max_bytes: usize) -> String {
    struct BoundedWriter {
        output: String,
        max_bytes: usize,
    }

    impl fmt::Write for BoundedWriter {
        fn write_str(&mut self, value: &str) -> fmt::Result {
            for character in value.chars() {
                let character = if character.is_control() { '�' } else { character };
                if self.output.len().saturating_add(character.len_utf8()) > self.max_bytes {
                    break;
                }
                self.output.push(character);
            }
            Ok(())
        }
    }

    let mut writer = BoundedWriter { output: String::new(), max_bytes };
    let _ = write!(&mut writer, "{value}");
    writer.output
}

fn filtered_indices<T>(items: &[SelectorItem<T>], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    let mut matches = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            match_score(&query, item.search_text()).map(|score| (score, index))
        })
        .collect::<Vec<_>>();
    matches.sort_unstable_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    matches.into_iter().map(|(_, index)| index).collect()
}

fn match_score(query: &str, candidate: &str) -> Option<(u8, usize)> {
    if query.is_empty() {
        return Some((0, 0));
    }
    if candidate == query {
        return Some((0, 0));
    }
    if candidate.starts_with(query) {
        return Some((1, 0));
    }
    if let Some(position) = candidate.find(query) {
        return Some((2, position));
    }
    let mut query_chars = query.chars();
    let mut wanted = query_chars.next()?;
    for (position, candidate) in candidate.chars().enumerate() {
        if candidate == wanted {
            if let Some(next) = query_chars.next() {
                wanted = next;
            } else {
                return Some((3, position));
            }
        }
    }
    None
}

fn nearest_enabled<T>(
    items: &[SelectorItem<T>],
    filtered: &[usize],
    position: usize,
    direction: i8,
) -> Option<usize> {
    if filtered.is_empty() {
        return None;
    }
    let mut cursor = position as isize;
    loop {
        cursor += isize::from(direction);
        if cursor < 0 || cursor >= filtered.len() as isize {
            return None;
        }
        let candidate = filtered[cursor as usize];
        if !items[candidate].disabled {
            return Some(cursor as usize);
        }
    }
}

fn selected_or_next_enabled<T>(
    items: &[SelectorItem<T>],
    filtered: &[usize],
    position: usize,
) -> Option<usize> {
    if filtered.get(position).is_some_and(|index| !items[*index].disabled) {
        Some(position)
    } else {
        nearest_enabled(items, filtered, position, 1)
            .or_else(|| nearest_enabled(items, filtered, position, -1))
    }
}

pub(crate) fn visible_window(
    selected_position: usize,
    item_count: usize,
    visible_items: usize,
) -> (usize, usize) {
    let visible_items = visible_items.max(1);
    if item_count <= visible_items {
        return (0, item_count);
    }
    let max_start = item_count - visible_items;
    let start = selected_position.saturating_sub(visible_items / 2).min(max_start);
    (start, (start + visible_items).min(item_count))
}

fn render_lines<T>(
    title: &str,
    query: &str,
    items: &[SelectorItem<T>],
    filtered: &[usize],
    selected_position: usize,
    no_selectable_match: bool,
    visible_items: usize,
) -> Vec<String>
where
    T: std::fmt::Display,
{
    let width = terminal_width();
    let mut lines = Vec::with_capacity(MAX_SELECTOR_VISIBLE_ITEMS + 4);
    lines.push(truncate_display(title, width));
    lines.push(truncate_display(&format!("› {query}"), width));
    if filtered.is_empty() {
        lines.push("  (no matches)".to_owned());
    } else {
        let (visible_start, visible_end) =
            visible_window(selected_position, filtered.len(), visible_items);
        if visible_start > 0 {
            lines.push(format!("  … {} earlier", visible_start));
        }
        for (position, index) in
            filtered.iter().enumerate().skip(visible_start).take(visible_end - visible_start)
        {
            let item = &items[*index];
            let pointer = if position == selected_position { "›" } else { " " };
            let disabled = if item.disabled { " unavailable" } else { "" };
            let detail = item
                .detail
                .as_deref()
                .map(|detail| format!("  {}", sanitize_terminal_text(detail)))
                .unwrap_or_default();
            let prefix = format!("{pointer} ");
            let label_width =
                width.saturating_sub(display_width(&prefix)).saturating_sub(display_width(&detail));
            let label = truncate_display(&item.label, label_width.max(1));
            lines.push(truncate_display(&format!("{prefix}{label}{detail}{disabled}"), width));
        }
        if visible_end < filtered.len() {
            lines.push(format!("  … {} more", filtered.len() - visible_end));
        }
    }
    if no_selectable_match {
        lines.push("  unavailable entries cannot be selected".to_owned());
    } else {
        lines.push("↑↓/j/k navigate · type to filter · enter select · esc cancel".to_owned());
    }
    lines
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn clear_previous_lines<W: Write>(writer: &mut W, count: usize) -> io::Result<()> {
    for _ in 0..count {
        writer.write_all(b"\x1b[2K\x1b[1A")?;
    }
    if count > 0 {
        writer.write_all(b"\x1b[2K\r")?;
    }
    Ok(())
}

pub(crate) fn clear_line<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(b"\r\x1b[2K")
}

fn finish_selector<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(b"\n\r")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items() -> Vec<SelectorItem<String>> {
        vec![
            SelectorItem::new("model-a".to_owned(), "Model A", Some("Tools".to_owned())),
            SelectorItem::new("model-b".to_owned(), "Model B", Some("Reasoning".to_owned())),
        ]
    }

    #[test]
    fn selector_filters_and_confirms() {
        let mut input = b"model-b\r".as_slice();
        let mut output = Vec::new();
        let outcome =
            select_from_items(&mut input, &mut output, "Select model", &items(), None).unwrap();
        assert_eq!(outcome, SelectorOutcome::Selected("model-b".to_owned()));
    }

    #[test]
    fn selector_filter_prefers_contiguous_model_match() {
        let items = vec![
            SelectorItem::new("claude-3-7-sonnet".to_owned(), "Claude 3.7", None),
            SelectorItem::new("o3-mini".to_owned(), "o3-mini", None),
        ];
        assert_eq!(filtered_indices(&items, "o3").first(), Some(&1));
    }

    #[test]
    fn selector_escape_cancels_without_returning_a_value() {
        let mut input = b"\x1b".as_slice();
        let mut output = Vec::new();
        let outcome = select_from_items(&mut input, &mut output, "Select", &items(), None).unwrap();
        assert_eq!(outcome, SelectorOutcome::Cancelled);
    }

    #[test]
    fn selector_keeps_controls_out_of_rendered_text() {
        assert_eq!(sanitize_terminal_text("bad\x1b[31mname\n"), "bad�[31mname�");
        assert_eq!(truncate_display("abcdef", 4), "abc…");
    }

    #[test]
    fn disabled_items_are_visible_but_not_selectable() {
        let items = vec![SelectorItem::new("blocked", "Blocked", None).disabled(true)];
        let mut input = b"\r".as_slice();
        let mut output = Vec::new();
        let outcome = select_from_items(&mut input, &mut output, "Select", &items, None).unwrap();
        assert_eq!(outcome, SelectorOutcome::NoSelection);
        assert!(String::from_utf8(output).unwrap().contains("unavailable"));
    }

    #[test]
    fn selector_filters_a_thousand_items_deterministically() {
        let items = (0..1000)
            .map(|index| {
                SelectorItem::new(
                    format!("model-{index:04}"),
                    format!("Model {index:04}"),
                    Some("tools · text".to_owned()),
                )
            })
            .collect::<Vec<_>>();
        let mut input = b"model-0999\r".as_slice();
        let mut output = Vec::new();
        let outcome = select_from_items(&mut input, &mut output, "Models", &items, None).unwrap();
        assert_eq!(outcome, SelectorOutcome::Selected("model-0999".to_owned()));
        assert!(output.len() < 64 * 1024);
    }

    #[test]
    fn selector_bounds_untrusted_labels_before_filtering_and_rendering() {
        let items = vec![SelectorItem::new(
            "bounded".to_owned(),
            "x".repeat(MAX_SELECTOR_TEXT_BYTES * 4),
            Some("detail".repeat(MAX_SELECTOR_TEXT_BYTES)),
        )];
        let mut input = b"\r".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            select_from_items(&mut input, &mut output, "Select", &items, None).unwrap(),
            SelectorOutcome::Selected("bounded".to_owned())
        );
        assert!(output.len() < 64 * 1024);
    }

    #[test]
    fn selector_truncates_by_terminal_cells_without_splitting_graphemes() {
        assert_eq!(truncate_display("界界界", 5), "界界…");
        assert_eq!(truncate_display("e\u{301}clair", 4), "e\u{301}cl…");
    }

    #[test]
    fn selector_keeps_current_selection_visible_and_skips_disabled_default() {
        let items = (0..20)
            .map(|index| {
                SelectorItem::new(format!("model-{index}"), format!("Model {index}"), None)
            })
            .collect::<Vec<_>>();
        let mut input = b"\r".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            select_from_items(&mut input, &mut output, "Models", &items, Some(&items[19].value))
                .unwrap(),
            SelectorOutcome::Selected("model-19".to_owned())
        );
        assert!(String::from_utf8(output).unwrap().contains("Model 19"));

        let disabled = items
            .into_iter()
            .enumerate()
            .map(|(index, item)| item.disabled(index == 19))
            .collect::<Vec<_>>();
        let mut input = b"\r".as_slice();
        let mut output = Vec::new();
        assert_eq!(
            select_from_items(
                &mut input,
                &mut output,
                "Models",
                &disabled,
                Some(&disabled[19].value)
            )
            .unwrap(),
            SelectorOutcome::Selected("model-18".to_owned())
        );
    }
}
