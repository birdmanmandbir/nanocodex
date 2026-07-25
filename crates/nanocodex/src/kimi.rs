use std::{fmt, sync::Arc, time::Duration};

use nanocodex_core::{
    AgentEventKind, ContentItem, EventSink, FunctionOutputBody, ItemStatus, MessagePhase,
    MessageRole, ResponseItem, ResponseItemId, ToolDefinition,
    responses::{InputTokenDetails, OutputTokenDetails, Usage},
};
use nanocodex_service::{
    CodeCall, CodeCallKind, ResponsePipelineStats, TurnResult as ServiceTurnResult,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json, value::RawValue};
use tracing::info;

pub const KIMI_FALLBACK_MODEL: &str = "kimi-k3";
pub const KIMI_FALLBACK_API_BASE_URL: &str = "https://api.moonshot.ai/v1";
pub const KIMI_FALLBACK_MAX_LEASE_GENERATIONS: u32 = 16;

/// Explicit policy for temporarily using Kimi after a structured
/// `cyber_policy` response from the primary model.
#[derive(Clone)]
pub struct KimiRefusalFallback {
    api_key: Arc<str>,
    api_base_url: Arc<str>,
    reasoning_effort: Arc<str>,
    max_lease_generations: u32,
    request_timeout: Duration,
}

impl KimiRefusalFallback {
    #[must_use]
    pub fn new(api_key: impl Into<Arc<str>>) -> Self {
        Self {
            api_key: api_key.into(),
            api_base_url: Arc::from(KIMI_FALLBACK_API_BASE_URL),
            reasoning_effort: Arc::from("high"),
            max_lease_generations: KIMI_FALLBACK_MAX_LEASE_GENERATIONS,
            request_timeout: Duration::from_mins(5),
        }
    }

    #[must_use]
    pub fn api_base_url(mut self, api_base_url: impl Into<Arc<str>>) -> Self {
        self.api_base_url = api_base_url.into();
        self
    }

    /// Sets Kimi K3 reasoning effort to `low`, `high`, or `max`.
    #[must_use]
    pub fn reasoning_effort(mut self, reasoning_effort: impl Into<Arc<str>>) -> Self {
        self.reasoning_effort = reasoning_effort.into();
        self
    }

    /// Caps the exponentially growing `1, 2, 4, ...` Kimi lease.
    #[must_use]
    pub const fn max_lease_generations(mut self, generations: u32) -> Self {
        self.max_lease_generations = generations;
        self
    }

    #[must_use]
    pub const fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    #[must_use]
    pub const fn configured_max_lease_generations(&self) -> u32 {
        self.max_lease_generations
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.api_key.trim().is_empty() {
            return Err("Kimi refusal fallback API key must not be empty".to_owned());
        }
        if self.api_base_url.trim().is_empty() {
            return Err("Kimi refusal fallback API base URL must not be empty".to_owned());
        }
        let endpoint = url::Url::parse(&self.api_base_url)
            .map_err(|error| format!("invalid Kimi refusal fallback API base URL: {error}"))?;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err("Kimi refusal fallback API base URL must use HTTP or HTTPS".to_owned());
        }
        if !matches!(self.reasoning_effort.as_ref(), "low" | "high" | "max") {
            return Err(
                "Kimi refusal fallback reasoning effort must be low, high, or max".to_owned(),
            );
        }
        if self.max_lease_generations == 0 {
            return Err(
                "Kimi refusal fallback maximum lease must be at least one generation".to_owned(),
            );
        }
        if self.request_timeout.is_zero() {
            return Err("Kimi refusal fallback request timeout must not be zero".to_owned());
        }
        Ok(())
    }

    fn chat_completions_url(&self) -> String {
        format!(
            "{}/chat/completions",
            self.api_base_url.trim_end_matches('/')
        )
    }
}

impl fmt::Debug for KimiRefusalFallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KimiRefusalFallback")
            .field("api_key", &"[redacted]")
            .field("api_base_url", &self.api_base_url)
            .field("model", &KIMI_FALLBACK_MODEL)
            .field("reasoning_effort", &self.reasoning_effort)
            .field("max_lease_generations", &self.max_lease_generations)
            .field("request_timeout", &self.request_timeout)
            .finish()
    }
}

pub(crate) struct KimiClient {
    config: Arc<KimiRefusalFallback>,
    http: reqwest::Client,
}

