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
mod planner;
mod policy;
mod repo_map;
mod scheduler;
mod session;
mod session_fs;
mod state;
mod symbol_index;
mod verification;
mod working_set;

pub use aether_core::{CommandClass, CommandEffects};
pub use agent::{Agent, AgentError, AgentMetrics, AgentRequest, AgentRunResult};
pub use backend::{BackendError, BackendFuture, BackendStream, ModelBackend, ModelCatalogItem};
pub use cancellation::CancellationToken;
pub use context::{ContextEngine, capture_git_snapshot};
pub use context_selection::{
    ContextCandidate, ContextKind, RelationshipHint, SelectedContext, SelectedContextItem,
    SymbolHint, select_context, select_context_with_relationships, select_context_with_symbols,
};
pub use fake::{FakeBackend, FakeStep, FakeToolCall};
pub use guardrails::LoopGuardrails;
pub use permission::{
    AllowAllPermissionBroker, NoPermissionBroker, PermissionBroker, PermissionFuture,
    SessionPermissionBroker,
};
pub use planner::{
    DEFAULT_MAX_PLANNER_ACTIONS, DEFAULT_MAX_PLANNER_BYTES, DEFAULT_MAX_PLANNER_CANDIDATES,
    DEFAULT_MAX_PLANNER_DEPTH, DEFAULT_MAX_PLANNER_EXECUTION_MICROS, DEFAULT_MAX_PLANNER_FILES,
    MAX_PLANNER_OBSERVATION_BYTES, MIN_PLANNER_CONFIDENCE, PlannerExecutionError, PlannerLimits,
    RepositoryAction, RepositoryActionPlan, RepositoryActionPlanner, RepositoryExcerpt,
    RepositoryFileObservation, RepositoryObservation, RepositoryPlanRequest, RepositoryReadTarget,
    RepositoryRequestKind, RepositorySymbolObservation,
};
pub use policy::{
    AutonomousCodingPolicy, MAX_POLICY_CANDIDATES, MAX_POLICY_OBSERVED_PATHS,
    MIN_MUTATION_CONFIDENCE, RankedAction,
};
pub use repo_map::{
    DEFAULT_MAX_REPO_FILES, DEFAULT_MAX_REPO_INSTRUCTION_BYTES, DEFAULT_MAX_REPO_MANIFEST_BYTES,
    DEFAULT_MAX_REPO_MAP_BYTES, DEFAULT_MAX_REPO_MAP_ITEMS, DEFAULT_MAX_REPO_README_BYTES,
    DEFAULT_MAX_REPO_SPECIAL_FILES, DocumentationFile, DocumentationKind, ManifestInfo,
    ManifestKind, ManifestStatus, PackageInfo, RepoFile, RepoFileKind, RepoMap, RepoMapError,
    RepoMapLimits, RepoMapSnapshot, RepoSelection, RepoSelectionKind, RepoSymbolSelection,
    ScopedInstruction, WorkspaceMember,
};
pub use scheduler::{DEFAULT_MAX_PARALLEL_TOOLS, schedule_ready_calls};
pub use session::{
    PersistCheckpoint, PersistTurn, ResumableSession, SessionStore, SessionStoreError,
    SessionSummary, persist_checkpoint, persist_turn,
};
pub use symbol_index::{
    MAX_SYMBOL_FILES, MAX_SYMBOL_LOOKUP_RESULTS, MAX_SYMBOL_RELATIONSHIP_FANOUT,
    MAX_SYMBOL_RELATIONSHIPS_PER_FILE, MAX_SYMBOL_SOURCE_BYTES, MAX_SYMBOLS_PER_FILE, Symbol,
    SymbolFile, SymbolIndex, SymbolKind, SymbolLanguage, SymbolMatch, SymbolRelation,
    SymbolRelationKind, SymbolRelationship, SymbolRelationshipKind, SymbolRelationshipMatch,
    parse_rust, parse_source,
};
pub use verification::{
    CommandIntent, PlannedCommand, VerificationPlan, classify_command, command_effects,
    plan_verification,
};
pub use working_set::{
    MAX_WORKING_SET_BLOCKERS, MAX_WORKING_SET_ITEMS, MAX_WORKING_SET_RENDER_BYTES, WorkingSet,
    WorkingSetItem, WorkingSetRole,
};
