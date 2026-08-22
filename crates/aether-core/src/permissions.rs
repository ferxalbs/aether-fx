use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreResult};

/// Policy classes used to authorize local and network-affecting operations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionClass {
    /// Read-only access inside the canonical workspace.
    ReadOnly,
    /// Create or replace workspace files.
    WorkspaceWrite,
    /// Execute a finite child process.
    ProcessExecute,
    /// Create or control a persistent child process.
    ProcessPersistent,
    /// A future mutating Git operation.
    GitMutate,
    /// Access outside the canonical workspace.
    OutsideWorkspace,
    /// A future network mutation.
    NetworkMutation,
}

impl std::fmt::Display for PermissionClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::ProcessExecute => "process_execute",
            Self::ProcessPersistent => "process_persistent",
            Self::GitMutate => "git_mutate",
            Self::OutsideWorkspace => "outside_workspace",
            Self::NetworkMutation => "network_mutation",
        })
    }
}

/// The user's response to a permission check.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    /// The policy allows the operation.
    Allowed,
    /// The operation must be shown to and approved by the user.
    RequiresConfirmation,
}

/// A structured operation presented to a future confirmation UI.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PermissionRequest {
    /// Policy category being requested.
    pub class: PermissionClass,
    /// Human-readable operation name.
    pub operation: String,
    /// Target or working directory, when applicable.
    pub target: Option<String>,
    /// Structured details such as program, arguments, and cwd.
    pub details: serde_json::Value,
}

/// Explicit permission policy. Read-only workspace operations are allowed by default.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct PermissionPolicy {
    /// Allow read-only operations.
    pub read_only: bool,
    /// Allow workspace writes.
    pub workspace_write: bool,
    /// Allow finite process execution.
    pub process_execute: bool,
    /// Allow persistent process lifecycle operations.
    pub process_persistent: bool,
    /// Allow Git mutations.
    pub git_mutate: bool,
    /// Allow outside-workspace operations.
    pub outside_workspace: bool,
    /// Allow network mutations.
    pub network_mutation: bool,
}

impl Default for PermissionPolicy {
    fn default() -> Self {
        Self {
            read_only: true,
            workspace_write: false,
            process_execute: false,
            process_persistent: false,
            git_mutate: false,
            outside_workspace: false,
            network_mutation: false,
        }
    }
}

impl PermissionPolicy {
    /// Allow every currently defined class. Composition roots should use this only after a UI decision.
    pub fn allow_all() -> Self {
        Self {
            read_only: true,
            workspace_write: true,
            process_execute: true,
            process_persistent: true,
            git_mutate: true,
            outside_workspace: true,
            network_mutation: true,
        }
    }

    fn allows(&self, class: PermissionClass) -> bool {
        match class {
            PermissionClass::ReadOnly => self.read_only,
            PermissionClass::WorkspaceWrite => self.workspace_write,
            PermissionClass::ProcessExecute => self.process_execute,
            PermissionClass::ProcessPersistent => self.process_persistent,
            PermissionClass::GitMutate => self.git_mutate,
            PermissionClass::OutsideWorkspace => self.outside_workspace,
            PermissionClass::NetworkMutation => self.network_mutation,
        }
    }
}

/// Small policy engine shared by all tool implementations.
#[derive(Clone, Debug)]
pub struct PermissionEngine {
    policy: PermissionPolicy,
}

impl PermissionEngine {
    /// Construct an engine from an explicit policy.
    pub fn new(policy: PermissionPolicy) -> Self {
        Self { policy }
    }

    /// Return the decision without mutating state or prompting.
    pub fn decide(&self, request: &PermissionRequest) -> PermissionDecision {
        if self.policy.allows(request.class) {
            PermissionDecision::Allowed
        } else {
            PermissionDecision::RequiresConfirmation
        }
    }

    /// Authorize or return a structured core error suitable for a tool result.
    pub fn authorize(&self, request: &PermissionRequest) -> CoreResult<()> {
        match self.decide(request) {
            PermissionDecision::Allowed => Ok(()),
            PermissionDecision::RequiresConfirmation => Err(CoreError::PermissionRequired {
                operation: request.operation.clone(),
                class: request.class.to_string(),
            }),
        }
    }

    /// Return the policy for diagnostics and composition.
    pub fn policy(&self) -> &PermissionPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_requires_confirmation_for_writes() {
        let engine = PermissionEngine::new(PermissionPolicy::default());
        let request = PermissionRequest {
            class: PermissionClass::WorkspaceWrite,
            operation: "write file".to_owned(),
            target: Some("src/main.rs".to_owned()),
            details: serde_json::json!({"path": "src/main.rs"}),
        };
        assert_eq!(engine.decide(&request), PermissionDecision::RequiresConfirmation);
        assert!(engine.authorize(&request).is_err());
    }
}
