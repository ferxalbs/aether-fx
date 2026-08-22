use std::fmt;

use serde::{Deserialize, Serialize};

/// Default output ceiling used by model-visible operations.
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;

/// Metadata describing how a value was bounded.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OutputLimit {
    /// Whether bytes were removed from the original value.
    pub truncated: bool,
    /// Original byte count before bounding.
    pub original_bytes: usize,
    /// Retained byte count after bounding.
    pub retained_bytes: usize,
}

/// A UTF-8 string with an explicit byte ceiling.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct BoundedText {
    text: String,
    limit: OutputLimit,
}

impl BoundedText {
    /// Bound `value` to at most `max_bytes`, preserving valid UTF-8 boundaries.
    pub fn new(value: impl AsRef<str>, max_bytes: usize) -> Self {
        let value = value.as_ref();
        let original_bytes = value.len();
        let mut end = original_bytes.min(max_bytes);
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        let truncated = end < original_bytes;
        Self {
            text: value[..end].to_owned(),
            limit: OutputLimit { truncated, original_bytes, retained_bytes: end },
        }
    }

    /// Return the bounded text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// Consume the wrapper and return bounded text.
    pub fn into_string(self) -> String {
        self.text
    }

    /// Return truncation metadata.
    pub fn limit(&self) -> OutputLimit {
        self.limit
    }

    /// Whether the original value exceeded the ceiling.
    pub fn is_truncated(&self) -> bool {
        self.limit.truncated
    }
}

impl fmt::Display for BoundedText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for BoundedText {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_preserves_utf8() {
        let bounded = BoundedText::new("aéz", 2);
        assert_eq!(bounded.as_str(), "a");
        assert!(bounded.is_truncated());
        assert_eq!(bounded.limit().original_bytes, 4);
    }
}
