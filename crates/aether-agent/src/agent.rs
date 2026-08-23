use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use aether_core::{
    AgentEvent, BoundedText, ContextSnapshot, DEFAULT_MAX_OUTPUT_BYTES, ExecutionPermit,
    ModelEvent, ModelRequest, OpaqueContinuation, PermissionDecision, SessionId, StepId,
    ToolCallId, ToolExecutionContext, ToolExecutor, ToolInvocation, TurnId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::context::{ContextEngine, capture_git_snapshot};
use crate::{BackendError, CancellationToken, ModelBackend, NoPermissionBroker, PermissionBroker};

/// A user-requested agent turn.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentRequest {
    /// Local session identity.
    pub session_id: SessionId,
    /// Local turn identity.
    pub turn_id: TurnId,
    /// User prompt.
    pub prompt: String,
    /// Optional opaque model identifier.
    pub model: Option<String>,
    /// Maximum model/tool continuation steps.
    pub max_steps: u16,
    /// Opaque provider continuation from the preceding external turn.
    pub continuation: Option<OpaqueContinuation>,
    /// Absolute workspace root used for git snapshots and resume refresh.
    #[serde(default)]
    pub workspace_root: Option<String>,
    /// Restored working context for resume or in-process continuity.
    #[serde(default)]
    pub context_seed: Option<ContextSnapshot>,
}

impl AgentRequest {
    /// Construct a bounded-default request.
    pub fn new(session_id: SessionId, turn_id: TurnId, prompt: impl Into<String>) -> Self {
        Self {
            session_id,
            turn_id,
            prompt: prompt.into(),
            model: None,
            max_steps: 16,
            continuation: None,
            workspace_root: None,
            context_seed: None,
        }
    }
}

/// A completed turn summary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AgentRunResult {
    /// Bounded accumulated assistant text.
    pub text: BoundedText,
    /// Number of semantic model steps used.
    pub steps: u16,
    /// Whether the turn ended due to cancellation.
    pub cancelled: bool,
    /// Opaque provider continuation for the next external turn.
    pub continuation: Option<OpaqueContinuation>,
    /// Bounded working context after this turn.
    pub context: ContextSnapshot,
}

/// Agent loop failures are typed and secret-free.
#[derive(Debug, Error)]
pub enum AgentError {
    /// Backend mapping or transport failure.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// The event consumer disappeared.
    #[error("agent event consumer closed")]
    EventChannelClosed,
    /// Tool execution returned an internal failure.
    #[error("tool execution failed: {0}")]
    Tool(String),
    /// The turn exceeded its semantic step cap.
    #[error("agent step limit exceeded")]
    StepLimit,
    /// The user cancelled the turn.
    #[error("agent turn cancelled")]
    Cancelled,
}

/// Backend-neutral agent orchestration.
pub struct Agent<B, T> {
    backend: Arc<B>,
    tools: Arc<T>,
    context: Mutex<ContextEngine>,
}

