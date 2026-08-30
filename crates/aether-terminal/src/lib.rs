#![deny(unsafe_code)]

//! Native terminal substrate with explicit restoration and incremental rendering.

mod buffer;
mod guard;
mod input;
mod platform;
mod renderer;
mod select_model;
mod selector;
mod shell;
mod style;

pub use buffer::{TerminalBuffer, display_width};
pub use guard::{PermissionPromptOutcome, TerminalGuard, prompt_permission, run_minimal_shell};
pub use input::{InputEvent, MAX_INPUT_LINE_BYTES, read_event, read_line_from};
pub use renderer::Renderer;
pub use select_model::{ModelSelectionItem, select_model_from_items, select_model_interactively};
pub use selector::{
    MAX_SELECTOR_ITEMS, MAX_SELECTOR_QUERY_BYTES, MAX_SELECTOR_TEXT_BYTES,
    MAX_SELECTOR_VISIBLE_ITEMS, Selector, SelectorItem, SelectorOutcome, sanitize_terminal_text,
    select_from_items, select_interactively, terminal_width, truncate_display,
};
pub use shell::{
    ShellInput, confirm, confirm_from, read_plain_line, read_plain_line_from, read_secret,
    read_secret_from, read_shell_input, read_shell_input_from, run_shell_input,
};
pub use style::{Intensity, Style};
