use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aether_core::{
    AgentEvent, BoundedText, ContextSnapshot, DEFAULT_MAX_OUTPUT_BYTES, ModelEvent, ModelRequest,
    ObservedFileState, OpaqueContinuation, SessionId, StepId, ToolCallId, ToolExecutor,
    ToolInvocation, ToolResult, TurnId, WorkspacePath,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use thiserror::Error;
use tokio::sync::mpsc;

use crate::context::{ContextEngine, capture_git_snapshot};
use crate::scheduler::{ToolCallGate, execute_tool_calls_with_gate};
use crate::{
    AutonomousCodingPolicy, BackendError, CancellationToken, ModelBackend, NoPermissionBroker,
    PermissionBroker,
};

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
    /// Collect structured execution metrics for this turn.
    #[serde(default)]
    pub metrics: bool,
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
            metrics: false,
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
    /// Structured execution metrics, present only when requested.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<AgentMetrics>,
}

/// Bounded, machine-readable accounting for one agent turn.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMetrics {
    pub model_steps: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub requested_tool_calls: u64,
    pub executed_tool_calls: u64,
    pub prevented_calls: u64,
    pub context_bytes: u64,
    pub bytes_read: u64,
    pub verification_attempts: u64,
    /// Smallest verification scope selected by the deterministic policy, when known.
    pub verification_scope: Option<String>,
    pub provider_wait_ms: u64,
    pub local_execution_ms: u64,
}

struct MetricsState {
    value: AgentMetrics,
    started: Instant,
    provider_wait: Duration,
}

impl MetricsState {
    fn new() -> Self {
        Self {
            value: AgentMetrics::default(),
            started: Instant::now(),
            provider_wait: Duration::ZERO,
        }
    }

    fn finish(mut self) -> AgentMetrics {
        self.value.provider_wait_ms = duration_ms(self.provider_wait);
        self.value.local_execution_ms =
            duration_ms(self.started.elapsed().saturating_sub(self.provider_wait));
        self.value
    }
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
        let mut metrics = request.metrics.then(MetricsState::new);
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
        let mut policy = AutonomousCodingPolicy::new();
        self.with_context(|engine| policy.refresh_candidates(engine));
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
            if let Some(metrics) = &mut metrics {
                metrics.value.model_steps = metrics.value.model_steps.saturating_add(1);
                metrics.value.context_bytes = metrics.value.context_bytes.saturating_add(
                    serde_json::to_vec(&model_request).map_or(u64::MAX, |value| value.len() as u64),
                );
            }
            let provider_started = metrics.as_ref().map(|_| Instant::now());
            let backend_future = self.backend.stream_step(model_request);
            let backend_result = tokio::select! {
                _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                result = backend_future => result,
            };
            if let (Some(metrics), Some(started)) = (&mut metrics, provider_started) {
                metrics.provider_wait = metrics.provider_wait.saturating_add(started.elapsed());
            }
            let mut stream = match backend_result {
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
            };
            let mut tool_calls = Vec::new();
            loop {
                let provider_started = metrics.as_ref().map(|_| Instant::now());
                let event = tokio::select! {
                    _ = cancellation.cancelled() => return Err(AgentError::Cancelled),
                    event = stream.recv() => event,
                };
                if let (Some(metrics), Some(started)) = (&mut metrics, provider_started) {
                    metrics.provider_wait = metrics.provider_wait.saturating_add(started.elapsed());
                }
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
                        if let Some(metrics) = &mut metrics {
                            metrics.value.requested_tool_calls =
                                metrics.value.requested_tool_calls.saturating_add(1);
                        }
                        tool_calls.push((call_id, name, arguments));
                    }
                    ModelEvent::Usage { input_tokens, output_tokens, total_tokens } => {
                        if let Some(metrics) = &mut metrics {
                            add_usage(
                                &mut metrics.value,
                                input_tokens,
                                output_tokens,
                                total_tokens,
                            );
                        }
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
                self.with_context(|engine| {
                    engine.set_continuation(continuation.clone());
                    policy.finish_turn(engine);
                });
                let context = self.current_context();
                let final_metrics = metrics.map(|metrics| {
                    let mut value = metrics.finish();
                    let scope = context.workflow.decision.verification_scope.as_str();
                    if !scope.is_empty() {
                        value.verification_scope = Some(scope.to_owned());
                    }
                    value
                });
                return Ok(AgentRunResult {
                    text: BoundedText::new(accumulated, DEFAULT_MAX_OUTPUT_BYTES),
                    steps,
                    cancelled: false,
                    continuation,
                    context: self.current_context(),
                    metrics: final_metrics,
                });
            }