impl<B, T> Agent<B, T>
where
    B: ModelBackend + 'static,
    T: ToolExecutor + 'static,
{
    /// Compose one backend and one tool executor.
    pub fn new(backend: B, tools: T) -> Self {
        Self {
            backend: Arc::new(backend),
            tools: Arc::new(tools),
            context: Mutex::new(ContextEngine::new(String::new(), None)),
        }
    }

    /// Execute one bounded turn, emitting ordered events into a bounded channel.
    pub async fn run(
        &self,
        request: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        cancellation: CancellationToken,
    ) -> Result<AgentRunResult, AgentError> {
        self.run_with_broker(request, events, cancellation, &NoPermissionBroker).await
    }

    /// Execute one bounded turn with an explicit terminal-neutral permission broker.
    pub async fn run_with_broker<P: PermissionBroker + ?Sized>(
        &self,
        request: AgentRequest,
        events: mpsc::Sender<AgentEvent>,
        cancellation: CancellationToken,
        broker: &P,
    ) -> Result<AgentRunResult, AgentError> {
        let mut accumulated = String::new();
        self.prepare_turn(&request);
        if let Some(root) = request.workspace_root.clone() {
            let git =
                tokio::task::spawn_blocking(move || capture_git_snapshot(&PathBuf::from(root)))
                    .await
                    .unwrap_or_else(|_| aether_core::GitSnapshot {
                        branch: None,
                        status: BoundedText::new("git snapshot cancelled", 128),
                        available: false,
                    });
            self.with_context(|engine| engine.set_git(git));
        }
        let mut continuation = request.continuation.clone();
        let mut input =
            assemble_user_input(&request.prompt, self.context_packet(continuation.is_some()));
        let mut steps = 0_u16;
        let mut reconstructed = false;

        loop {
            if cancellation.is_cancelled() {
                let _ = events
                    .send(AgentEvent::Error {
                        message: BoundedText::new("agent turn cancelled", DEFAULT_MAX_OUTPUT_BYTES),
                    })
                    .await;
                return Err(AgentError::Cancelled);
            }
            if steps >= request.max_steps {
                return Err(AgentError::StepLimit);
            }
            steps = steps.saturating_add(1);
            let step_id = StepId::new(format!("{}.{}", request.turn_id, steps))
                .map_err(|error| AgentError::Tool(error.to_string()))?;
            let model_request = ModelRequest {
                session_id: request.session_id.clone(),
                turn_id: request.turn_id.clone(),
                step_id,
                model: request.model.clone(),
                input: input.clone(),
                tools: self.tools.definitions().to_vec(),
                continuation: continuation.clone(),
            };
            let backend_future = self.backend.stream_step(model_request);
            let mut stream = tokio::select! {
                _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                result = backend_future => match result {
                    Err(error)
                        if steps == 1
                            && !reconstructed
                            && continuation.is_some()
                            && matches!(error, BackendError::InvalidContinuation { .. }) =>
                    {
                        continuation = None;
                        reconstructed = true;
                        input = assemble_user_input(&request.prompt, Some(self.render_context(true)));
                        send_event(
                            &events,
                            AgentEvent::Warning {
                                message: BoundedText::new(
                                    "Rainy continuation was unusable; reconstructed from bounded session state",
                                    DEFAULT_MAX_OUTPUT_BYTES,
                                ),
                            },
                        )
                        .await?;
                        steps = steps.saturating_sub(1);
                        continue;
                    }
                    result => result?,
                },
            };
            let mut tool_calls = Vec::new();
            loop {
                let event = tokio::select! {
                    _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                    event = stream.recv() => event,
                };
                let Some(event) = event else {
                    break;
                };
                let event = event?;
                match event {
                    ModelEvent::TextDelta { text } => {
                        append_bounded(&mut accumulated, &text, DEFAULT_MAX_OUTPUT_BYTES);
                        send_event(
                            &events,
                            AgentEvent::TextDelta {
                                text: BoundedText::new(text, DEFAULT_MAX_OUTPUT_BYTES),
                            },
                        )
                        .await?;
                    }
                    ModelEvent::ToolCall { call_id, name, arguments } => {
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelEvent::Usage { input_tokens, output_tokens, total_tokens } => {
                        send_event(
                            &events,
                            AgentEvent::Usage {
                                usage: aether_core::UsageMetadata {
                                    input_tokens,
                                    output_tokens,
                                    total_tokens,
                                },
                            },
                        )
                        .await?;
                    }
                    ModelEvent::Warning { message } => {
                        send_event(
                            &events,
                            AgentEvent::Warning {
                                message: BoundedText::new(message, DEFAULT_MAX_OUTPUT_BYTES),
                            },
                        )
                        .await?;
                    }
                    ModelEvent::Done { continuation: next } => {
                        continuation = next;
                    }
                }
            }

            if tool_calls.is_empty() {
                send_event(&events, AgentEvent::Done).await?;
                self.with_context(|engine| engine.set_continuation(continuation.clone()));
                return Ok(AgentRunResult {
                    text: BoundedText::new(accumulated, DEFAULT_MAX_OUTPUT_BYTES),
                    steps,
                    cancelled: false,
                    continuation,
                    context: self.current_context(),
                });
            }

            let mut outputs = Vec::with_capacity(tool_calls.len());
            for (call_id, name, arguments) in tool_calls {
                if cancellation.is_cancelled() {
                    return Err(AgentError::Cancelled);
                }
                let invocation = ToolInvocation {
                    call_id: call_id.clone(),
                    name: name.clone(),
                    input: arguments,
                };
                let permission_request = self.tools.permission_request(&invocation);
                let permit = if let Some(permission_request) = permission_request {
                    let needs_prompt = broker.needs_prompt(&permission_request);
                    let decision_future = broker.decide(permission_request.clone());
                    if needs_prompt {
                        send_event(
                            &events,
                            AgentEvent::PermissionRequested { request: permission_request.clone() },
                        )
                        .await?;
                    }
                    let decision = tokio::select! {
                        _ = cancellation.cancelled() => {
                            broker.cancel(&call_id);
                            return Err(AgentError::Cancelled);
                        }
                        decision = decision_future => decision,
                    };
                    if cancellation.is_cancelled() {
                        broker.cancel(&call_id);
                        return Err(AgentError::Cancelled);
                    }
                    if needs_prompt {
                        send_event(
                            &events,
                            AgentEvent::PermissionResolved { call_id: call_id.clone(), decision },
                        )
                        .await?;
                    }
                    match decision {
                        PermissionDecision::AllowOnce | PermissionDecision::AllowSession => {
                            Some(ExecutionPermit::new(
                                call_id.clone(),
                                name.clone(),
                                permission_request.class,
                            ))
                        }
                        PermissionDecision::Deny => None,
                    }
                } else {
                    let class = self
                        .tools
                        .definitions()
                        .iter()
                        .find(|definition| definition.name == name)
                        .map(|definition| definition.permission)
                        .unwrap_or(aether_core::PermissionClass::ReadOnly);
                    Some(ExecutionPermit::new(call_id.clone(), name.clone(), class))
                };
                let permission = self
                    .tools
                    .definitions()
                    .iter()
                    .find(|definition| definition.name == name)
                    .map(|definition| definition.permission.to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                send_event(
                    &events,
                    AgentEvent::ToolStarted {
                        call_id: call_id.clone(),
                        name: name.clone(),
                        permission,
                        operation: name.clone(),
                        step_id: None,
                    },
                )
                .await?;
                let tool_input = invocation.input.clone();
                let result = if let Some(permit) = permit {
                    let execution = self.tools.execute(
                        invocation,
                        ToolExecutionContext::new(cancellation.flag(), permit),
                    );
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                        result = execution => result,
                    }
                } else {
                    aether_core::ToolResult::failure(
                        call_id.clone(),
                        "permission_denied",
                        "user denied permission for this operation",
                        false,
                        DEFAULT_MAX_OUTPUT_BYTES,
                    )
                };
                send_event(
                    &events,
                    AgentEvent::ToolOutput {
                        call_id: call_id.clone(),
                        output: result.output.clone(),
                    },
                )
                .await?;
                send_event(
                    &events,
                    AgentEvent::ToolFinished { call_id: call_id.clone(), ok: result.ok },
                )
                .await?;
                self.with_context(|engine| {
                    engine.observe_tool(&name, &tool_input, &result);
                });
                outputs.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id.as_str(),
                    "output": result.output.as_str(),
                    "ok": result.ok,
                    "error": result.error.as_ref().map(|error| json!({
                        "code": error.code.as_str(),
                        "message": error.message.as_str(),
                        "retryable": error.retryable,
                    })),
                }));
            }
            input = serde_json::Value::Array(outputs);
        }
    }

    fn prepare_turn(&self, request: &AgentRequest) {
        self.with_context(|engine| {
            if let Some(seed) = request.context_seed.clone() {
                *engine = ContextEngine::restore(seed);
            }
            if let Some(root) = &request.workspace_root
                && engine.snapshot().workspace_root.is_empty()
            {
                *engine = ContextEngine::new(root, request.model.clone());
            }
            engine.set_model(request.model.clone());
            engine.set_task(&request.prompt);
            if request.continuation.is_some() {
                engine.set_continuation(request.continuation.clone());
            }
        });
    }

    fn with_context<R>(&self, operation: impl FnOnce(&mut ContextEngine) -> R) -> R {
        let mut engine = self.context.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        operation(&mut engine)
    }

    fn current_context(&self) -> ContextSnapshot {
        self.with_context(|engine| engine.snapshot().clone())
    }

    fn render_context(&self, include_guidance: bool) -> String {
        self.with_context(|engine| engine.render(include_guidance))
    }

    fn context_packet(&self, has_continuation: bool) -> Option<String> {
        self.with_context(|engine| {
            engine.should_attach_to_prompt(has_continuation).then(|| engine.render(true))
        })
    }
}

