#![deny(unsafe_code)]

//! The only AETHER crate that directly consumes the verified Rainy SDK.

use std::{
    collections::{HashMap, HashSet},
    env,
    sync::Arc,
    time::{Duration, Instant},
};

use aether_agent::{BackendError, BackendFuture, ModelBackend, ModelCatalogItem};
use aether_core::{ModelEvent, ModelRequest, OpaqueContinuation, ToolCallId, ToolDefinition};
use futures_util::StreamExt;
use rainy_sdk::{
    CapabilityFlag, ModelCatalogItem as RainyModelCatalogItem, RainyCapabilities, RainyClient,
    RainyError, ResponsesEventType, ResponsesRequest, ResponsesStreamEvent,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

const CATALOG_TTL: Duration = Duration::from_secs(300);
const STREAM_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
struct CatalogCache {
    refreshed_at: Instant,
    models: Vec<ModelCatalogItem>,
    views: Vec<ModelView>,
}

#[derive(Clone)]
pub struct RainyBackend {
    client: Arc<RainyClient>,
    catalog: Arc<Mutex<Option<CatalogCache>>>,
}

/// A three-valued capability used when the provider did not make a claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    Unknown,
}

impl CapabilityState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Unknown => "unknown",
        }
    }
}

/// Provider access state for a catalog entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelAvailability {
    Available,
    OrganizationDenied,
    PrivacyIncompatible,
    Unknown,
}

impl ModelAvailability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::OrganizationDenied => "organization_denied",
            Self::PrivacyIncompatible => "privacy_incompatible",
            Self::Unknown => "unknown",
        }
    }
}

/// Stable, terminal-safe model metadata shared by the CLI model views.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ModelView {
    pub id: String,
    pub display_name: String,
    pub context_length: Option<u64>,
    pub model_context_length: Option<u32>,
    pub plan_context_length: Option<u64>,
    pub effective_context_length: Option<u64>,
    pub tier: Option<String>,
    pub billing_class: Option<String>,
    pub tools: CapabilityState,
    pub reasoning: CapabilityState,
    pub reasoning_modes: Vec<String>,
    pub reasoning_values: Vec<String>,
    pub reasoning_parameters: Vec<String>,
    pub thinking_budget_min: Option<i32>,
    pub thinking_budget_max: Option<i32>,
    pub thinking_budget_dynamic_value: Option<i32>,
    pub thinking_budget_disable_value: Option<i32>,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
    pub modalities_reported: bool,
    pub supported_parameters: Vec<String>,
    pub availability: ModelAvailability,
    pub organization_enabled: Option<bool>,
    pub privacy_compatible: Option<bool>,
    pub privacy_mode: Option<String>,
    pub policy_status: Option<String>,
    pub disclosure_required: bool,
    pub policy_notice: Option<String>,
    pub training_with_inputs: Option<bool>,
    pub zdr_available: Option<bool>,
    pub retention_days: Option<u32>,
}

impl ModelView {
    fn from_catalog(item: &RainyModelCatalogItem) -> Option<Self> {
        let id = bounded_identifier(&item.id)?;
        let display_name = bounded_text(
            item.name.as_deref().filter(|name| !name.trim().is_empty()).unwrap_or(&item.id),
            160,
        );
        let (input_modalities, output_modalities, modalities_reported) =
            modalities(item.architecture.as_ref());
        let supported_parameters =
            bounded_values(item.supported_parameters.as_deref().unwrap_or_default(), 64, 80);
        let (reasoning_modes, reasoning_values, reasoning_parameters) =
            reasoning_metadata(item.supported_parameters.as_deref());
        Some(Self {
            id,
            display_name,
            context_length: item.context_length.map(u64::from),
            model_context_length: item.context_length,
            plan_context_length: None,
            effective_context_length: None,
            tier: None,
            billing_class: None,
            tools: tools_capability(
                item.rainy_capabilities.as_ref(),
                item.supported_parameters.as_deref(),
            ),
            reasoning: reasoning_capability(
                item.rainy_capabilities.as_ref(),
                item.supported_parameters.as_deref(),
            ),
            reasoning_modes,
            reasoning_values,
            reasoning_parameters,
            thinking_budget_min: None,
            thinking_budget_max: None,
            thinking_budget_dynamic_value: None,
            thinking_budget_disable_value: None,
            input_modalities,
            output_modalities,
            modalities_reported,
            supported_parameters,
            availability: ModelAvailability::Unknown,
            organization_enabled: None,
            privacy_compatible: None,
            privacy_mode: None,
            policy_status: None,
            disclosure_required: false,
            policy_notice: None,
            training_with_inputs: None,
            zdr_available: None,
            retention_days: None,
        })
    }

    pub fn selectable(&self) -> bool {
        !matches!(
            self.availability,
            ModelAvailability::OrganizationDenied | ModelAvailability::PrivacyIncompatible
        ) && self.tools != CapabilityState::Unsupported
            && (!self.modalities_reported
                || self.input_modalities.iter().any(|value| value == "text"))
    }

    pub fn has_unknown_metadata(&self) -> bool {
        self.availability == ModelAvailability::Unknown
            || self.tools == CapabilityState::Unknown
            || self.reasoning == CapabilityState::Unknown
            || !self.modalities_reported
    }

    pub fn detail(&self) -> String {
        let context = self
            .effective_context_length
            .or(self.context_length)
            .map(format_context)
            .unwrap_or_else(|| "context ?".to_owned());
        let tier = self.tier.as_deref().unwrap_or("tier ?");
        let reasoning_detail = if self.reasoning_modes.is_empty() {
            self.reasoning.as_str().to_owned()
        } else {
            format!("{} ({})", self.reasoning.as_str(), self.reasoning_modes.join(","))
        };
        let budget_detail = match (self.thinking_budget_min, self.thinking_budget_max) {
            (Some(min), Some(max)) => format!(" · budget {min}..{max}"),
            _ => String::new(),
        };
        format!(
            "{context} · {tier} · tools {} · reasoning {reasoning_detail}{budget_detail} · access {}{}",
            self.tools.as_str(),
            self.availability.as_str(),
            if self.disclosure_required { " · disclosure" } else { "" }
        )
    }
}

