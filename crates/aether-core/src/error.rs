use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A typed error that is safe to carry across crate boundaries.
#[derive(Debug, Clone, Error, Serialize, Deserialize, PartialEq, Eq)]
pub enum CoreError {
    /// Input did not satisfy a domain invariant.
    #[error("invalid input for {operation}: {message}")]
    InvalidInput { operation: String, message: String },
    /// A path could not be kept inside the workspace root.
    #[error("path escapes workspace: {path}")]
    PathEscape { path: String },
    /// A bounded operation reached its configured limit.
    #[error("{operation} exceeded limit of {limit} bytes")]
    LimitExceeded { operation: String, limit: usize },
    /// The caller needs to authorize an operation before it can proceed.
    #[error("permission required for {operation}: {class}")]
    PermissionRequired { operation: String, class: String },
    /// A caller supplied a permit that does not authorize the requested operation.
    #[error("permission denied for {operation}: {reason}")]
    PermissionDenied { operation: String, reason: String },
    /// A bounded runtime resource reached its configured capacity.
    #[error("{resource} limit reached ({limit})")]
    ResourceLimit { resource: String, limit: usize },
    /// The operation was cancelled by the user.
    #[error("operation cancelled")]
    Cancelled,
    /// The requested capability is intentionally not part of this bootstrap.
    #[error("unsupported: {operation}")]
    Unsupported { operation: String },
    /// An integration is unavailable without pretending it exists.
    #[error("feature unavailable: {feature}")]
    FeatureUnavailable { feature: String },
    /// A local I/O operation failed without exposing a secret.
    #[error("{operation} failed: {message}")]
    Io { operation: String, message: String },
    /// A serialization boundary failed.
    #[error("serialization failed for {operation}: {message}")]
    Serialization { operation: String, message: String },
}

/// A convenient result alias for domain APIs.
pub type CoreResult<T> = Result<T, CoreError>;

impl CoreError {
    /// Construct a sanitized I/O error from an operation and source display.
    pub fn io(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Io { operation: operation.into(), message: message.into() }
    }

    /// Construct an invalid-input error without allocating a formatted string at the call site.
    pub fn invalid(operation: impl Into<String>, message: impl Into<String>) -> Self {
        Self::InvalidInput { operation: operation.into(), message: message.into() }
    }
}