fn assemble_user_input(prompt: &str, context_packet: Option<String>) -> serde_json::Value {
    let content = match context_packet {
        Some(packet) if !packet.trim().is_empty() => format!("{packet}\n\n{prompt}"),
        _ => prompt.to_owned(),
    };
    json!([{"type":"message","role":"user","content": content}])
}

async fn send_event(
    sender: &mpsc::Sender<AgentEvent>,
    event: AgentEvent,
) -> Result<(), AgentError> {
    sender.send(event).await.map_err(|_| AgentError::EventChannelClosed)
}

fn append_bounded(target: &mut String, value: &str, max_bytes: usize) {
    if target.len() >= max_bytes {
        return;
    }
    let remaining = max_bytes - target.len();
    let bounded = BoundedText::new(value, remaining);
    target.push_str(bounded.as_str());
}

#[allow(dead_code)]
fn _tool_call_id_type_is_used(id: ToolCallId) -> String {
    id.into_string()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::{BackendFuture, ModelCatalogItem, SessionPermissionBroker};
    use aether_core::tools::ToolFuture;
    use aether_core::{
        ModelEvent, ModelRequest, OpaqueContinuation, PermissionClass, ToolDefinition,
        ToolExecutionContext, ToolResult,
    };
    use serde_json::json;

    struct FakeTools {
        definitions: Vec<ToolDefinition>,
    }

    impl ToolExecutor for FakeTools {
        fn definitions(&self) -> &[ToolDefinition] {
            &self.definitions
        }

        fn permission_request(&self, _: &ToolInvocation) -> Option<aether_core::PermissionRequest> {
            None
        }

        fn execute<'a>(
            &'a self,
            invocation: ToolInvocation,
            _: ToolExecutionContext,
        ) -> ToolFuture<'a> {
            Box::pin(
                async move { ToolResult::success_text(invocation.call_id, "tool output", 1024) },
            )
        }
    }

    fn tools() -> FakeTools {
        FakeTools {
            definitions: vec![ToolDefinition {
                name: "read".to_owned(),
                description: "test read".to_owned(),
                input_schema: json!({"type": "object"}),
                permission: PermissionClass::ReadOnly,
            }],
        }
    }

    #[tokio::test]
    async fn fake_backend_proves_tool_continuation_and_event_order() {
        let backend =
            crate::FakeBackend::new("completed").with_tool_call("read", json!({"files": []}));
        let agent = Agent::new(backend, tools());
        let session = SessionId::new("session-test").unwrap();
        let turn = TurnId::new("turn-test").unwrap();
        let mut request = AgentRequest::new(session, turn, "inspect");
        request.max_steps = 4;
        let (sender, mut receiver) = mpsc::channel(32);
        let result = agent.run(request, sender, CancellationToken::new()).await.unwrap();
        let mut kinds = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            kinds.push(event.kind());
        }
        assert_eq!(result.text.as_str(), "completed");
        assert_eq!(
            kinds,
            vec!["tool_started", "tool_output", "tool_finished", "text_delta", "done"]
        );
    }

    #[tokio::test]
    async fn cancellation_is_first_class() {
        let agent = Agent::new(crate::FakeBackend::default(), tools());
        let request = AgentRequest::new(
            SessionId::new("session-cancel").unwrap(),
            TurnId::new("turn-cancel").unwrap(),
            "stop",
        );
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let (sender, _receiver) = mpsc::channel(4);
        assert!(matches!(
            agent.run(request, sender, cancellation).await,
            Err(AgentError::Cancelled)
        ));
    }

    #[derive(Clone, Default)]
    struct ContinuationBackend {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl ModelBackend for ContinuationBackend {
        fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a> {
            let requests = self.requests.clone();
            Box::pin(async move {
                requests.lock().unwrap().push(request.clone());
                let (sender, receiver) = mpsc::channel(4);
                sender
                    .send(Ok(ModelEvent::TextDelta { text: "ok".to_owned() }))
                    .await
                    .map_err(|_| BackendError::Cancelled)?;
                sender
                    .send(Ok(ModelEvent::Done {
                        continuation: Some(OpaqueContinuation(json!({
                            "previous_response_id": request.step_id.as_str()
                        }))),
                    }))
                    .await
                    .map_err(|_| BackendError::Cancelled)?;
                Ok(receiver)
            })
        }

        fn discover_models<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<ModelCatalogItem>, BackendError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[tokio::test]
    async fn external_turns_preserve_continuation_and_identity() {
        let backend = ContinuationBackend::default();
        let requests = backend.requests.clone();
        let agent = Agent::new(backend, tools());
        let session = SessionId::new("session-continuity").unwrap();
        let (sender, _receiver) = mpsc::channel(8);

        let first = agent
            .run(
                AgentRequest::new(session.clone(), TurnId::new("turn-1").unwrap(), "first"),
                sender,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let (sender, _receiver) = mpsc::channel(8);
        let mut second_request =
            AgentRequest::new(session.clone(), TurnId::new("turn-2").unwrap(), "second");
        second_request.continuation = first.continuation.clone();
        agent.run(second_request, sender, CancellationToken::new()).await.unwrap();

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].session_id, session);
        assert_eq!(requests[1].session_id, requests[0].session_id);
        assert_ne!(requests[0].turn_id, requests[1].turn_id);
        assert!(requests[0].continuation.is_none());
        assert_eq!(requests[1].continuation, first.continuation);
    }

    struct PermissionTools {
        executions: Arc<AtomicUsize>,
    }

    impl ToolExecutor for PermissionTools {
        fn definitions(&self) -> &[ToolDefinition] {
            static DEFINITIONS: std::sync::OnceLock<Vec<ToolDefinition>> =
                std::sync::OnceLock::new();
            DEFINITIONS.get_or_init(|| {
                vec![ToolDefinition {
                    name: "write".to_owned(),
                    description: "test write".to_owned(),
                    input_schema: json!({"type": "object"}),
                    permission: PermissionClass::WorkspaceWrite,
                }]
            })
        }

        fn permission_request(
            &self,
            invocation: &ToolInvocation,
        ) -> Option<aether_core::PermissionRequest> {
            Some(aether_core::PermissionRequest {
                call_id: invocation.call_id.clone(),
                tool: invocation.name.clone(),
                class: PermissionClass::WorkspaceWrite,
                operation: "write test file".to_owned(),
                target: Some("test.txt".to_owned()),
                details: json!({"path": "test.txt", "bytes": 1}),
            })
        }

        fn execute<'a>(
            &'a self,
            invocation: ToolInvocation,
            _: ToolExecutionContext,
        ) -> ToolFuture<'a> {
            self.executions.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move { ToolResult::success_text(invocation.call_id, "mutated", 1024) })
        }
    }

    #[tokio::test]
    async fn noninteractive_mutation_is_denied_before_tool_execution() {
        let executions = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(
            crate::FakeBackend::new("done").with_tool_call("write", json!({"path": "test.txt"})),
            PermissionTools { executions: executions.clone() },
        );
        let (sender, mut receiver) = mpsc::channel(16);
        let result = agent
            .run(
                AgentRequest::new(
                    SessionId::new("session-permission").unwrap(),
                    TurnId::new("turn-permission").unwrap(),
                    "write",
                ),
                sender,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(executions.load(Ordering::Relaxed), 0);
        let mut denied = false;
        while let Ok(event) = receiver.try_recv() {
            if matches!(event, AgentEvent::ToolFinished { ok: false, .. }) {
                denied = true;
            }
        }
        assert!(denied);
        assert_eq!(result.text.as_str(), "done");
    }

    #[tokio::test]
    async fn agent_permission_event_resolves_allow_once_before_execution() {
        let executions = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(Agent::new(
            crate::FakeBackend::new("done").with_tool_call("write", json!({"path": "test.txt"})),
            PermissionTools { executions: executions.clone() },
        ));
        let broker = SessionPermissionBroker::new();
        let (sender, mut receiver) = mpsc::channel(16);
        let task_agent = Arc::clone(&agent);
        let task_broker = broker.clone();
        let task = tokio::spawn(async move {
            task_agent
                .run_with_broker(
                    AgentRequest::new(
                        SessionId::new("session-permission-event").unwrap(),
                        TurnId::new("turn-permission-event").unwrap(),
                        "write",
                    ),
                    sender,
                    CancellationToken::new(),
                    &task_broker,
                )
                .await
        });
        while let Some(event) = receiver.recv().await {
            if let AgentEvent::PermissionRequested { request } = event {
                assert!(broker.resolve(&request.call_id, PermissionDecision::AllowOnce));
                break;
            }
        }
        assert!(task.await.unwrap().is_ok());
        assert_eq!(executions.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn cancelling_during_permission_prompt_cancels_before_tool_execution() {
        let executions = Arc::new(AtomicUsize::new(0));
        let agent = Arc::new(Agent::new(
            crate::FakeBackend::new("done").with_tool_call("write", json!({"path": "test.txt"})),
            PermissionTools { executions: executions.clone() },
        ));
        let broker = SessionPermissionBroker::new();
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let (sender, mut receiver) = mpsc::channel(16);
        let task_agent = Arc::clone(&agent);
        let task_broker = broker.clone();
        let task = tokio::spawn(async move {
            task_agent
                .run_with_broker(
                    AgentRequest::new(
                        SessionId::new("session-permission-cancel").unwrap(),
                        TurnId::new("turn-permission-cancel").unwrap(),
                        "write",
                    ),
                    sender,
                    task_cancellation,
                    &task_broker,
                )
                .await
        });
        let call_id = loop {
            if let Some(AgentEvent::PermissionRequested { request }) = receiver.recv().await {
                break request.call_id;
            }
        };
        assert_eq!(broker.pending_count(), 1);
        cancellation.cancel();
        assert!(matches!(task.await.unwrap(), Err(AgentError::Cancelled)));
        assert_eq!(executions.load(Ordering::Relaxed), 0);
        assert_eq!(broker.pending_count(), 0);
        assert!(!broker.resolve(&call_id, PermissionDecision::AllowOnce));
        assert!(
            !std::iter::from_fn(|| receiver.try_recv().ok())
                .any(|event| matches!(event, AgentEvent::ToolFinished { .. }))
        );
    }

    #[derive(Clone, Copy)]
    struct ToolThenIncompleteBackend;

    impl ModelBackend for ToolThenIncompleteBackend {
        fn stream_step<'a>(&'a self, _: ModelRequest) -> BackendFuture<'a> {
            Box::pin(async {
                let (sender, receiver) = mpsc::channel(4);
                sender
                    .send(Ok(ModelEvent::ToolCall {
                        call_id: ToolCallId::new("truncated-call").unwrap(),
                        name: "write".to_owned(),
                        arguments: json!({"path": "test.txt"}),
                    }))
                    .await
                    .unwrap();
                sender
                    .send(Err(BackendError::IncompleteStream {
                        message: "synthetic truncated response".to_owned(),
                    }))
                    .await
                    .unwrap();
                Ok(receiver)
            })
        }

        fn discover_models<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<ModelCatalogItem>, BackendError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[tokio::test]
    async fn incomplete_stream_after_tool_call_never_executes_the_tool() {
        let executions = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(
            ToolThenIncompleteBackend,
            PermissionTools { executions: executions.clone() },
        );
        let (sender, _receiver) = mpsc::channel(16);
        let result = agent
            .run(
                AgentRequest::new(
                    SessionId::new("session-truncated-tool").unwrap(),
                    TurnId::new("turn-truncated-tool").unwrap(),
                    "write",
                ),
                sender,
                CancellationToken::new(),
            )
            .await;
        assert!(matches!(result, Err(AgentError::Backend(BackendError::IncompleteStream { .. }))));
        assert_eq!(executions.load(Ordering::Relaxed), 0);
    }

    struct SlowTools;

    impl ToolExecutor for SlowTools {
        fn definitions(&self) -> &[ToolDefinition] {
            static DEFINITIONS: std::sync::OnceLock<Vec<ToolDefinition>> =
                std::sync::OnceLock::new();
            DEFINITIONS.get_or_init(|| {
                vec![ToolDefinition {
                    name: "read".to_owned(),
                    description: "slow read".to_owned(),
                    input_schema: json!({"type": "object"}),
                    permission: PermissionClass::ReadOnly,
                }]
            })
        }

        fn permission_request(&self, _: &ToolInvocation) -> Option<aether_core::PermissionRequest> {
            None
        }

        fn execute<'a>(
            &'a self,
            invocation: ToolInvocation,
            context: ToolExecutionContext,
        ) -> ToolFuture<'a> {
            Box::pin(async move {
                loop {
                    if context.cancellation().is_cancelled() {
                        return ToolResult::failure(
                            invocation.call_id,
                            "cancelled",
                            "slow fixture cancelled",
                            true,
                            1024,
                        );
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
        }
    }

    #[derive(Clone, Default)]
    struct UnusableContinuationBackend {
        calls: Arc<AtomicUsize>,
    }

    impl ModelBackend for UnusableContinuationBackend {
        fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                if request.continuation.is_some() {
                    return Err(BackendError::InvalidContinuation {
                        message: "stored previous_response_id is not usable".to_owned(),
                    });
                }
                let (sender, receiver) = mpsc::channel(4);
                sender
                    .send(Ok(ModelEvent::TextDelta { text: "reconstructed".to_owned() }))
                    .await
                    .map_err(|_| BackendError::Cancelled)?;
                sender
                    .send(Ok(ModelEvent::Done { continuation: None }))
                    .await
                    .map_err(|_| BackendError::Cancelled)?;
                Ok(receiver)
            })
        }

        fn discover_models<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<ModelCatalogItem>, BackendError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[tokio::test]
    async fn unusable_rainy_continuation_reconstructs_from_bounded_state() {
        let agent = Agent::new(UnusableContinuationBackend::default(), tools());
        let mut request = AgentRequest::new(
            SessionId::new("session-reconstruct").unwrap(),
            TurnId::new("turn-reconstruct").unwrap(),
            "continue",
        );
        request.continuation =
            Some(OpaqueContinuation(json!({"previous_response_id": "stale-response"})));
        request.context_seed = Some(ContextSnapshot::new("/workspace", Some("model-a".to_owned())));
        let (sender, _receiver) = mpsc::channel(8);
        let result = agent.run(request, sender, CancellationToken::new()).await.unwrap();
        assert_eq!(result.text.as_str(), "reconstructed");
        assert!(result.continuation.is_none());
    }

    #[derive(Clone, Default)]
    struct UnrelatedContinuationErrorBackend {
        calls: Arc<AtomicUsize>,
    }

    impl ModelBackend for UnrelatedContinuationErrorBackend {
        fn stream_step<'a>(&'a self, _: ModelRequest) -> BackendFuture<'a> {
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                Err(BackendError::Operation {
                    message: "transport failure".to_owned(),
                    retryable: true,
                })
            })
        }

        fn discover_models<'a>(
            &'a self,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = Result<Vec<ModelCatalogItem>, BackendError>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    #[tokio::test]
    async fn unrelated_backend_error_does_not_reconstruct_or_retry() {
        let backend = UnrelatedContinuationErrorBackend::default();
        let calls = backend.calls.clone();
        let agent = Agent::new(backend, tools());
        let mut request = AgentRequest::new(
            SessionId::new("session-no-retry").unwrap(),
            TurnId::new("turn-no-retry").unwrap(),
            "continue",
        );
        request.continuation =
            Some(OpaqueContinuation(json!({"previous_response_id": "resp_keep"})));
        let (sender, _receiver) = mpsc::channel(8);
        let result = agent.run(request, sender, CancellationToken::new()).await;
        assert!(matches!(
            result,
            Err(AgentError::Backend(BackendError::Operation { retryable: true, .. }))
        ));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn tool_results_are_compacted_into_session_context() {
        let agent = Agent::new(
            crate::FakeBackend::new("done")
                .with_tool_call("read", json!({"files": [{"path": "src/lib.rs"}]})),
            tools(),
        );
        let (sender, _receiver) = mpsc::channel(8);
        let result = agent
            .run(
                AgentRequest::new(
                    SessionId::new("session-compact").unwrap(),
                    TurnId::new("turn-compact").unwrap(),
                    "inspect",
                ),
                sender,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(result.context.tool_summaries.len(), 1);
        assert_eq!(result.context.tool_summaries[0].tool, "read");
        assert!(result.context.tool_summaries[0].summary.as_str().contains("read"));
    }

    #[tokio::test]
    async fn cancelling_a_turn_waiting_on_a_tool_returns_control() {
        let agent = Arc::new(Agent::new(
            crate::FakeBackend::default().with_tool_call("read", json!({"files": []})),
            SlowTools,
        ));
        let cancellation = CancellationToken::new();
        let task_cancellation = cancellation.clone();
        let (sender, _receiver) = mpsc::channel(16);
        let task_agent = Arc::clone(&agent);
        let task = tokio::spawn(async move {
            task_agent
                .run(
                    AgentRequest::new(
                        SessionId::new("session-tool-cancel").unwrap(),
                        TurnId::new("turn-tool-cancel").unwrap(),
                        "read",
                    ),
                    sender,
                    task_cancellation,
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        cancellation.cancel();
        assert!(matches!(task.await.unwrap(), Err(AgentError::Cancelled)));
    }
}