impl RainyBackend {
    pub fn from_env() -> Result<Self, BackendError> {
        let key = env::var_os("RAINY_API_KEY")
            .ok_or(BackendError::CredentialsUnavailable)?
            .into_string()
            .map_err(|_| BackendError::CredentialsUnavailable)?;
        Self::from_api_key(key)
    }

    pub fn from_api_key(key: String) -> Result<Self, BackendError> {
        if key.is_empty() || key.len() > 16 * 1024 || key.chars().any(char::is_control) {
            return Err(BackendError::CredentialsUnavailable);
        }
        let client =
            RainyClient::with_api_key(key).map_err(|error| map_rainy_error(&error, false))?;
        Ok(Self { client: Arc::new(client), catalog: Arc::new(Mutex::new(None)) })
    }

    pub fn new(client: RainyClient) -> Self {
        Self { client: Arc::new(client), catalog: Arc::new(Mutex::new(None)) }
    }

    async fn refresh_catalog(&self) -> Result<(), BackendError> {
        {
            let cache = self.catalog.lock().await;
            if let Some(cache) = cache.as_ref()
                && cache.refreshed_at.elapsed() < CATALOG_TTL
            {
                return Ok(());
            }
        }
        let catalog = self
            .client
            .get_models_catalog()
            .await
            .map_err(|error| map_rainy_error(&error, false))?;
        let (models, views) = normalize_catalog(catalog);
        *self.catalog.lock().await =
            Some(CatalogCache { refreshed_at: Instant::now(), models, views });
        Ok(())
    }

    async fn catalog(&self) -> Result<Vec<ModelCatalogItem>, BackendError> {
        self.refresh_catalog().await?;
        Ok(self.catalog.lock().await.as_ref().map(|cache| cache.models.clone()).unwrap_or_default())
    }

    pub async fn discover_model_views(&self) -> Result<Vec<ModelView>, BackendError> {
        self.refresh_catalog().await?;
        Ok(self.catalog.lock().await.as_ref().map(|cache| cache.views.clone()).unwrap_or_default())
    }

    async fn model_for(&self, requested: Option<String>) -> Result<String, BackendError> {
        select_model(requested)
    }

    fn request(
        &self,
        request: &ModelRequest,
        model: String,
    ) -> Result<ResponsesRequest, BackendError> {
        let tools = request.tools.iter().map(tool_definition).collect::<Vec<_>>();
        let mut rainy_request =
            ResponsesRequest::new(model, request.input.clone()).with_stream(true).with_tools(tools);
        if let Some(continuation) = request.continuation.as_ref() {
            rainy_request =
                rainy_request.with_previous_response_id(previous_response_id(continuation)?);
        }
        Ok(rainy_request)
    }
}

fn normalize_catalog(
    catalog: Vec<RainyModelCatalogItem>,
) -> (Vec<ModelCatalogItem>, Vec<ModelView>) {
    let mut normalized = catalog
        .into_iter()
        .filter_map(|item| {
            let view = ModelView::from_catalog(&item)?;
            let capabilities = serde_json::to_value(&view).unwrap_or(Value::Null);
            let model = ModelCatalogItem {
                id: view.id.clone(),
                display_name: Some(view.display_name.clone()),
                context_window: item.context_length,
                capabilities,
            };
            Some((model, view))
        })
        .collect::<Vec<_>>();
    normalized.sort_by(|left, right| left.1.id.cmp(&right.1.id));
    normalized.into_iter().unzip()
}

fn bounded_identifier(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > 256
        || value.chars().any(char::is_control)
        || value.chars().any(char::is_whitespace)
    {
        return None;
    }
    Some(value.to_owned())
}

fn bounded_text(value: &str, max_bytes: usize) -> String {
    let mut output = String::new();
    for character in value.chars() {
        let character = if character.is_control() { '�' } else { character };
        if output.len().saturating_add(character.len_utf8()) > max_bytes {
            break;
        }
        output.push(character);
    }
    output
}

fn bounded_values(values: &[String], max_items: usize, max_bytes: usize) -> Vec<String> {
    let mut output = values
        .iter()
        .filter_map(|value| {
            let value = bounded_text(value, max_bytes);
            (!value.is_empty()).then_some(value)
        })
        .take(max_items)
        .collect::<Vec<_>>();
    output.sort_by_key(|value| value.to_lowercase());
    output.dedup();
    output
}

fn modalities(
    architecture: Option<&rainy_sdk::ModelArchitecture>,
) -> (Vec<String>, Vec<String>, bool) {
    architecture.map_or_else(
        || (Vec::new(), Vec::new(), false),
        |architecture| {
            (
                bounded_modalities(&architecture.input_modalities),
                bounded_modalities(&architecture.output_modalities),
                true,
            )
        },
    )
}

fn bounded_modalities(values: &[String]) -> Vec<String> {
    bounded_values(values, 16, 32).into_iter().map(|value| value.to_lowercase()).collect()
}

fn capability_flag(flag: Option<&CapabilityFlag>) -> CapabilityState {
    match flag {
        Some(CapabilityFlag::Bool(true)) => CapabilityState::Supported,
        Some(CapabilityFlag::Bool(false)) => CapabilityState::Unsupported,
        Some(CapabilityFlag::Text(value)) if value.eq_ignore_ascii_case("unknown") => {
            CapabilityState::Unknown
        }
        Some(CapabilityFlag::Text(_)) => CapabilityState::Unknown,
        None => CapabilityState::Unknown,
    }
}

fn parameters_include(parameters: Option<&[String]>, names: &[&str]) -> Option<bool> {
    parameters.map(|parameters| {
        parameters
            .iter()
            .any(|parameter| names.iter().any(|name| parameter.eq_ignore_ascii_case(name)))
    })
}