impl KimiClient {
    pub(crate) fn new(config: Arc<KimiRefusalFallback>) -> Self {
        Self {
            config,
            http: reqwest::Client::new(),
        }
    }

    pub(crate) const fn model() -> &'static str {
        KIMI_FALLBACK_MODEL
    }

    pub(crate) fn reasoning_effort(&self) -> &str {
        &self.config.reasoning_effort
    }

    pub(crate) fn max_lease_generations(&self) -> u32 {
        self.config.configured_max_lease_generations()
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn generate(
        &self,
        events: &EventSink,
        call_index: u32,
        request_prefix: &[ResponseItem],
        history: &[ResponseItem],
        history_revision: u64,
        prompt_cache_key: &str,
        transcript: &mut Option<KimiTranscript>,
    ) -> Result<ServiceTurnResult, KimiError> {
        let tools = convert_tools(request_prefix)?;
        let transcript = prepare_transcript(transcript, request_prefix, history, history_revision)?;
        let request = ChatCompletionRequest {
            model: Self::model(),
            messages: &transcript.messages,
            tools: &tools,
            prompt_cache_key,
            stream: false,
            reasoning_effort: self.config.reasoning_effort.as_ref(),
        };
        let encoded = serde_json::to_string(&request).map_err(KimiError::EncodeRequest)?;
        emit_api_event(events, call_index, "outgoing", &encoded)?;
        info!(
            target: "nanocodex",
            content_kind = "kimi.request",
            content = encoded.as_str(),
            "model content"
        );
        let response = self
            .http
            .post(self.config.chat_completions_url())
            .bearer_auth(self.config.api_key.as_ref())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .timeout(self.config.request_timeout)
            .body(encoded)
            .send()
            .await
            .map_err(KimiError::Request)?;
        let status = response.status();
        let body = response.text().await.map_err(KimiError::Request)?;
        emit_api_event(events, call_index, "incoming", &body)?;
        info!(
            target: "nanocodex",
            content_kind = "kimi.response",
            content = body.as_str(),
            "model content"
        );
        if !status.is_success() {
            return Err(KimiError::Rejected {
                status: status.as_u16(),
                body,
            });
        }
        let response: ChatCompletionResponse =
            serde_json::from_str(&body).map_err(KimiError::DecodeResponse)?;
        let choice = response
            .choices
            .into_iter()
            .next()
            .ok_or(KimiError::InvalidResponse(
                "Kimi response did not contain a choice".to_owned(),
            ))?;
        if choice.finish_reason.as_deref() == Some("length") {
            return Err(KimiError::InvalidResponse(
                "Kimi response reached its completion-token limit".to_owned(),
            ));
        }
        let converted = convert_response(
            events,
            call_index,
            &response.id,
            choice.message.clone(),
            choice.finish_reason.as_deref(),
            response.usage,
        )?;
        transcript.messages.push(choice.message);
        transcript.consumed_history_len =
            history.len().saturating_add(converted.output_items.len());
        Ok(converted)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum KimiError {
    #[error("failed to encode the Kimi fallback request")]
    EncodeRequest(#[source] serde_json::Error),
    #[error("Kimi fallback request failed")]
    Request(#[source] reqwest::Error),
    #[error("Kimi fallback API rejected the request with HTTP {status}: {body}")]
    Rejected { status: u16, body: String },
    #[error("Kimi fallback response was not valid JSON")]
    DecodeResponse(#[source] serde_json::Error),
    #[error("invalid Kimi fallback response: {0}")]
    InvalidResponse(String),
    #[error("failed to emit Kimi fallback telemetry")]
    Event(#[from] nanocodex_core::EventError),
}

pub(crate) struct KimiTranscript {
    messages: Vec<ChatMessage>,
    consumed_history_len: usize,
    history_revision: u64,
}

fn prepare_transcript<'a>(
    transcript: &'a mut Option<KimiTranscript>,
    request_prefix: &[ResponseItem],
    history: &[ResponseItem],
    history_revision: u64,
) -> Result<&'a mut KimiTranscript, KimiError> {
    let rebuild = transcript.as_ref().is_none_or(|current| {
        current.history_revision != history_revision || current.consumed_history_len > history.len()
    });
    if rebuild {
        let mut messages = Vec::new();
        append_items(&mut messages, request_prefix)?;
        append_items(&mut messages, history)?;
        *transcript = Some(KimiTranscript {
            messages,
            consumed_history_len: history.len(),
            history_revision,
        });
    } else if let Some(current) = transcript.as_mut() {
        append_items(
            &mut current.messages,
            &history[current.consumed_history_len..],
        )?;
        current.consumed_history_len = history.len();
    }
    transcript.as_mut().ok_or(KimiError::InvalidResponse(
        "failed to prepare Kimi transcript".to_owned(),
    ))
}

#[derive(Serialize)]
struct ChatCompletionRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    tools: &'a [ChatTool],
    prompt_cache_key: &'a str,
    stream: bool,
    reasoning_effort: &'a str,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChatMessage {
    role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reasoning_content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ChatToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

impl ChatMessage {
    fn ordinary(role: &str, content: String) -> Self {
        Self {
            role: role.to_owned(),
            content: Some(content),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    fn assistant() -> Self {
        Self {
            role: "assistant".to_owned(),
            content: Some(String::new()),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }

    fn tool(call_id: &str, name: Option<&str>, content: String) -> Self {
        Self {
            role: "tool".to_owned(),
            content: Some(content),
            reasoning_content: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.to_owned()),
            name: name.map(str::to_owned),
        }
    }

    fn append_content(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let content = self.content.get_or_insert_default();
        if !content.is_empty() {
            content.push('\n');
        }
        content.push_str(text);
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChatToolCall {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    index: Option<u32>,
    id: String,
    #[serde(rename = "type")]
    kind: String,
    function: ChatFunctionCall,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ChatFunctionCall {
    name: String,
    arguments: String,
}

#[derive(Serialize)]
struct ChatTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: ChatToolFunction,
}

#[derive(Serialize)]
struct ChatToolFunction {
    name: String,
    description: String,
    parameters: Value,
}

#[derive(Deserialize)]
struct ChatCompletionResponse {
    id: String,
    #[serde(default)]
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<ChatUsage>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ChatUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    cached_tokens: u64,
    #[serde(default)]
    prompt_tokens_details: Option<ChatPromptTokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<ChatCompletionTokenDetails>,
}

#[derive(Deserialize)]
struct ChatPromptTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct ChatCompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: u64,
}

fn convert_tools(prefix: &[ResponseItem]) -> Result<Vec<ChatTool>, KimiError> {
    let mut converted = Vec::new();
    for item in prefix {
        let ResponseItem::AdditionalTools { tools, .. } = item else {
            continue;
        };
        for tool in tools {
            let (name, description, parameters) = match tool {
                ToolDefinition::Function {
                    name,
                    description,
                    parameters,
                    ..
                } => (
                    name.to_string(),
                    description.to_string(),
                    parameters.as_value().clone(),
                ),
                ToolDefinition::Custom {
                    name, description, ..
                } if name.as_ref() == "exec" => (
                    name.to_string(),
                    description.to_string(),
                    json!({
                        "type": "object",
                        "properties": {
                            "source": {
                                "type": "string",
                                "description": "JavaScript source accepted by Nanocodex Code Mode."
                            }
                        },
                        "required": ["source"],
                        "additionalProperties": false
                    }),
                ),
                ToolDefinition::Custom {
                    name, description, ..
                } => (
                    name.to_string(),
                    description.to_string(),
                    json!({
                        "type": "object",
                        "properties": {
                            "input": {"type": "string"}
                        },
                        "required": ["input"],
                        "additionalProperties": false
                    }),
                ),
            };
            converted.push(ChatTool {
                kind: "function",
                function: ChatToolFunction {
                    name,
                    description,
                    parameters,
                },
            });
        }
    }
    if converted.is_empty() {
        return Err(KimiError::InvalidResponse(
            "Kimi fallback request did not contain any tools".to_owned(),
        ));
    }
    Ok(converted)
}

#[allow(clippy::too_many_lines)]
fn append_items(messages: &mut Vec<ChatMessage>, items: &[ResponseItem]) -> Result<(), KimiError> {
    let mut assistant = None;
    for item in items {
        match item {
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::CompactionTrigger {} => {}
            ResponseItem::Message { role, content, .. } => {
                let text = content_items_text(content);
                match role {
                    MessageRole::Assistant => {
                        assistant
                            .get_or_insert_with(ChatMessage::assistant)
                            .append_content(&text);
                    }
                    MessageRole::Developer | MessageRole::User => {
                        flush_assistant(messages, &mut assistant);
                        if !text.is_empty() {
                            messages.push(ChatMessage::ordinary(
                                match role {
                                    MessageRole::Developer => "system",
                                    MessageRole::User => "user",
                                    MessageRole::Assistant => unreachable!(),
                                },
                                text,
                            ));
                        }
                    }
                }
            }
            ResponseItem::AgentMessage { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|item| match item {
                        nanocodex_core::AgentMessageContent::InputText { text } => {
                            Some(text.as_ref())
                        }
                        nanocodex_core::AgentMessageContent::EncryptedContent { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                assistant
                    .get_or_insert_with(ChatMessage::assistant)
                    .append_content(&text);
            }
            ResponseItem::FunctionCall {
                name,
                arguments,
                call_id,
                ..
            } => {
                assistant
                    .get_or_insert_with(ChatMessage::assistant)
                    .tool_calls
                    .push(ChatToolCall {
                        index: None,
                        id: call_id.to_string(),
                        kind: "function".to_owned(),
                        function: ChatFunctionCall {
                            name: name.to_string(),
                            arguments: arguments.to_string(),
                        },
                    });
            }
            ResponseItem::CustomToolCall {
                name,
                input,
                call_id,
                ..
            } => {
                let arguments = if name.as_ref() == "exec" {
                    serde_json::to_string(&json!({ "source": input.as_ref() }))
                } else {
                    serde_json::to_string(&json!({ "input": input.as_ref() }))
                }
                .map_err(KimiError::EncodeRequest)?;
                assistant
                    .get_or_insert_with(ChatMessage::assistant)
                    .tool_calls
                    .push(ChatToolCall {
                        index: None,
                        id: call_id.to_string(),
                        kind: "function".to_owned(),
                        function: ChatFunctionCall {
                            name: name.to_string(),
                            arguments,
                        },
                    });
            }
            ResponseItem::FunctionCallOutput {
                call_id, output, ..
            } => {
                flush_assistant(messages, &mut assistant);
                messages.push(ChatMessage::tool(
                    call_id,
                    None,
                    function_output_text(output)?,
                ));
            }
            ResponseItem::CustomToolCallOutput {
                call_id,
                name,
                output,
                ..
            } => {
                flush_assistant(messages, &mut assistant);
                messages.push(ChatMessage::tool(
                    call_id,
                    name.as_deref(),
                    function_output_text(output)?,
                ));
            }
            other => {
                flush_assistant(messages, &mut assistant);
                let encoded = serde_json::to_string(other).map_err(KimiError::EncodeRequest)?;
                messages.push(ChatMessage::ordinary(
                    "system",
                    format!("<responses_item>{encoded}</responses_item>"),
                ));
            }
        }
    }
    flush_assistant(messages, &mut assistant);
    Ok(())
}

fn flush_assistant(messages: &mut Vec<ChatMessage>, assistant: &mut Option<ChatMessage>) {
    if let Some(message) = assistant.take()
        && (message
            .content
            .as_deref()
            .is_some_and(|content| !content.is_empty())
            || !message.tool_calls.is_empty())
    {
        messages.push(message);
    }
}

fn content_items_text(items: &[ContentItem]) -> String {
    let mut parts = Vec::with_capacity(items.len());
    for item in items {
        match item {
            ContentItem::InputText { text } | ContentItem::OutputText { text, .. } => {
                if !text.is_empty() {
                    parts.push(text.to_string());
                }
            }
            ContentItem::InputImage { image_url, .. } => {
                parts.push(format!("[image: {image_url}]"));
            }
            ContentItem::InputAudio { audio_url } => {
                parts.push(format!("[audio: {audio_url}]"));
            }
        }
    }
    parts.join("\n")
}

fn function_output_text(output: &FunctionOutputBody) -> Result<String, KimiError> {
    match output {
        FunctionOutputBody::Text(text) => Ok(text.to_string()),
        FunctionOutputBody::Content(content) => {
            serde_json::to_string(content).map_err(KimiError::EncodeRequest)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn convert_response(
    events: &EventSink,
    call_index: u32,
    response_id: &str,
    message: ChatMessage,
    finish_reason: Option<&str>,
    usage: Option<ChatUsage>,
) -> Result<ServiceTurnResult, KimiError> {
    if message.role != "assistant" {
        return Err(KimiError::InvalidResponse(format!(
            "Kimi choice used unexpected role {:?}",
            message.role
        )));
    }
    if finish_reason == Some("tool_calls") && message.tool_calls.is_empty() {
        return Err(KimiError::InvalidResponse(
            "Kimi reported tool_calls without returning a tool call".to_owned(),
        ));
    }
    let mut output_items = Vec::with_capacity(message.tool_calls.len() + 1);
    let mut code_calls = Vec::with_capacity(message.tool_calls.len());
    let content = message.content.unwrap_or_default();
    if !content.is_empty() {
        let item_id = ResponseItemId::from_server(format!("msg_{response_id}"));
        let phase = if message.tool_calls.is_empty() {
            MessagePhase::FinalAnswer
        } else {
            MessagePhase::Commentary
        };
        events.emit(
            AgentEventKind::AssistantMessage,
            AssistantMessageEvent {
                model_call_index: call_index,
                item_id: item_id.as_str(),
                phase,
                text: &content,
            },
        )?;
        output_items.push(ResponseItem::Message {
            id: Some(item_id),
            role: MessageRole::Assistant,
            content: vec![ContentItem::output_text(content.clone())],
            status: Some(ItemStatus::Completed),
            phase: Some(phase),
            internal_chat_message_metadata_passthrough: None,
        });
    }
    for (index, call) in message.tool_calls.into_iter().enumerate() {
        let call_id = call.id;
        let name = call.function.name;
        let arguments = call.function.arguments;
        if name == "exec" {
            let source = serde_json::from_str::<Value>(&arguments)
                .ok()
                .and_then(|arguments| {
                    arguments
                        .get("source")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                })
                .ok_or_else(|| {
                    KimiError::InvalidResponse(
                        "Kimi exec call did not contain a string source argument".to_owned(),
                    )
                })?;
            output_items.push(ResponseItem::CustomToolCall {
                id: Some(ResponseItemId::from_server(format!(
                    "ctc_{response_id}_{index}"
                ))),
                status: Some(ItemStatus::Completed),
                call_id: call_id.clone().into_boxed_str(),
                name: name.clone().into_boxed_str(),
                namespace: None,
                input: source.clone().into_boxed_str(),
                caller: None,
                created_by: Some("kimi".into()),
                internal_chat_message_metadata_passthrough: None,
            });
            code_calls.push(CodeCall {
                call_id,
                name,
                namespace: None,
                input: source,
                kind: CodeCallKind::Custom,
            });
        } else {
            output_items.push(ResponseItem::FunctionCall {
                id: Some(ResponseItemId::from_server(format!(
                    "fc_{response_id}_{index}"
                ))),
                name: name.clone().into_boxed_str(),
                namespace: None,
                arguments: arguments.clone().into_boxed_str(),
                call_id: call_id.clone().into_boxed_str(),
                caller: None,
                status: Some(ItemStatus::Completed),
                created_by: Some("kimi".into()),
                internal_chat_message_metadata_passthrough: None,
            });
            code_calls.push(CodeCall {
                call_id,
                name,
                namespace: None,
                input: arguments,
                kind: CodeCallKind::Function,
            });
        }
    }
    if output_items.is_empty() {
        return Err(KimiError::InvalidResponse(
            "Kimi completed without assistant text or a tool call".to_owned(),
        ));
    }
    let usage = usage.map(|usage| {
        let cached_tokens = usage
            .prompt_tokens_details
            .map_or(usage.cached_tokens, |details| details.cached_tokens);
        Usage {
            input_tokens: usage.prompt_tokens,
            input_tokens_details: Some(InputTokenDetails {
                cached_tokens,
                cache_write_tokens: 0,
            }),
            output_tokens: usage.completion_tokens,
            output_tokens_details: usage.completion_tokens_details.map(|details| {
                OutputTokenDetails {
                    reasoning_tokens: details.reasoning_tokens,
                }
            }),
            total_tokens: usage.total_tokens,
        }
    });
    Ok(ServiceTurnResult {
        id: response_id.to_owned(),
        status: "completed".to_owned(),
        end_turn: Some(code_calls.is_empty()),
        final_message: code_calls.is_empty().then_some(content),
        output_items,
        code_calls,
        usage,
        time_to_first_event_ns: 0,
        time_to_first_output_ns: None,
        pipeline_stats: ResponsePipelineStats::default(),
    })
}

#[derive(Serialize)]
struct AssistantMessageEvent<'a> {
    model_call_index: u32,
    item_id: &'a str,
    phase: MessagePhase,
    text: &'a str,
}

fn emit_api_event(
    events: &EventSink,
    call_index: u32,
    direction: &'static str,
    event: &str,
) -> Result<(), KimiError> {
    let event = RawValue::from_string(event.to_owned())
        .or_else(|_| RawValue::from_string(json!({ "raw_body": event }).to_string()))
        .map_err(KimiError::DecodeResponse)?;
    events.emit(
        AgentEventKind::ApiEvent,
        KimiApiEvent {
            direction,
            transport: "kimi_chat_https",
            phase: "generation",
            model_call_index: call_index,
            event: &event,
        },
    )?;
    Ok(())
}

#[derive(Serialize)]
struct KimiApiEvent<'a> {
    direction: &'static str,
    transport: &'static str,
    phase: &'static str,
    model_call_index: u32,
    event: &'a RawValue,
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanocodex_core::{CustomToolFormat, JsonSchema};

    #[test]
    fn fallback_configuration_is_explicit_and_redacts_credentials() {
        let fallback = KimiRefusalFallback::new("super-secret-kimi-key")
            .reasoning_effort("low")
            .max_lease_generations(8);

        fallback.validate().unwrap();
        let debug = format!("{fallback:?}");
        assert!(!debug.contains("super-secret-kimi-key"));
        assert!(debug.contains("[redacted]"));
        assert!(debug.contains(KIMI_FALLBACK_MODEL));

        assert!(
            KimiRefusalFallback::new("key")
                .reasoning_effort("medium")
                .validate()
                .is_err()
        );
        assert!(
            KimiRefusalFallback::new("key")
                .max_lease_generations(0)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn code_mode_exec_is_wrapped_as_a_function() {
        let prefix = vec![ResponseItem::additional_tools(vec![
            ToolDefinition::custom(
                "exec",
                "run code",
                CustomToolFormat::grammar("lark", "start: /.+/"),
            ),
            ToolDefinition::Function {
                name: "wait".into(),
                description: "wait".into(),
                strict: false,
                parameters: JsonSchema::from(json!({
                    "type": "object",
                    "properties": {"cell_id": {"type": "string"}}
                })),
                output_schema: None,
            },
        ])];

        let tools = convert_tools(&prefix).unwrap();
        assert_eq!(
            serde_json::to_value(&tools).unwrap()[0]["function"]["name"],
            "exec"
        );
        assert_eq!(
            serde_json::to_value(&tools).unwrap()[0]["function"]["parameters"]["required"][0],
            "source"
        );
    }

    #[test]
    fn transcript_preserves_exact_kimi_reasoning_between_calls() {
        let prefix = vec![
            ResponseItem::additional_tools(vec![ToolDefinition::custom(
                "exec",
                "run code",
                CustomToolFormat::grammar("lark", "start: /.+/"),
            )]),
            ResponseItem::message(
                MessageRole::Developer,
                [ContentItem::InputText {
                    text: "instructions".into(),
                }],
            ),
        ];
        let mut history = vec![ResponseItem::message(
            MessageRole::User,
            [ContentItem::InputText {
                text: "do work".into(),
            }],
        )];
        let mut transcript = None;
        let prepared = prepare_transcript(&mut transcript, &prefix, &history, 0).unwrap();
        prepared.messages.push(ChatMessage {
            role: "assistant".to_owned(),
            content: Some(String::new()),
            reasoning_content: Some("private reasoning".to_owned()),
            tool_calls: vec![ChatToolCall {
                index: Some(0),
                id: "exec_0".to_owned(),
                kind: "function".to_owned(),
                function: ChatFunctionCall {
                    name: "exec".to_owned(),
                    arguments: r#"{"source":"text(\"ok\")"}"#.to_owned(),
                },
            }],
            tool_call_id: None,
            name: None,
        });
        prepared.consumed_history_len = 2;
        history.push(ResponseItem::CustomToolCall {
            id: Some(ResponseItemId::from_server("ctc-kimi")),
            status: Some(ItemStatus::Completed),
            call_id: "exec_0".into(),
            name: "exec".into(),
            namespace: None,
            input: "text(\"ok\")".into(),
            caller: None,
            created_by: Some("kimi".into()),
            internal_chat_message_metadata_passthrough: None,
        });
        history.push(ResponseItem::custom_tool_output(
            "exec_0".to_owned(),
            None,
            FunctionOutputBody::Text("ok".into()),
        ));

        let prepared = prepare_transcript(&mut transcript, &prefix, &history, 0).unwrap();
        assert_eq!(
            prepared
                .messages
                .iter()
                .find_map(|message| message.reasoning_content.as_deref()),
            Some("private reasoning")
        );
        assert_eq!(prepared.messages.last().unwrap().role, "tool");
    }
}
