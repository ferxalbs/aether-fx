#![deny(unsafe_code)]

//! Backend-neutral AETHER Fx agent runtime.

mod agent;
mod backend;
mod cancellation;
mod context;
mod fake;
mod permission;
mod session;

pub use agent::{Agent, AgentError, AgentRequest, AgentRunResult};
pub use backend::{BackendError, BackendFuture, BackendStream, ModelBackend, ModelCatalogItem};
pub use cancellation::CancellationToken;
pub use context::{ContextEngine, capture_git_snapshot};
pub use fake::{FakeBackend, FakeToolCall};
pub use permission::{
    NoPermissionBroker, PermissionBroker, PermissionFuture, SessionPermissionBroker,
};
pub use session::{PersistTurn, ResumableSession, SessionStore, SessionStoreError, persist_turn};