fn tools_capability(
    v1: Option<&RainyCapabilities>,
    parameters: Option<&[String]>,
) -> CapabilityState {
    if let Some(capabilities) = v1 {
        return capability_flag(capabilities.tools.as_ref());
    }
    parameters_include(parameters, &["tools"]).map_or(CapabilityState::Unknown, |supported| {
        if supported { CapabilityState::Supported } else { CapabilityState::Unsupported }
    })
}

fn reasoning_capability(
    v1: Option<&RainyCapabilities>,
    parameters: Option<&[String]>,
) -> CapabilityState {
    if let Some(capabilities) = v1 {
        return capability_flag(capabilities.reasoning.as_ref());
    }
    parameters_include(parameters, &["reasoning", "reasoning.effort", "thinking"]).map_or(
        CapabilityState::Unknown,
        |supported| {
            if supported { CapabilityState::Supported } else { CapabilityState::Unsupported }
        },
    )
}

fn reasoning_metadata(parameters: Option<&[String]>) -> (Vec<String>, Vec<String>, Vec<String>) {
    let parameters = bounded_values(parameters.unwrap_or_default(), 64, 80);
    let reasoning_parameters = parameters
        .iter()
        .filter(|parameter| {
            parameter.eq_ignore_ascii_case("reasoning")
                || parameter.eq_ignore_ascii_case("reasoning_effort")
                || parameter.eq_ignore_ascii_case("reasoning.effort")
                || parameter.eq_ignore_ascii_case("reasoning_max_tokens")
                || parameter.eq_ignore_ascii_case("reasoning.max_tokens")
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut modes = Vec::new();
    if reasoning_parameters.iter().any(|parameter| {
        parameter.eq_ignore_ascii_case("reasoning_effort")
            || parameter.eq_ignore_ascii_case("reasoning.effort")
    }) {
        modes.push("effort".to_owned());
    }
    if reasoning_parameters.iter().any(|parameter| {
        parameter.eq_ignore_ascii_case("reasoning_max_tokens")
            || parameter.eq_ignore_ascii_case("reasoning.max_tokens")
    }) {
        modes.push("budget".to_owned());
    }
    if modes.is_empty() && !reasoning_parameters.is_empty() {
        modes.push("reasoning".to_owned());
    }
    (modes, Vec::new(), reasoning_parameters)
}

fn format_context(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M context", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}k context", tokens / 1_000)
    } else {
        format!("{tokens} context")
    }
}

fn select_model(requested: Option<String>) -> Result<String, BackendError> {
    if let Some(model) = requested {
        if model.is_empty()
            || model.len() > 256
            || model.chars().any(char::is_control)
            || model.chars().any(char::is_whitespace)
        {
            return Err(BackendError::Mapping {
                message: "model identifier must be non-empty, bounded, and free of controls"
                    .to_owned(),
            });
        }
        return Ok(model);
    }
    Err(BackendError::FeatureUnavailable {
        feature: "no model selected; pass --model <id> or set AETHER_MODEL".to_owned(),
    })
}

impl ModelBackend for RainyBackend {
    fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a> {
        Box::pin(async move {
            let model = self.model_for(request.model.clone()).await?;
            let continuation_attached = request.continuation.is_some();
            let rainy_request = self.request(&request, model)?;
            let stream = self
                .client
                .create_response_stream(rainy_request)
                .await
                .map_err(|error| map_rainy_error(&error, continuation_attached))?;
            let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
            tokio::spawn(async move {
                adapt_stream(stream, sender, continuation_attached).await;
            });
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
        Box::pin(async move { self.catalog().await })
    }
}

fn tool_definition(definition: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "name": definition.name,
        "description": definition.description,
        "parameters": definition.input_schema
    })
}

async fn adapt_stream(
    mut stream: std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<ResponsesStreamEvent, RainyError>> + Send>,
    >,
    sender: mpsc::Sender<Result<ModelEvent, BackendError>>,
    continuation_attached: bool,
) {
    let mut tool_calls: HashMap<String, (String, String)> = HashMap::new();
    let mut completed_tool_calls = HashSet::new();
    loop {
        tokio::select! {
            _ = sender.closed() => return,
            event = stream.next() => {
                let Some(event) = event else {
                    let _ = sender.send(Err(BackendError::IncompleteStream {
                        message: "Rainy response stream ended before a terminal response event".to_owned(),
                    })).await;
                    return;
                };
                let event = match event {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = sender.send(Err(map_rainy_error(&error, continuation_attached))).await;
                        return;
                    }
                };
                let is_completed = event.kind == ResponsesEventType::ResponseCompleted;
                if is_completed && !tool_calls.is_empty() {
                    let _ = sender.send(Err(BackendError::IncompleteStream {
                        message: "Rainy response completed with an incomplete tool call".to_owned(),
                    })).await;
                    return;
                }
                match map_event(event, &mut tool_calls, &mut completed_tool_calls) {
                    Ok(events) => {
                        for event in events {
                            if sender.send(Ok(event)).await.is_err() {
                                return;
                            }
                        }
                        if is_completed {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(error)).await;
                        return;
                    }
                }
            }
        }
    }
}