            let snapshot = self.current_context();
            let gate = AgentToolGate {
                workflow: WorkflowMutationGate { snapshot: snapshot.clone() },
                policy: &policy,
            };
            let completed = execute_tool_calls_with_gate(
                self.tools.as_ref(),
                tool_calls,
                &events,
                &cancellation,
                broker,
                &gate,
            )
            .await?;
            let mut outputs = Vec::with_capacity(completed.len());
            for completed in completed {
                let name = completed.name;
                let tool_input = completed.input;
                let result = completed.result;
                let before = self.current_context().workflow;
                let feedback = self.with_context(|engine| {
                    policy.observe(engine, &name, &tool_input, &result, completed.executed)
                });
                let after = self.current_context().workflow;
                if let Some(metrics) = &mut metrics {
                    if completed.executed {
                        metrics.value.executed_tool_calls =
                            metrics.value.executed_tool_calls.saturating_add(1);
                    } else {
                        metrics.value.prevented_calls =
                            metrics.value.prevented_calls.saturating_add(1);
                    }
                    metrics.value.bytes_read = metrics
                        .value
                        .bytes_read
                        .saturating_add(sum_named_u64(result.data.as_ref(), "bytes_read"));
                    if after.progress.verifications > before.progress.verifications {
                        metrics.value.verification_attempts =
                            metrics.value.verification_attempts.saturating_add(u64::from(
                                after.progress.verifications - before.progress.verifications,
                            ));
                    }
                }
                outputs.push(json!({
                    "type": "function_call_output",
                    "call_id": result.call_id.as_str(),
                    "output": result.output.as_str(),
                    "ok": result.ok,
                    "error": result.error.as_ref().map(|error| json!({
                        "code": error.code.as_str(),
                        "message": error.message.as_str(),
                        "retryable": error.retryable,
                    })),
                }));
                if let Some(feedback) = feedback {
                    outputs.push(json!({"type":"message","role":"developer","content":feedback}));
                }
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

fn add_usage(
    metrics: &mut AgentMetrics,
    input: Option<u32>,
    output: Option<u32>,
    total: Option<u32>,
) {
    metrics.input_tokens = add_optional(metrics.input_tokens, input);
    metrics.output_tokens = add_optional(metrics.output_tokens, output);
    metrics.total_tokens = add_optional(metrics.total_tokens, total);
}

fn add_optional(current: Option<u64>, value: Option<u32>) -> Option<u64> {
    value.map(|value| current.unwrap_or(0).saturating_add(u64::from(value))).or(current)
}

fn sum_named_u64(value: Option<&serde_json::Value>, key: &str) -> u64 {
    match value {
        Some(serde_json::Value::Object(values)) => values.iter().fold(0, |total, (name, value)| {
            total.saturating_add(if name == key {
                value.as_u64().unwrap_or(0)
            } else {
                sum_named_u64(Some(value), key)
            })
        }),
        Some(serde_json::Value::Array(values)) => values
            .iter()
            .fold(0, |total, value| total.saturating_add(sum_named_u64(Some(value), key))),
        _ => 0,
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

struct WorkflowMutationGate {
    snapshot: ContextSnapshot,
}

struct AgentToolGate<'a> {
    workflow: WorkflowMutationGate,
    policy: &'a AutonomousCodingPolicy,
}

impl ToolCallGate for AgentToolGate<'_> {
    fn preflight(&self, invocation: &ToolInvocation) -> Option<ToolResult> {
        self.policy
            .preflight(
                &self.workflow.snapshot,
                invocation.call_id.clone(),
                &invocation.name,
                &invocation.input,
            )
            .or_else(|| self.workflow.preflight(invocation))
    }
}

impl ToolCallGate for WorkflowMutationGate {
    fn preflight(&self, invocation: &ToolInvocation) -> Option<ToolResult> {
        if !matches!(invocation.name.as_str(), "write" | "patch")
            || mutation_targets_inspected(&self.snapshot, &invocation.name, &invocation.input)
        {
            return None;
        }
        Some(ToolResult::failure(
            invocation.call_id.clone(),
            "inspection_required",
            "inspect the relevant workspace code before modifying it",
            true,
            DEFAULT_MAX_OUTPUT_BYTES,
        ))
    }
}

fn mutation_targets_inspected(
    snapshot: &ContextSnapshot,
    name: &str,
    input: &serde_json::Value,
) -> bool {
    match name {
        "write" => input.get("path").and_then(serde_json::Value::as_str).is_some_and(|path| {
            target_is_currently_inspected(snapshot, path)
                || (input.get("create_only").and_then(serde_json::Value::as_bool) == Some(true)
                    && new_target_has_discovered_parent(snapshot, path))
        }),
        "patch" => input.get("files").and_then(serde_json::Value::as_array).is_some_and(|files| {
            !files.is_empty()
                && files.iter().all(|file| {
                    file.get("path")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|path| target_is_currently_inspected(snapshot, path))
                })
        }),
        _ => true,
    }
}

fn target_is_currently_inspected(snapshot: &ContextSnapshot, path: &str) -> bool {
    WorkspacePath::new(path).is_ok()
        && snapshot.workflow.relevant_files.iter().any(|relevant| relevant == path)
        && snapshot.inspected.iter().any(|file| {
            file.path == path
                && file.last_state == ObservedFileState::Present
                && !file.stale
                && file.content_hash.is_some()
        })
}

fn new_target_has_discovered_parent(snapshot: &ContextSnapshot, path: &str) -> bool {
    let parent = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new("."));
    snapshot.workflow.relevant_files.iter().any(|relevant| {
        std::path::Path::new(relevant).parent().is_some_and(|candidate| candidate == parent)
    })
}

