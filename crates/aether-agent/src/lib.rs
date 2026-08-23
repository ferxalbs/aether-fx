#![deny(unsafe_code)]

//! Backend-neutral AETHER Fx agent runtime.

mod agent;
mod backend;
mod cancellation;
mod context;
mod fake;
mod permission;
mod scheduler;
mod session;
mod session_fs;
mod state;

pub use agent::{Agent, AgentError, AgentRequest, AgentRunResult};
pub use backend::{BackendError, BackendFuture, BackendStream, ModelBackend, ModelCatalogItem};
pub use cancellation::CancellationToken;
pub use context::{ContextEngine, capture_git_snapshot};
pub use fake::{FakeBackend, FakeToolCall};
pub use permission::{
    NoPermissionBroker, PermissionBroker, PermissionFuture, SessionPermissionBroker,
};
pub use scheduler::{DEFAULT_MAX_PARALLEL_TOOLS, schedule_ready_calls};
pub use session::{
    PersistTurn, ResumableSession, SessionStore, SessionStoreError, SessionSummary, persist_turn,
};