fn map_event(
    event: ResponsesStreamEvent,
    tool_calls: &mut HashMap<String, (String, String)>,
    completed_tool_calls: &mut HashSet<String>,
) -> Result<Vec<ModelEvent>, BackendError> {
    match event.kind {
        ResponsesEventType::OutputTextDelta => Ok(event
            .text_delta()
            .map(|text| vec![ModelEvent::TextDelta { text: text.to_owned() }])
            .unwrap_or_default()),
        ResponsesEventType::FunctionCallArgumentsDelta => {
            let call_id = event_string(&event.data, &["call_id", "item_id", "id"])?;
            let entry = tool_calls.entry(call_id).or_insert_with(|| (String::new(), String::new()));
            if let Some(delta) = event.data.get("delta").and_then(Value::as_str) {
                entry.1.push_str(delta);
            }
            Ok(Vec::new())
        }
        ResponsesEventType::FunctionCallArgumentsDone => {
            let call_id = event_string(&event.data, &["call_id", "item_id", "id"])?;
            if completed_tool_calls.contains(&call_id) {
                return Ok(Vec::new());
            }
            let name = event
                .data
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| tool_calls.get(&call_id).map(|value| value.0.clone()))
                .ok_or_else(|| BackendError::Mapping {
                    message: "Rainy function call omitted its name".to_owned(),
                })?;
            let arguments = event
                .data
                .get("arguments")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| tool_calls.get(&call_id).map(|value| value.1.clone()))
                .unwrap_or_else(|| "{}".to_owned());
            let arguments =
                serde_json::from_str(&arguments).map_err(|_| BackendError::Mapping {
                    message: "Rainy function call arguments were not valid JSON".to_owned(),
                })?;
            tool_calls.remove(&call_id);
            completed_tool_calls.insert(call_id.clone());
            Ok(vec![ModelEvent::ToolCall {
                call_id: ToolCallId::new(call_id)
                    .map_err(|error| BackendError::Mapping { message: error.to_string() })?,
                name,
                arguments,
            }])
        }
        ResponsesEventType::OutputItemDone | ResponsesEventType::OutputItemAdded => {
            let item = event.data.get("item").unwrap_or(&event.data);
            if item.get("type").and_then(Value::as_str) != Some("function_call") {
                return Ok(Vec::new());
            }
            let call_id = event_string(item, &["call_id", "id"])?;
            if completed_tool_calls.contains(&call_id) {
                return Ok(Vec::new());
            }
            let name = event_string(item, &["name"])?;
            let arguments =
                item.get("arguments").and_then(Value::as_str).unwrap_or("{}").to_owned();
            if event.kind == ResponsesEventType::OutputItemAdded {
                tool_calls.insert(call_id, (name, arguments));
                return Ok(Vec::new());
            }
            let arguments =
                serde_json::from_str(&arguments).map_err(|_| BackendError::Mapping {
                    message: "Rainy function call arguments were not valid JSON".to_owned(),
                })?;
            tool_calls.remove(&call_id);
            completed_tool_calls.insert(call_id.clone());
            Ok(vec![ModelEvent::ToolCall {
                call_id: ToolCallId::new(call_id)
                    .map_err(|error| BackendError::Mapping { message: error.to_string() })?,
                name,
                arguments,
            }])
        }
        ResponsesEventType::ResponseCompleted => {
            let response = event.data.get("response").unwrap_or(&event.data);
            let id =
                response.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()).ok_or_else(
                    || BackendError::Mapping {
                        message: "Rainy completed response omitted its response id".to_owned(),
                    },
                )?;
            let mut output = Vec::new();
            if let Some(usage) = response.get("usage") {
                output.push(ModelEvent::Usage {
                    input_tokens: usage
                        .get("input_tokens")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    output_tokens: usage
                        .get("output_tokens")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                    total_tokens: usage
                        .get("total_tokens")
                        .and_then(Value::as_u64)
                        .map(|v| v as u32),
                });
            }
            output.push(ModelEvent::Done {
                continuation: Some(OpaqueContinuation(json!({"previous_response_id": id}))),
            });
            Ok(output)
        }
        ResponsesEventType::ResponseFailed => {
            let event_name =
                event.event.as_deref().or_else(|| event.data.get("type").and_then(Value::as_str));
            if event_name.is_some_and(|name| name.eq_ignore_ascii_case("response.incomplete")) {
                Err(BackendError::ResponseIncomplete { message: terminal_message(&event.data) })
            } else {
                Err(BackendError::ResponseFailed { message: terminal_message(&event.data) })
            }
        }
        ResponsesEventType::Error => Err(BackendError::Operation {
            message: terminal_message(&event.data),
            retryable: false,
        }),
        _ => Ok(Vec::new()),
    }
}

