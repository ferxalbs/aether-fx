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
use rainy_sdk::{RainyClient, RainyError, ResponsesRequest};
use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

const CATALOG_TTL: Duration = Duration::from_secs(300);
const STREAM_CHANNEL_CAPACITY: usize = 64;

#[derive(Clone, Debug)]
struct CatalogCache {
    refreshed_at: Instant,
    models: Vec<ModelCatalogItem>,
}

pub struct RainyBackend {
    client: RainyClient,
    catalog: Arc<Mutex<Option<CatalogCache>>>,
}

impl RainyBackend {
    pub fn from_env() -> Result<Self, BackendError> {
        let key = env::var_os("RAINY_API_KEY")
            .ok_or(BackendError::CredentialsUnavailable)?
            .into_string()
            .map_err(|_| BackendError::CredentialsUnavailable)?;
        let client = RainyClient::with_api_key(key).map_err(|error| map_rainy_error(&error))?;
        Ok(Self { client, catalog: Arc::new(Mutex::new(None)) })
    }

    pub fn new(client: RainyClient) -> Self {
        Self { client, catalog: Arc::new(Mutex::new(None)) }
    }

    async fn catalog(&self) -> Result<Vec<ModelCatalogItem>, BackendError> {
        {
            let cache = self.catalog.lock().await;
            if let Some(cache) = cache.as_ref()
                && cache.refreshed_at.elapsed() < CATALOG_TTL
            {
                return Ok(cache.models.clone());
            }
        }
        let catalog =
            self.client.get_models_catalog().await.map_err(|error| map_rainy_error(&error))?;
        let models = catalog
            .into_iter()
            .map(|item| {
                let capabilities = serde_json::to_value(&item).unwrap_or(Value::Null);
                ModelCatalogItem {
                    id: item.id,
                    display_name: item.name,
                    context_window: item.context_length,
                    capabilities,
                }
            })
            .collect::<Vec<_>>();
        *self.catalog.lock().await =
            Some(CatalogCache { refreshed_at: Instant::now(), models: models.clone() });
        Ok(models)
    }

    async fn model_for(&self, requested: Option<String>) -> Result<String, BackendError> {
        if let Some(model) = requested {
            if model.is_empty() {
                return Err(BackendError::Mapping {
                    message: "model identifier must not be empty".to_owned(),
                });
            }
            return Ok(model);
        }
        self.catalog().await?.into_iter().next().map(|model| model.id).ok_or_else(|| {
            BackendError::FeatureUnavailable {
                feature: "Rainy returned an empty model catalog".to_owned(),
            }
        })
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
            let response_id =
                continuation.0.get("previous_response_id").and_then(Value::as_str).ok_or_else(
                    || BackendError::FeatureUnavailable {
                        feature: "Rainy SDK continuation requires previous_response_id".to_owned(),
                    },
                )?;
            rainy_request = rainy_request.with_previous_response_id(response_id);
        }
        Ok(rainy_request)
    }
}

impl ModelBackend for RainyBackend {
    fn stream_step<'a>(&'a self, request: ModelRequest) -> BackendFuture<'a> {
        Box::pin(async move {
            let model = self.model_for(request.model.clone()).await?;
            let rainy_request = self.request(&request, model)?;
            let stream = self
                .client
                .create_response_stream(rainy_request)
                .await
                .map_err(|error| map_rainy_error(&error))?;
            let (sender, receiver) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
            tokio::spawn(async move {
                adapt_stream(stream, sender).await;
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
        Box<dyn futures_util::Stream<Item = Result<Value, RainyError>> + Send>,
    >,
    sender: mpsc::Sender<Result<ModelEvent, BackendError>>,
) {
    let mut tool_calls: HashMap<String, (String, String)> = HashMap::new();
    let mut completed_tool_calls = HashSet::new();
    let mut completed = false;
    loop {
        tokio::select! {
            _ = sender.closed() => return,
            event = stream.next() => {
                let Some(event) = event else { break; };
                let event = match event {
                    Ok(value) => value,
                    Err(error) => {
                        let _ = sender.send(Err(map_rainy_error(&error))).await;
                        return;
                    }
                };
                if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                    completed = true;
                }
                match map_event(event, &mut tool_calls, &mut completed_tool_calls) {
                    Ok(events) => {
                        for event in events {
                            if sender.send(Ok(event)).await.is_err() {
                                return;
                            }
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
    if !completed {
        let _ = sender.send(Ok(ModelEvent::Done { continuation: None })).await;
    }
}

fn map_event(
    event: Value,
    tool_calls: &mut HashMap<String, (String, String)>,
    completed_tool_calls: &mut HashSet<String>,
) -> Result<Vec<ModelEvent>, BackendError> {
    let event_type = event.get("type").and_then(Value::as_str).unwrap_or_default();
    match event_type {
        "response.output_text.delta" => Ok(event
            .get("delta")
            .and_then(Value::as_str)
            .map(|text| vec![ModelEvent::TextDelta { text: text.to_owned() }])
            .unwrap_or_default()),
        "response.function_call_arguments.delta" => {
            let call_id = event_string(&event, &["call_id", "item_id", "id"])?;
            let entry = tool_calls.entry(call_id).or_insert_with(|| (String::new(), String::new()));
            if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                entry.1.push_str(delta);
            }
            Ok(Vec::new())
        }
        "response.function_call_arguments.done" => {
            let call_id = event_string(&event, &["call_id", "item_id", "id"])?;
            if completed_tool_calls.contains(&call_id) {
                return Ok(Vec::new());
            }
            let name = event
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| tool_calls.get(&call_id).map(|value| value.0.clone()))
                .ok_or_else(|| BackendError::Mapping {
                    message: "Rainy function call omitted its name".to_owned(),
                })?;
            let arguments = event
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
        "response.output_item.done" | "response.output_item.added" => {
            let item = event.get("item").unwrap_or(&event);
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
            if event_type == "response.output_item.added" {
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
        "response.completed" => {
            let response = event.get("response").unwrap_or(&event);
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
            let continuation = response
                .get("id")
                .and_then(Value::as_str)
                .map(|id| OpaqueContinuation(json!({"previous_response_id": id})));
            output.push(ModelEvent::Done { continuation });
            Ok(output)
        }
        "response.failed" | "response.incomplete" | "error" => Err(BackendError::Operation {
            message: "Rainy returned an unsuccessful response event".to_owned(),
            retryable: false,
        }),
        _ => Ok(Vec::new()),
    }
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

fn map_rainy_error(error: &RainyError) -> BackendError {
    let request_id =
        error.request_id().map(|value| format!(" (request_id={value})")).unwrap_or_default();
    BackendError::Operation {
        message: format!("Rainy SDK request failed{request_id}"),
        retryable: error.is_retryable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
