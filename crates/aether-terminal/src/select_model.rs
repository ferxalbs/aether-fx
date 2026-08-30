use std::io::{self, IsTerminal, Read, Write};

use crate::{SelectorItem, SelectorOutcome, select_from_items, select_interactively};

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

fn shared_items(
    items: &[ModelSelectionItem],
    default_id: Option<&str>,
) -> Vec<SelectorItem<String>> {
    items
        .iter()
        .map(|item| {
            let detail = item.detail.as_deref().map(|detail| {
                if default_id.is_some_and(|default| default == item.id) {
                    format!("{detail} · default")
                } else {
                    detail.to_owned()
                }
            });
            SelectorItem::new(item.id.clone(), item.label.clone(), detail)
                .with_search_text(item.detail.as_deref().unwrap_or_default())
        })
        .collect()
}

/// Run the shared selector loop against raw reader and writer streams.
pub fn select_model_from_items<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    items: &[ModelSelectionItem],
    default_id: Option<&str>,
) -> io::Result<Option<String>> {
    let shared = shared_items(items, default_id);
    let default = default_id
        .and_then(|id| shared.iter().find(|item| item.value == id).map(|item| &item.value));
    match select_from_items(reader, writer, "Select model", &shared, default)? {
        SelectorOutcome::Selected(value) => Ok(Some(value)),
        SelectorOutcome::Cancelled => Ok(None),
        SelectorOutcome::EndOfInput | SelectorOutcome::NoSelection => {
            Ok(default_id.map(str::to_owned))
        }
    }
}

/// Interactively prompt the user to select a model when stdin/stdout are terminals.
pub fn select_model_interactively(
    items: &[ModelSelectionItem],
    default_id: Option<&str>,
) -> io::Result<Option<String>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(default_id.map(str::to_owned));
    }
    let shared = shared_items(items, default_id);
    let default = default_id
        .and_then(|id| shared.iter().find(|item| item.value == id).map(|item| &item.value));
    match select_interactively("Select model", &shared, default)? {
        SelectorOutcome::Selected(value) => Ok(Some(value)),
        SelectorOutcome::Cancelled => Ok(None),
        SelectorOutcome::EndOfInput | SelectorOutcome::NoSelection => {
            Ok(default_id.map(str::to_owned))
        }
    }
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
    fn select_model_escape_cancels() {
        let items = vec![ModelSelectionItem::new("model-a", "Model A", None)];
        let mut input = b"\x1b".as_slice();
        let mut output = Vec::new();
        let selected =
            select_model_from_items(&mut input, &mut output, &items, Some("model-a")).unwrap();
        assert_eq!(selected, None);
    }
}
