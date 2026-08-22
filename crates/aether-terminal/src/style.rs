use std::env;

/// Monochrome intensity levels. No RGB/color escape is ever emitted.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Intensity {
    /// Full white/bold primary text.
    Primary,
    /// Normal intensity.
    Normal,
    /// Dim secondary text.
    Secondary,
    /// Dim structural text.
    Structural,
}

/// A cosmetic monochrome style that respects `NO_COLOR`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Style {
    intensity: Intensity,
}

impl Style {
    /// Construct a style at a chosen intensity.
    pub const fn new(intensity: Intensity) -> Self {
        Self { intensity }
    }

    /// Wrap text with monochrome intensity controls unless `NO_COLOR` is set.
    pub fn paint(self, text: &str) -> String {
        if env::var_os("NO_COLOR").is_some() {
            return text.to_owned();
        }
        let code = match self.intensity {
            Intensity::Primary => "1",
            Intensity::Normal => "22",
            Intensity::Secondary | Intensity::Structural => "2",
        };
        format!("\x1b[{code}m{text}\x1b[0m")
    }
}
