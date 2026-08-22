#![deny(unsafe_code)]

//! Backend-neutral AETHER Fx agent runtime.

mod agent;
mod backend;
mod cancellation;
mod fake;
mod session;

pub use agent::{Agent, AgentError, AgentRequest, AgentRunResult};
pub use backend::{BackendError, BackendFuture, BackendStream, ModelBackend, ModelCatalogItem};
pub use cancellation::CancellationToken;
pub use fake::{FakeBackend, FakeToolCall};
pub use session::{SessionStore, SessionStoreError};
