use std::future::Future;
use std::pin::Pin;

use aether_core::{ModelEvent, ModelRequest};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A bounded receiver for backend stream events.
pub type BackendStream = tokio::sync::mpsc::Receiver<Result<ModelEvent, BackendError>>;

/// A boxed future used by the object-safe model boundary.
pub type BackendFuture<'a> =
    Pin<Box<dyn Future<Output = Result<BackendStream, BackendError>> + Send + 'a>>;

/// Backend errors mapped into a provider-neutral shape.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum BackendError {
    /// Credentials were absent without exposing their value.
    #[error("Rainy credentials are unavailable")]
    CredentialsUnavailable,
    /// A provider/network operation failed.
    #[error("backend operation failed: {message}")]
    Operation { message: String, retryable: bool },
    /// A backend payload could not be adapted.
    #[error("backend payload mapping failed: {message}")]
    Mapping { message: String },
    /// The backend is not available in this build/configuration.
    #[error("backend feature unavailable: {feature}")]
    FeatureUnavailable { feature: String },
    /// The user cancelled the backend stream.
    #[error("backend stream cancelled")]
    Cancelled,
}

/// Model capability metadata returned by a lazy catalog request.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct ModelCatalogItem {
    /// Provider-owned model identifier, treated as opaque by AETHER.
    pub id: String,
    /// Optional display label supplied by the backend.
    pub display_name: Option<String>,
    /// Optional context limit supplied by the backend.
    pub context_window: Option<u32>,
    /// Capability metadata kept opaque and never inferred from model names.
    pub capabilities: serde_json::Value,
}

/// Provider-neutral model backend contract.
pub trait ModelBackend: Send + Sync {
    /// Start one semantic model step and return a bounded event receiver.
    fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a>;

    /// Lazily refresh model capability metadata.
    fn discover_models<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<ModelCatalogItem>, BackendError>> + Send + 'a>>;
}