fn assemble_user_input(prompt: &str, context_packet: Option<String>) -> serde_json::Value {
    let content = match context_packet {
        Some(packet) if !packet.trim().is_empty() => format!("{packet}\n\n{prompt}"),
        _ => prompt.to_owned(),
    };
    json!([{"type":"message","role":"user","content": content}])
}

pub(crate) async fn send_event(
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
        InspectedFile, ModelEvent, ModelRequest, OpaqueContinuation, PermissionClass,
        PermissionDecision, ToolDefinition, ToolExecutionContext, ToolInvocation, ToolResult,
        WorkflowVerification,
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

    #[test]
    fn mutation_gate_requires_current_inspection_before_execution() {
        let call_id = ToolCallId::new("workflow-gate").unwrap();
        let invocation = ToolInvocation {
            call_id: call_id.clone(),
            name: "write".to_owned(),
            input: json!({"path": "src/lib.rs"}),
        };
        let gate = WorkflowMutationGate { snapshot: ContextSnapshot::new("/workspace", None) };
        let blocked = gate.preflight(&invocation).expect("uninspected mutation must be blocked");
        assert_eq!(blocked.error.as_ref().unwrap().code, "inspection_required");

        let mut inspected = ContextSnapshot::new("/workspace", None);
        inspected.inspected.push(InspectedFile {
            path: "src/lib.rs".to_owned(),
            content_hash: Some("current".to_owned()),
            ranges: Vec::new(),
            last_state: ObservedFileState::Present,
            stale: false,
        });
        inspected.workflow.record_relevant_file("src/lib.rs");
        let gate = WorkflowMutationGate { snapshot: inspected };
        assert!(gate.preflight(&invocation).is_none());
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
    async fn metrics_account_requested_executed_and_prevented_calls() {
        let backend =
            crate::FakeBackend::new("completed").with_tool_call("read", json!({"files": []}));
        let agent = Agent::new(backend, tools());
        let mut request = AgentRequest::new(
            SessionId::new("metrics-accounting").unwrap(),
            TurnId::new("metrics-turn").unwrap(),
            "inspect",
        );
        request.metrics = true;
        let (sender, _receiver) = mpsc::channel(32);
        let result = agent.run(request, sender, CancellationToken::new()).await.unwrap();
        let metrics = result.metrics.unwrap();
        assert_eq!(metrics.model_steps, 2);
        assert_eq!(metrics.requested_tool_calls, 1);
        assert_eq!(metrics.executed_tool_calls, 1);
        assert_eq!(metrics.prevented_calls, 0);
        assert!(metrics.context_bytes > 0);
    }

    #[test]
    fn metrics_aggregate_cached_usage_and_preserve_missing_provider_usage() {
        let mut metrics = AgentMetrics::default();
        add_usage(&mut metrics, Some(10), Some(3), Some(13));
        add_usage(&mut metrics, Some(4), Some(2), Some(6));
        add_usage(&mut metrics, None, None, None);
        assert_eq!(metrics.input_tokens, Some(14));
        assert_eq!(metrics.output_tokens, Some(5));
        assert_eq!(metrics.total_tokens, Some(19));

        let mut missing = AgentMetrics::default();
        add_usage(&mut missing, None, None, None);
        assert_eq!(missing.input_tokens, None);
        assert_eq!(missing.output_tokens, None);
        assert_eq!(missing.total_tokens, None);
    }

    #[test]
    fn metrics_count_nested_read_bytes_and_verification_attempts() {
        assert_eq!(
            sum_named_u64(
                Some(&json!({"files": [{"bytes_read": 7}, {"bytes_read": 11}]})),
                "bytes_read",
            ),
            18
        );
        let before = 2_u16;
        let after = 4_u16;
        assert_eq!(u64::from(after.saturating_sub(before)), 2);
    }

    #[tokio::test]
    async fn metrics_disabled_avoids_collection_and_cancellation_remains_terminal() {
        let agent = Agent::new(crate::FakeBackend::new("done"), tools());
        let request = AgentRequest::new(
            SessionId::new("metrics-disabled").unwrap(),
            TurnId::new("metrics-disabled-turn").unwrap(),
            "finish",
        );
        let (sender, _receiver) = mpsc::channel(8);
        let result = agent.run(request, sender, CancellationToken::new()).await.unwrap();
        assert!(result.metrics.is_none());

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let request = AgentRequest::new(
            SessionId::new("metrics-cancelled").unwrap(),
            TurnId::new("metrics-cancelled-turn").unwrap(),
            "cancel",
        );
        let (sender, _receiver) = mpsc::channel(8);
        assert!(matches!(
            agent.run(request, sender, cancellation).await,
            Err(AgentError::Cancelled)
        ));
    }

    #[derive(Clone, Default)]
    struct WorkflowBackend {
        steps: Arc<AtomicUsize>,
    }

    impl ModelBackend for WorkflowBackend {
        fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a> {
            let step = self.steps.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                let (sender, receiver) = mpsc::channel(4);
                match step {
                    0 => sender
                        .send(Ok(ModelEvent::ToolCall {
                            call_id: ToolCallId::new("workflow-read-1").unwrap(),
                            name: "read".to_owned(),
                            arguments: json!({
                                "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
                            }),
                        }))
                        .await
                        .unwrap(),
                    1 => sender
                        .send(Ok(ModelEvent::ToolCall {
                            call_id: ToolCallId::new("workflow-write-1").unwrap(),
                            name: "write".to_owned(),
                            arguments: json!({"path": "src/lib.rs", "content": "new"}),
                        }))
                        .await
                        .unwrap(),
                    2 => sender
                        .send(Ok(ModelEvent::ToolCall {
                            call_id: ToolCallId::new("workflow-read-2").unwrap(),
                            name: "read".to_owned(),
                            arguments: json!({
                                "files": [{"path": "src/lib.rs", "start_line": 1, "end_line": 1}]
                            }),
                        }))
                        .await
                        .unwrap(),
                    _ => sender
                        .send(Ok(ModelEvent::TextDelta { text: "complete".to_owned() }))
                        .await
                        .unwrap(),
                }
                sender.send(Ok(ModelEvent::Done { continuation: None })).await.unwrap();
                let _ = request;
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

    struct WorkflowTools;

    impl ToolExecutor for WorkflowTools {
        fn definitions(&self) -> &[ToolDefinition] {
            static DEFINITIONS: std::sync::OnceLock<Vec<ToolDefinition>> =
                std::sync::OnceLock::new();
            DEFINITIONS.get_or_init(|| {
                vec![
                    ToolDefinition {
                        name: "read".to_owned(),
                        description: "workflow read".to_owned(),
                        input_schema: json!({"type": "object"}),
                        permission: PermissionClass::ReadOnly,
                    },
                    ToolDefinition {
                        name: "write".to_owned(),
                        description: "workflow write".to_owned(),
                        input_schema: json!({"type": "object"}),
                        permission: PermissionClass::WorkspaceWrite,
                    },
                ]
            })
        }

        fn permission_request(&self, _: &ToolInvocation) -> Option<aether_core::PermissionRequest> {
            None
        }

        fn execute<'a>(
            &'a self,
            invocation: ToolInvocation,
            _: ToolExecutionContext,
        ) -> ToolFuture<'a> {
            Box::pin(async move {
                match invocation.name.as_str() {
                    "read" => ToolResult::success_json(
                        invocation.call_id,
                        json!({
                            "files": [{
                                "path": "src/lib.rs",
                                "content_hash": "current",
                                "lines": [{"number": 1, "text": "fn main() {}"}]
                            }]
                        }),
                        4096,
                    ),
                    "write" => ToolResult::success_json(
                        invocation.call_id,
                        json!({"path": "src/lib.rs", "hash": "current"}),
                        4096,
                    ),
                    _ => ToolResult::failure(
                        invocation.call_id,
                        "unknown",
                        "unknown workflow tool",
                        false,
                        4096,
                    ),
                }
            })
        }
    }

    #[tokio::test]
    async fn normal_coding_flow_completes_after_focused_verification() {
        let agent = Agent::new(WorkflowBackend::default(), WorkflowTools);
        let request = AgentRequest::new(
            SessionId::new("workflow-normal").unwrap(),
            TurnId::new("workflow-turn").unwrap(),
            "update lib",
        );
        let (sender, _receiver) = mpsc::channel(32);
        let result = agent.run(request, sender, CancellationToken::new()).await.unwrap();
        assert_eq!(result.context.workflow.phase, aether_core::WorkflowPhase::Complete);
        assert_eq!(result.context.workflow.verification, WorkflowVerification::Passed);
        assert_eq!(result.context.workflow.progress.mutations, 1);
        assert!(result.context.workflow.unresolved_failures.is_empty());
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
            let mut request = AgentRequest::new(
                SessionId::new("session-permission-event").unwrap(),
                TurnId::new("turn-permission-event").unwrap(),
                "write",
            );
            request.context_seed = Some(ContextSnapshot {
                workspace_root: String::new(),
                current_task: BoundedText::default(),
                inspected: vec![InspectedFile {
                    path: "test.txt".to_owned(),
                    content_hash: Some("current".to_owned()),
                    ranges: Vec::new(),
                    last_state: ObservedFileState::Present,
                    stale: false,
                }],
                modified: Vec::new(),
                excerpts: Vec::new(),
                tool_summaries: Vec::new(),
                git: None,
                continuation: None,
                model: None,
                workspace_changed: false,
                workflow: aether_core::WorkflowState::new(),
            });
            request.context_seed.as_mut().unwrap().workflow.record_relevant_file("test.txt");
            task_agent
                .run_with_broker(request, sender, CancellationToken::new(), &task_broker)
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
        request.metrics = true;
        let (sender, _receiver) = mpsc::channel(8);
        let result = agent.run(request, sender, CancellationToken::new()).await.unwrap();
        assert_eq!(result.text.as_str(), "reconstructed");
        assert!(result.continuation.is_none());
        assert_eq!(result.metrics.unwrap().model_steps, 2);
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
