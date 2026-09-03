use std::{
    collections::{HashMap, HashSet},
    future::{Future, ready},
    pin::Pin,
    sync::{Arc, Mutex},
};

use aether_core::{PermissionClass, PermissionDecision, PermissionRequest, ToolCallId};
use tokio::sync::oneshot;

/// Boxed, backend- and terminal-neutral permission decision future.
pub type PermissionFuture<'a> = Pin<Box<dyn Future<Output = PermissionDecision> + Send + 'a>>;

/// Runtime permission broker used by the agent before executing a tool.
pub trait PermissionBroker: Send + Sync {
    /// Return whether the request needs a user-facing prompt.
    fn needs_prompt(&self, request: &PermissionRequest) -> bool;

    /// Register and await a decision for one exact request.
    fn decide<'a>(&'a self, request: PermissionRequest) -> PermissionFuture<'a>;

    /// Remove a pending request when the turn is cancelled.
    fn cancel(&self, call_id: &ToolCallId);
}

/// Fail-closed broker for one-shot/non-interactive execution.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPermissionBroker;

impl PermissionBroker for NoPermissionBroker {
    fn needs_prompt(&self, _: &PermissionRequest) -> bool {
        false
    }

    fn decide<'a>(&'a self, _: PermissionRequest) -> PermissionFuture<'a> {
        Box::pin(ready(PermissionDecision::Deny))
    }

    fn cancel(&self, _: &ToolCallId) {}
}

/// Explicit opt-in broker for trusted, isolated non-interactive runs.
#[derive(Clone, Copy, Debug, Default)]
pub struct AllowAllPermissionBroker;

impl PermissionBroker for AllowAllPermissionBroker {
    fn needs_prompt(&self, _: &PermissionRequest) -> bool {
        false
    }

    fn decide<'a>(&'a self, _: PermissionRequest) -> PermissionFuture<'a> {
        Box::pin(ready(PermissionDecision::AllowOnce))
    }

    fn cancel(&self, _: &ToolCallId) {}
}

#[derive(Debug)]
struct PendingPermission {
    request: PermissionRequest,
    sender: oneshot::Sender<PermissionDecision>,
}

#[derive(Debug)]
struct PermissionState {
    session_grants: HashSet<(String, PermissionClass)>,
    pending: HashMap<ToolCallId, PendingPermission>,
}

/// In-process permission broker with allow-once and tool/class-scoped session grants.
#[derive(Clone, Debug)]
pub struct SessionPermissionBroker {
    state: Arc<Mutex<PermissionState>>,
}

impl Default for SessionPermissionBroker {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionPermissionBroker {
    /// Construct an empty session grant set.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(PermissionState {
                session_grants: HashSet::new(),
                pending: HashMap::new(),
            })),
        }
    }

    /// Resolve a terminal request. Returns false for an unknown/stale call id.
    pub fn resolve(&self, call_id: &ToolCallId, decision: PermissionDecision) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        let Some(pending) = state.pending.remove(call_id) else {
            return false;
        };
        if decision == PermissionDecision::AllowSession
            && pending.request.class != PermissionClass::BrowserAction
        {
            state.session_grants.insert((pending.request.tool, pending.request.class));
        }
        pending.sender.send(decision).is_ok()
    }

    /// Return whether this exact tool/class scope already has a session grant.
    pub fn has_session_grant(&self, tool: &str, class: PermissionClass) -> bool {
        self.state
            .lock()
            .map(|state| state.session_grants.contains(&(tool.to_owned(), class)))
            .unwrap_or(false)
    }

    #[cfg(test)]
    pub(crate) fn pending_count(&self) -> usize {
        self.state.lock().map(|state| state.pending.len()).unwrap_or(0)
    }
}

impl PermissionBroker for SessionPermissionBroker {
    fn needs_prompt(&self, request: &PermissionRequest) -> bool {
        !self.has_session_grant(&request.tool, request.class)
    }

    fn decide<'a>(&'a self, request: PermissionRequest) -> PermissionFuture<'a> {
        if self.has_session_grant(&request.tool, request.class) {
            return Box::pin(ready(PermissionDecision::AllowSession));
        }
        let (sender, receiver) = oneshot::channel();
        if let Ok(mut state) = self.state.lock() {
            state.pending.insert(request.call_id.clone(), PendingPermission { request, sender });
            Box::pin(async move { receiver.await.unwrap_or(PermissionDecision::Deny) })
        } else {
            Box::pin(ready(PermissionDecision::Deny))
        }
    }

    fn cancel(&self, call_id: &ToolCallId) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.remove(call_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(call: &str, tool: &str, class: PermissionClass) -> PermissionRequest {
        PermissionRequest {
            call_id: ToolCallId::new(call).unwrap(),
            tool: tool.to_owned(),
            class,
            operation: tool.to_owned(),
            target: None,
            details: serde_json::json!({}),
        }
    }

    #[tokio::test]
    async fn explicit_allow_all_broker_is_prompt_free() {
        let broker = AllowAllPermissionBroker;
        let request = request("trusted-call", "write", PermissionClass::WorkspaceWrite);
        assert!(!broker.needs_prompt(&request));
        assert_eq!(broker.decide(request).await, PermissionDecision::AllowOnce);
    }

    #[tokio::test]
    async fn allow_once_is_consumed_and_session_grants_are_scoped() {
        let broker = SessionPermissionBroker::new();
        let write = request("write-1", "write", PermissionClass::WorkspaceWrite);
        assert!(broker.needs_prompt(&write));
        let decision = broker.decide(write.clone());
        assert!(broker.resolve(&write.call_id, PermissionDecision::AllowOnce));
        assert_eq!(decision.await, PermissionDecision::AllowOnce);
        assert!(broker.needs_prompt(&request("write-2", "write", PermissionClass::WorkspaceWrite)));

        let shell = request("shell-1", "shell", PermissionClass::ProcessExecute);
        let decision = broker.decide(shell.clone());
        assert!(broker.resolve(&shell.call_id, PermissionDecision::AllowSession));
        assert_eq!(decision.await, PermissionDecision::AllowSession);
        assert!(!broker.needs_prompt(&request(
            "shell-2",
            "shell",
            PermissionClass::ProcessExecute
        )));
        assert!(broker.needs_prompt(&request("write-3", "write", PermissionClass::WorkspaceWrite)));
    }

    #[tokio::test]
    async fn cancellation_removes_pending_permission() {
        let broker = SessionPermissionBroker::new();
        let request = request("cancel-1", "write", PermissionClass::WorkspaceWrite);
        let decision = broker.decide(request.clone());
        assert_eq!(broker.pending_count(), 1);
        broker.cancel(&request.call_id);
        assert_eq!(broker.pending_count(), 0);
        assert_eq!(decision.await, PermissionDecision::Deny);
    }

    #[tokio::test]
    async fn browser_action_allow_session_never_creates_a_session_grant() {
        let broker = SessionPermissionBroker::new();
        let action_request =
            request("browser-action-1", "browser.click", PermissionClass::BrowserAction);
        let decision = broker.decide(action_request.clone());
        assert!(broker.resolve(&action_request.call_id, PermissionDecision::AllowSession));
        assert_eq!(decision.await, PermissionDecision::AllowSession);
        assert!(broker.needs_prompt(&request(
            "browser-action-2",
            "browser.click",
            PermissionClass::BrowserAction
        )));
    }
}
