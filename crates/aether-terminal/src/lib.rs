#![deny(unsafe_code)]

//! Native terminal substrate with explicit restoration and incremental rendering.

mod buffer;
mod guard;
mod input;
mod platform;
mod renderer;
mod style;

pub use buffer::{TerminalBuffer, display_width};
pub use guard::{TerminalGuard, run_minimal_shell};
pub use input::{InputEvent, read_line_from};
pub use renderer::Renderer;
pub use style::{Intensity, Style};
