#![deny(unsafe_code)]

//! Backend-neutral AETHER Fx agent runtime.

mod agent;
mod backend;
mod cancellation;
mod context;
mod context_selection;
mod fake;
mod guardrails;
mod permission;
mod repo_map;
mod scheduler;
mod session;
mod session_fs;
mod state;
mod verification;

pub use agent::{Agent, AgentError, AgentRequest, AgentRunResult};
pub use backend::{BackendError, BackendFuture, BackendStream, ModelBackend, ModelCatalogItem};
pub use cancellation::CancellationToken;
pub use context::{ContextEngine, capture_git_snapshot};
pub use context_selection::{
    ContextCandidate, ContextKind, SelectedContext, SelectedContextItem, select_context,
};
pub use fake::{FakeBackend, FakeToolCall};
pub use guardrails::LoopGuardrails;
pub use permission::{
    NoPermissionBroker, PermissionBroker, PermissionFuture, SessionPermissionBroker,
};
pub use repo_map::{
    DEFAULT_MAX_REPO_FILES, DEFAULT_MAX_REPO_INSTRUCTION_BYTES, DEFAULT_MAX_REPO_MANIFEST_BYTES,
    DEFAULT_MAX_REPO_MAP_BYTES, DEFAULT_MAX_REPO_MAP_ITEMS, DEFAULT_MAX_REPO_README_BYTES,
    DEFAULT_MAX_REPO_SPECIAL_FILES, DocumentationFile, DocumentationKind, ManifestInfo,
    ManifestKind, ManifestStatus, PackageInfo, RepoFile, RepoFileKind, RepoMap, RepoMapError,
    RepoMapLimits, RepoMapSnapshot, RepoSelection, RepoSelectionKind, ScopedInstruction,
    WorkspaceMember,
};
pub use scheduler::{DEFAULT_MAX_PARALLEL_TOOLS, schedule_ready_calls};
pub use session::{
    PersistTurn, ResumableSession, SessionStore, SessionStoreError, SessionSummary, persist_turn,
};
pub use verification::{
    CommandIntent, PlannedCommand, VerificationPlan, classify_command, plan_verification,
};