fn terminal_message(event: &Value) -> String {
    let response = event.get("response").unwrap_or(event);
    let error = response.get("error").or_else(|| event.get("error"));
    let code = error
        .and_then(|value| value.get("code"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let message = error
        .and_then(|value| value.get("message"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let raw = match (code, message) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.to_owned(),
        (None, Some(message)) => message.to_owned(),
        (None, None) => response
            .get("incomplete_details")
            .and_then(|value| value.get("reason"))
            .and_then(Value::as_str)
            .map_or_else(
                || "Rainy returned an unsuccessful response event".to_owned(),
                str::to_owned,
            ),
    };
    raw.chars().take(512).collect()
}

fn event_string(event: &Value, keys: &[&str]) -> Result<String, BackendError> {
    keys.iter()
        .find_map(|key| event.get(*key).and_then(Value::as_str))
        .map(str::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BackendError::Mapping {
            message: "Rainy event omitted a required identifier".to_owned(),
        })
}

fn previous_response_id(continuation: &OpaqueContinuation) -> Result<&str, BackendError> {
    continuation
        .0
        .get("previous_response_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BackendError::InvalidContinuation {
            message: "continuation omitted previous_response_id".to_owned(),
        })
}

fn map_rainy_error(error: &RainyError, continuation_attached: bool) -> BackendError {
    if continuation_attached
        && let RainyError::InvalidRequest { code, .. } = error
        && is_continuation_field_code(code)
    {
        return BackendError::InvalidContinuation {
            message: "Rainy rejected previous_response_id".to_owned(),
        };
    }
    let code = error
        .code()
        .filter(|value| {
            !value.is_empty() && value.len() <= 80 && !value.chars().any(char::is_control)
        })
        .map(|value| format!(": {value}"))
        .unwrap_or_default();
    let request_id = error
        .request_id()
        .filter(|value| {
            !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
        })
        .map(|value| format!(" (request_id={value})"))
        .unwrap_or_default();
    BackendError::Operation {
        message: format!("Rainy {} error{code}{request_id}", rainy_error_kind(error)),
        retryable: error.is_retryable(),
    }
}

fn rainy_error_kind(error: &RainyError) -> &'static str {
    match error {
        RainyError::Authentication { .. } => "authentication",
        RainyError::AccessDenied { .. } => "access denied",
        RainyError::RateLimit { .. } => "rate limit",
        RainyError::InsufficientCredits { .. } => "insufficient credits",
        RainyError::Network { .. } | RainyError::NetworkError(_) => "network",
        RainyError::Api { .. } | RainyError::Provider { .. } => "service",
        RainyError::Timeout { .. } => "timeout",
        RainyError::Serialization { .. } => "serialization",
        RainyError::InvalidRequest { .. } | RainyError::ValidationError(_) => "invalid request",
        RainyError::FeatureNotAvailable { .. } => "feature unavailable",
        RainyError::PayloadTooLarge { .. } => "payload too large",
    }
}

fn is_continuation_field_code(code: &str) -> bool {
    matches!(
        code,
        "previous_response_id" | "INVALID_PREVIOUS_RESPONSE_ID" | "invalid_previous_response_id"
    )
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::atomic::{AtomicUsize, Ordering},
        thread,
    };

    use super::*;
    use aether_core::{SessionId, StepId, TurnId};
    use futures_util::stream;
    use rainy_sdk::ResponsesEvent;

    async fn collect(events: Vec<Value>) -> Vec<Result<ModelEvent, BackendError>> {
        let stream = Box::pin(stream::iter(
            events
                .into_iter()
                .map(|value| {
                    Ok::<ResponsesStreamEvent, RainyError>(ResponsesEvent::new(None, value))
                })
                .collect::<Vec<_>>(),
        ));
        let (sender, mut receiver) = mpsc::channel(16);
        adapt_stream(stream, sender, false).await;
        let mut output = Vec::new();
        while let Ok(event) = receiver.try_recv() {
            output.push(event);
        }
        output
    }

    fn test_model_request() -> ModelRequest {
        ModelRequest {
            session_id: SessionId::new("session-1").unwrap(),
            turn_id: TurnId::new("turn-1").unwrap(),
            step_id: StepId::new("step-1").unwrap(),
            model: Some("test-model".to_owned()),
            input: json!("hello"),
            tools: Arc::new(Vec::new()),
            continuation: None,
        }
    }

    fn test_backend(base_url: &str) -> RainyBackend {
        let client = RainyClient::with_config(
            rainy_sdk::AuthConfig::new("test-key")
                .with_api_base_url(base_url)
                .with_timeout(5)
                .with_retry(false),
        )
        .unwrap();
        RainyBackend::new(client)
    }

    fn spawn_stream_server(body: Vec<u8>, chunk_size: usize) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request);
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(headers.as_bytes()).unwrap();
            for chunk in body.chunks(chunk_size.max(1)) {
                if socket.write_all(chunk).is_err() {
                    break;
                }
                if socket.flush().is_err() {
                    break;
                }
            }
        });
        (format!("http://{address}"), handle)
    }

    async fn collect_backend_stream(
        body: Vec<u8>,
        chunk_size: usize,
    ) -> Vec<Result<ModelEvent, BackendError>> {
        let (base_url, handle) = spawn_stream_server(body, chunk_size);
        let backend = test_backend(&base_url);
        let result = backend.stream_step(test_model_request()).await;
        let mut output = Vec::new();
        match result {
            Ok(mut stream) => {
                while let Some(event) = stream.recv().await {
                    output.push(event);
                }
            }
            Err(error) => output.push(Err(error)),
        }
        handle.join().unwrap();
        output
    }

    fn spawn_closing_server() -> (String, Arc<AtomicUsize>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let observed_requests = Arc::clone(&requests);
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        observed_requests.fetch_add(1, Ordering::SeqCst);
                        let _ = socket.set_read_timeout(Some(Duration::from_millis(50)));
                        let mut request = [0_u8; 1024];
                        let _ = socket.read(&mut request);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::yield_now();
                    }
                    Err(_) => break,
                }
            }
        });
        (format!("http://{address}"), requests, handle)
    }

    fn map_event(
        value: Value,
        tool_calls: &mut HashMap<String, (String, String)>,
        completed_tool_calls: &mut HashSet<String>,
    ) -> Result<Vec<ModelEvent>, BackendError> {
        super::map_event(ResponsesEvent::new(None, value), tool_calls, completed_tool_calls)
    }

    #[test]
    fn maps_text_delta_without_exposing_reasoning() {
        let mut calls = HashMap::new();
        let mut completed = HashSet::new();
        let events = map_event(
            json!({
                "type": "response.output_text.delta",
                "delta": "hello"
            }),
            &mut calls,
            &mut completed,
        )
        .unwrap();
        assert!(matches!(
            events.first(),
            Some(ModelEvent::TextDelta { text }) if text == "hello"
        ));
    }

    #[tokio::test]
    async fn sdk_stream_handles_fragmented_responses_events_and_done() {
        let body = concat!(
            "event: vendor.unknown\n",
            "data: {\"type\":\"vendor.unknown\",\"ignored\":true}\n\n",
            "event: response.output_text.delta\r\n",
            "data: {\"type\":\"response.output_text.delta\",\r\n",
            "data: \"delta\":\"hé\"}\r\n",
            "\r\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-1\"}}\n\n",
            "data: [DONE]\n\n",
        )
        .as_bytes()
        .to_vec();
        let output = collect_backend_stream(body, 1).await;
        assert_eq!(output.len(), 2);
        assert!(matches!(
            &output[0],
            Ok(ModelEvent::TextDelta { text }) if text == "hé"
        ));
        assert!(matches!(
            &output[1],
            Ok(ModelEvent::Done { continuation: Some(continuation) })
                if continuation.0["previous_response_id"] == "resp-1"
        ));
    }

    #[tokio::test]
    async fn sdk_stream_maps_malformed_json_to_bounded_serialization_error() {
        let body = b"event: response.output_text.delta\ndata: {not-json}\n\n".to_vec();
        let output = collect_backend_stream(body, 3).await;
        assert!(matches!(
            output.as_slice(),
            [Err(BackendError::Operation { message, retryable: false })]
                if message.contains("serialization")
        ));
    }

    #[tokio::test]
    async fn sdk_stream_rejects_oversized_sse_frames_without_forwarding_payload() {
        let mut body = b"event: response.output_text.delta\ndata: ".to_vec();
        body.extend(vec![b'x'; 8 * 1024 * 1024]);
        body.extend_from_slice(b"\n\n");
        let output = collect_backend_stream(body, 64 * 1024).await;
        assert!(matches!(
            output.as_slice(),
            [Err(BackendError::Operation { message, retryable: false })]
                if message.contains("payload too large")
        ));
    }

    #[tokio::test]
    async fn sdk_stream_reports_disconnect_after_partial_response() {
        let body = b"event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".to_vec();
        let output = collect_backend_stream(body, 2).await;
        assert!(output.iter().any(|event| matches!(
            event,
            Ok(ModelEvent::TextDelta { text }) if text == "partial"
        )));
        assert!(output.iter().any(|event| matches!(
            event,
            Err(BackendError::IncompleteStream { message })
                if message.contains("ended before a terminal")
        )));
        assert!(!output.iter().any(|event| matches!(event, Ok(ModelEvent::Done { .. }))));
    }

    #[tokio::test]
    async fn inference_transport_failure_is_sent_once_without_adapter_retry() {
        let (base_url, requests, handle) = spawn_closing_server();
        let backend = test_backend(&base_url);
        let result = backend.stream_step(test_model_request()).await;
        assert!(matches!(result, Err(BackendError::Operation { .. })));
        handle.join().unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn normalized_model_view_preserves_sdk_fields_and_marks_removed_metadata_unknown() {
        let item: RainyModelCatalogItem = serde_json::from_value(json!({
            "id": "provider/frontier",
            "name": "Frontier \u{001b}[31mModel",
            "context_length": 128000,
            "architecture": {
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"]
            },
            "supported_parameters": ["tools", "reasoning.effort"],
            "rainy_capabilities": {"tools": true, "reasoning": true}
        }))
        .unwrap();
        let view = ModelView::from_catalog(&item).expect("safe model id");
        assert_eq!(view.display_name, "Frontier �[31mModel");
        assert_eq!(view.context_length, Some(128000));
        assert_eq!(view.model_context_length, Some(128000));
        assert_eq!(view.plan_context_length, None);
        assert_eq!(view.effective_context_length, None);
        assert_eq!(view.tier, None);
        assert_eq!(view.billing_class, None);
        assert_eq!(view.tools, CapabilityState::Supported);
        assert_eq!(view.reasoning, CapabilityState::Supported);
        assert!(view.input_modalities.contains(&"image".to_owned()));
        assert!(view.reasoning_modes.contains(&"effort".to_owned()));
        assert!(view.reasoning_parameters.contains(&"reasoning.effort".to_owned()));
        assert_eq!(view.thinking_budget_min, None);
        assert_eq!(view.thinking_budget_max, None);
        assert_eq!(view.availability, ModelAvailability::Unknown);
        assert!(view.selectable());
        assert!(!view.disclosure_required);
        assert_eq!(view.retention_days, None);
        assert!(view.detail().contains("access unknown"));
    }

    #[test]
    fn normalized_catalog_fixture_matrix_preserves_unknowns_and_selection_rules() {
        let fixtures = vec![
            json!({
                "id": "01-coding",
                "name": "Coding Core",
                "context_length": 128000,
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "rainy_capabilities": {"tools": true, "reasoning": false}
            }),
            json!({
                "id": "02-no-tools",
                "name": "No Tools",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "rainy_capabilities": {"tools": false, "reasoning": false}
            }),
            json!({
                "id": "03-reasoning",
                "name": "Reasoning Model",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "supported_parameters": ["tools", "reasoning.effort", "reasoning.max_tokens"],
                "rainy_capabilities": {"tools": true, "reasoning": true}
            }),
            json!({"id": "04-unknown", "name": "Unknown Capability"}),
            json!({
                "id": "05-context",
                "name": "Context Metadata",
                "context_length": 1000000,
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "rainy_capabilities": {"tools": true, "reasoning": false}
            }),
            json!({
                "id": "06-org-metadata-removed",
                "name": "Organization Metadata Removed",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "rainy_organization_enabled": false,
                "rainy_capabilities": {"tools": true, "reasoning": false}
            }),
            json!({
                "id": "07-privacy-metadata-removed",
                "name": "Privacy Metadata Removed",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "rainy_privacy_compatible": false,
                "rainy_capabilities": {"tools": true, "reasoning": false}
            }),
            json!({
                "id": "08-policy-metadata-removed",
                "name": "Policy Metadata Removed",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "rainy_data_policy": {"requiresDataDisclosure": true, "retentionDays": 7},
                "rainy_capabilities": {"tools": true, "reasoning": false}
            }),
            json!({
                "id": "09-missing-optional",
                "name": "Minimal Metadata",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "supported_parameters": ["tools"]
            }),
            json!({
                "id": "10-untrusted-name",
                "name": "Bad\u{001b}[31mName\n",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "supported_parameters": ["tools"]
            }),
            json!({"id": "11-duplicate-a", "name": "Same Name", "supported_parameters": ["tools"]}),
            json!({"id": "12-duplicate-b", "name": "Same Name", "supported_parameters": ["tools"]}),
            json!({
                "id": "13-v1-capabilities",
                "name": "V1 Capabilities",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "rainy_capabilities": {"tools": true, "reasoning": "unknown"}
            }),
            json!({
                "id": "14-parameter-fallback",
                "name": "Parameter Fallback",
                "architecture": {"input_modalities": ["text"], "output_modalities": ["text"]},
                "supported_parameters": ["tools", "reasoning.effort"]
            }),
        ]
        .into_iter()
        .map(|fixture| serde_json::from_value::<RainyModelCatalogItem>(fixture).unwrap())
        .collect::<Vec<_>>();

        let (models, views) = normalize_catalog(fixtures);
        assert_eq!(models.len(), 14);
        assert_eq!(views.len(), 14);
        assert!(views.windows(2).all(|pair| pair[0].id < pair[1].id));
        let view = |id: &str| views.iter().find(|view| view.id == id).unwrap();

        assert!(view("01-coding").selectable());
        assert_eq!(view("02-no-tools").tools, CapabilityState::Unsupported);
        assert!(!view("02-no-tools").selectable());
        assert_eq!(view("03-reasoning").reasoning, CapabilityState::Supported);
        assert!(view("03-reasoning").reasoning_modes.contains(&"effort".to_owned()));
        assert!(view("03-reasoning").reasoning_modes.contains(&"budget".to_owned()));
        assert_eq!(view("03-reasoning").thinking_budget_min, None);
        assert_eq!(view("03-reasoning").thinking_budget_max, None);
        assert!(view("03-reasoning").reasoning_values.is_empty());
        assert!(view("03-reasoning").reasoning_parameters.contains(&"reasoning.effort".to_owned()));
        assert!(
            view("03-reasoning").reasoning_parameters.contains(&"reasoning.max_tokens".to_owned())
        );
        assert_eq!(view("04-unknown").tools, CapabilityState::Unknown);
        assert_eq!(view("04-unknown").availability, ModelAvailability::Unknown);
        assert!(view("04-unknown").has_unknown_metadata());
        assert_eq!(view("05-context").context_length, Some(1000000));
        assert_eq!(view("05-context").model_context_length, Some(1000000));
        assert_eq!(view("05-context").plan_context_length, None);
        assert_eq!(view("05-context").effective_context_length, None);
        assert_eq!(view("06-org-metadata-removed").availability, ModelAvailability::Unknown);
        assert!(view("06-org-metadata-removed").selectable());
        assert_eq!(view("07-privacy-metadata-removed").availability, ModelAvailability::Unknown);
        assert!(view("07-privacy-metadata-removed").selectable());
        assert!(!view("08-policy-metadata-removed").disclosure_required);
        assert_eq!(view("08-policy-metadata-removed").retention_days, None);
        assert_eq!(view("09-missing-optional").tools, CapabilityState::Supported);
        assert_eq!(view("10-untrusted-name").display_name, "Bad�[31mName�");
        assert_eq!(view("11-duplicate-a").display_name, view("12-duplicate-b").display_name);
        assert_eq!(view("13-v1-capabilities").tools, CapabilityState::Supported);
        assert_eq!(view("13-v1-capabilities").reasoning, CapabilityState::Unknown);
        assert_eq!(view("14-parameter-fallback").reasoning, CapabilityState::Supported);
    }

    #[test]
    fn normalized_catalog_drops_unsafe_ids_and_orders_by_id() {
        let first: RainyModelCatalogItem =
            serde_json::from_value(json!({"id": "z-model", "name": "Z"})).unwrap();
        let second: RainyModelCatalogItem =
            serde_json::from_value(json!({"id": "a-model", "name": "A"})).unwrap();
        let unsafe_id: RainyModelCatalogItem =
            serde_json::from_value(json!({"id": "bad\u{001b}[31m", "name": "unsafe"})).unwrap();
        let whitespace_id: RainyModelCatalogItem =
            serde_json::from_value(json!({"id": "bad id", "name": "ambiguous"})).unwrap();
        let (models, views) = normalize_catalog(vec![first, unsafe_id, whitespace_id, second]);
        assert_eq!(
            models.iter().map(|model| model.id.as_str()).collect::<Vec<_>>(),
            vec!["a-model", "z-model"]
        );
        assert_eq!(
            views.iter().map(|view| view.id.as_str()).collect::<Vec<_>>(),
            vec!["a-model", "z-model"]
        );
    }

    #[test]
    fn model_selection_never_uses_catalog_position() {
        assert_eq!(select_model(Some("rainy-model".to_owned())).unwrap(), "rainy-model");
        assert!(matches!(
            select_model(None),
            Err(BackendError::FeatureUnavailable { feature })
                if feature.contains("no model selected")
        ));
    }

    #[test]
    fn missing_previous_response_id_is_typed_invalid_continuation() {
        let error =
            previous_response_id(&OpaqueContinuation(json!({"fake_step": "x"}))).unwrap_err();
        assert!(matches!(error, BackendError::InvalidContinuation { .. }));
    }

    #[test]
    fn rainy_invalid_request_code_maps_to_invalid_continuation_only_when_attached() {
        let error = RainyError::InvalidRequest {
            code: "previous_response_id".to_owned(),
            message: "ignored".to_owned(),
            details: None,
        };
        assert!(matches!(map_rainy_error(&error, true), BackendError::InvalidContinuation { .. }));
        assert!(matches!(map_rainy_error(&error, false), BackendError::Operation { .. }));
        let network = RainyError::Network {
            message: "previous_response_id expired".to_owned(),
            retryable: true,
            source_error: None,
        };
        assert!(matches!(map_rainy_error(&network, true), BackendError::Operation { .. }));
    }

    #[test]
    fn typed_rainy_errors_map_to_bounded_categories_without_provider_messages() {
        let cases = [
            (
                RainyError::Authentication {
                    code: "INVALID_API_KEY".to_owned(),
                    message: "secret-key-value".to_owned(),
                    retryable: false,
                },
                "authentication",
                false,
            ),
            (
                RainyError::RateLimit {
                    code: "RATE_LIMIT_EXCEEDED".to_owned(),
                    message: "secret-rate-detail".to_owned(),
                    retry_after: Some(30),
                    current_usage: None,
                },
                "rate limit",
                true,
            ),
            (
                RainyError::Serialization {
                    message: "secret-serialization-detail".to_owned(),
                    source_error: Some("secret-source-detail".to_owned()),
                },
                "serialization",
                false,
            ),
            (
                RainyError::PayloadTooLarge {
                    message: "secret-payload-detail".to_owned(),
                    max_bytes: 1024,
                },
                "payload too large",
                false,
            ),
        ];
        for (error, category, retryable) in cases {
            let mapped = map_rainy_error(&error, false);
            assert!(matches!(
                mapped,
                BackendError::Operation { ref message, retryable: actual }
                    if actual == retryable
                        && message.contains(category)
                        && message.len() <= 256
                        && !message.contains("secret")
            ));
        }
    }

    #[test]
    fn maps_previous_response_id_as_opaque_continuation() {
        let mut calls = HashMap::new();
        let mut completed = HashSet::new();
        let events = map_event(
            json!({
                "type": "response.completed",
                "response": {"id": "resp_123"}
            }),
            &mut calls,
            &mut completed,
        )
        .unwrap();
        assert!(matches!(
            events.last(),
            Some(ModelEvent::Done { continuation: Some(value) })
                if value.0["previous_response_id"] == "resp_123"
        ));
    }

    #[test]
    fn malformed_tool_arguments_fail_closed() {
        let mut calls = HashMap::new();
        let mut completed = HashSet::new();
        let result = map_event(
            json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "function_call",
                    "id": "call_1",
                    "name": "read",
                    "arguments": "not-json"
                }
            }),
            &mut calls,
            &mut completed,
        );
        assert!(matches!(result, Err(BackendError::Mapping { .. })));
    }

    #[test]
    fn duplicate_function_call_completion_emits_once() {
        let mut calls = HashMap::new();
        let mut completed = HashSet::new();
        assert!(
            map_event(
                json!({
                    "type": "response.output_item.added",
                    "item": {
                        "type": "function_call",
                        "id": "call_1",
                        "name": "read",
                        "arguments": "{}"
                    }
                }),
                &mut calls,
                &mut completed,
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            map_event(
                json!({
                    "type": "response.function_call_arguments.done",
                    "call_id": "call_1",
                    "name": "read",
                    "arguments": "{}"
                }),
                &mut calls,
                &mut completed,
            )
            .unwrap()
            .len(),
            1
        );
        assert!(
            map_event(
                json!({
                    "type": "response.output_item.done",
                    "item": {
                        "type": "function_call",
                        "id": "call_1",
                        "name": "read",
                        "arguments": "{}"
                    }
                }),
                &mut calls,
                &mut completed,
            )
            .unwrap()
            .is_empty()
        );
    }

    #[tokio::test]
    async fn normal_text_response_requires_completed_terminal_event() {
        let output = collect(vec![
            json!({"type": "response.output_text.delta", "delta": "hello"}),
            json!({"type": "response.completed", "response": {"id": "resp_text"}}),
        ])
        .await;
        assert!(matches!(output[0], Ok(ModelEvent::TextDelta { ref text }) if text == "hello"));
        assert!(matches!(
            output[1],
            Ok(ModelEvent::Done { continuation: Some(ref continuation) })
                if continuation.0["previous_response_id"] == "resp_text"
        ));
    }

    #[tokio::test]
    async fn completed_tool_call_and_terminal_event_are_forwarded() {
        let output = collect(vec![
            json!({
                "type": "response.function_call_arguments.delta",
                "call_id": "call_1",
                "delta": "{}"
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "call_id": "call_1",
                "name": "read"
            }),
            json!({"type": "response.completed", "response": {"id": "resp_tool"}}),
        ])
        .await;
        assert!(matches!(output[0], Ok(ModelEvent::ToolCall { ref name, .. }) if name == "read"));
        assert!(matches!(output[1], Ok(ModelEvent::Done { .. })));
    }

    #[tokio::test]
    async fn unexpected_eof_is_an_incomplete_stream_error() {
        let output = collect(vec![json!({
            "type": "response.output_text.delta",
            "delta": "partial"
        })])
        .await;
        assert!(matches!(
            output.last(),
            Some(Err(BackendError::IncompleteStream { message }))
                if message.contains("ended before a terminal")
        ));
        assert!(!output.iter().any(|event| matches!(event, Ok(ModelEvent::Done { .. }))));
    }

    #[tokio::test]
    async fn partial_tool_arguments_are_discarded_on_eof() {
        let output = collect(vec![json!({
            "type": "response.function_call_arguments.delta",
            "call_id": "call_partial",
            "delta": "{\"path\":"
        })])
        .await;
        assert!(
            output.iter().any(|event| matches!(event, Err(BackendError::IncompleteStream { .. })))
        );
        assert!(!output.iter().any(|event| matches!(event, Ok(ModelEvent::ToolCall { .. }))));
    }

    #[tokio::test]
    async fn completed_tool_call_without_terminal_event_fails_closed() {
        let output = collect(vec![json!({
            "type": "response.function_call_arguments.done",
            "call_id": "call_no_terminal",
            "name": "read",
            "arguments": "{}"
        })])
        .await;
        assert!(output.iter().any(|event| matches!(event, Ok(ModelEvent::ToolCall { .. }))));
        assert!(
            output.iter().any(|event| matches!(event, Err(BackendError::IncompleteStream { .. })))
        );
        assert!(!output.iter().any(|event| matches!(event, Ok(ModelEvent::Done { .. }))));
    }

    #[test]
    fn failed_and_incomplete_response_events_are_typed_failures() {
        let mut calls = HashMap::new();
        let mut completed = HashSet::new();
        assert!(matches!(
            map_event(
                json!({
                    "type": "response.failed",
                    "response": {"error": {"code": "server_error", "message": "bounded"}}
                }),
                &mut calls,
                &mut completed,
            ),
            Err(BackendError::ResponseFailed { message }) if message.contains("server_error")
        ));
        assert!(matches!(
            map_event(
                json!({
                    "type": "response.incomplete",
                    "response": {"incomplete_details": {"reason": "max_output_tokens"}}
                }),
                &mut calls,
                &mut completed,
            ),
            Err(BackendError::ResponseIncomplete { message })
                if message.contains("max_output_tokens")
        ));
    }

    #[tokio::test]
    async fn receiver_drop_stops_stream_consumption() {
        let stream = Box::pin(stream::pending::<Result<ResponsesStreamEvent, RainyError>>());
        let (sender, receiver) = mpsc::channel(1);
        let task = tokio::spawn(adapt_stream(stream, sender, false));
        drop(receiver);
        tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap();
    }
}
