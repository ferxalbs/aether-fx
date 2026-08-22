use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreResult};

macro_rules! id_type {
    ($name:ident, $label:literal) => {
        /// A stable semantic identifier used by the runtime.
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Create an identifier after checking the bounded identifier grammar.
            pub fn new(value: impl Into<String>) -> CoreResult<Self> {
                let value = value.into();
                if value.is_empty() || value.len() > 128 {
                    return Err(CoreError::invalid(
                        $label,
                        "identifier must contain 1..=128 bytes",
                    ));
                }
                if value.bytes().any(|byte| {
                    !(byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
                }) {
                    return Err(CoreError::invalid(
                        $label,
                        "identifier may contain only ASCII letters, digits, '-', '_', and '.'",
                    ));
                }
                Ok(Self(value))
            }

            /// Return the identifier as a string slice.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// Consume the wrapper and return the owned identifier.
            pub fn into_string(self) -> String {
                self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }
    };
}

id_type!(SessionId, "session id");
id_type!(TurnId, "turn id");
id_type!(StepId, "step id");
id_type!(ToolCallId, "tool call id");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_reject_unsafe_values() {
        assert!(SessionId::new("session/one").is_err());
        assert!(SessionId::new("").is_err());
        assert_eq!(SessionId::new("session-1").unwrap().to_string(), "session-1");
    }
}
