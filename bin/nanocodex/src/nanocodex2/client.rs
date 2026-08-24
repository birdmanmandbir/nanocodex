//! Typed client for account-owned managed Nanocodex agents.

use std::{fmt, time::Duration};

use nanocodex::agent::events::AgentEvent;
use reqwest::{
    Method, Response, StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::time::sleep;
use url::Url;
use zeroize::Zeroize;

const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const DEFAULT_RECONNECT_DELAY: Duration = Duration::from_secs(1);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(30);
const MAX_HISTORY_PAGE: u16 = 256;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedError {
    #[error("{0}")]
    Configuration(String),
    #[error("managed request failed")]
    Transport(#[source] reqwest::Error),
    #[error("managed request failed ({status}): {code}: {message}")]
    Http {
        status: StatusCode,
        code: String,
        message: String,
    },
    #[error("managed response exceeded {MAX_RESPONSE_BYTES} bytes")]
    ResponseTooLarge,
    #[error("managed response is malformed: {0}")]
    InvalidResponse(&'static str),
    #[error("managed event stream is malformed: {0}")]
    InvalidEvent(String),
    #[error("managed turn {turn_id} {state}: {message}")]
    Turn {
        turn_id: String,
        state: String,
        message: String,
    },
}

pub(crate) struct ApiKey(String);

impl ApiKey {
    pub(crate) fn parse(value: String) -> Result<Self, ManagedError> {
        if !valid_api_key(&value) {
            return Err(ManagedError::Configuration(
                "NANOCODEX_API_KEY must be an ncx_live account API key".to_owned(),
            ));
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ApiKey([REDACTED])")
    }
}

impl Drop for ApiKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

fn valid_api_key(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("ncx_live_") else {
        return false;
    };
    let Some((id, secret)) = rest.split_once('_') else {
        return false;
    };
    id.len() == 12
        && secret.len() == 43
        && id.bytes().all(base64url_byte)
        && secret.bytes().all(base64url_byte)
}

fn base64url_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')
}

#[derive(Clone)]
pub(crate) struct ManagedClient {
    http: reqwest::Client,
    base_url: Url,
}

impl fmt::Debug for ManagedClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedClient")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl ManagedClient {
    pub(crate) fn new(mut base_url: Url, api_key: ApiKey) -> Result<Self, ManagedError> {
        nanocodex::oai::transport::install_default_rustls_crypto_provider();
        if !matches!(base_url.scheme(), "http" | "https")
            || !base_url.username().is_empty()
            || base_url.password().is_some()
            || base_url.query().is_some()
            || base_url.fragment().is_some()
            || !matches!(base_url.path(), "" | "/")
        {
            return Err(ManagedError::Configuration(
                "NANOCODEX_MANAGED_URL must be an HTTP(S) origin".to_owned(),
            ));
        }
        base_url.set_path("/");

        let mut bearer = b"Bearer ".to_vec();
        bearer.extend_from_slice(api_key.0.as_bytes());
        let mut authorization = HeaderValue::from_bytes(&bearer)
            .map_err(|_| ManagedError::Configuration("NANOCODEX_API_KEY is invalid".to_owned()))?;
        bearer.zeroize();
        authorization.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, authorization);
        let http = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(ManagedError::Transport)?;
        // `reqwest` retains a non-zeroizing header copy for the client lifetime.
        // The application-owned source string is zeroized here when `api_key` drops.
        drop(api_key);
        Ok(Self { http, base_url })
    }

    pub(crate) async fn create(&self) -> Result<AgentReceipt, ManagedError> {
        self.json(Method::POST, "v1/agents", None, None).await
    }

    pub(crate) async fn list(&self) -> Result<AgentList, ManagedError> {
        self.json(Method::GET, "v1/agents", None, None).await
    }

    pub(crate) async fn state(&self, agent_id: &str) -> Result<AgentState, ManagedError> {
        validate_id("agent", agent_id)?;
        self.json(Method::GET, &agent_path(agent_id), None, None)
            .await
    }

    pub(crate) async fn delete(&self, agent_id: &str) -> Result<(), ManagedError> {
        validate_id("agent", agent_id)?;
        let response = self
            .request(Method::DELETE, &agent_path(agent_id), None, None)
            .await?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        Ok(())
    }

    pub(crate) async fn find_sessions(
        &self,
        request: &FindSessionsRequest,
    ) -> Result<FindSessionsResponse, ManagedError> {
        request.validate()?;
        let body = serde_json::to_vec(request)
            .map_err(|_| ManagedError::InvalidResponse("failed to encode session search"))?;
        self.json(
            Method::POST,
            "/v1/history/sessions/search",
            Some(&body),
            None,
        )
        .await
    }

    pub(crate) async fn read_session(
        &self,
        request: &ReadSessionRequest,
    ) -> Result<ReadSessionResponse, ManagedError> {
        request.validate()?;
        let body = serde_json::to_vec(&ReadSessionBody {
            turn_ids: request.turn_ids.as_deref(),
        })
        .map_err(|_| ManagedError::InvalidResponse("failed to encode session read"))?;
        self.json(
            Method::POST,
            &format!("/v1/history/sessions/{}/read", request.session_id),
            Some(&body),
            None,
        )
        .await
    }

    pub(crate) async fn list_memories(&self) -> Result<Vec<MemoryRecord>, ManagedError> {
        let response: MemoryListResponse = self.json(Method::GET, "/v1/memory", None, None).await?;
        for memory in &response.memories {
            memory.key.validate()?;
        }
        Ok(response.memories)
    }

    pub(crate) async fn delete_memory(&self, key: MemoryKey) -> Result<(), ManagedError> {
        key.validate()?;
        let mut url = self.url(&format!("/v1/memory/{}", key.id))?;
        url.query_pairs_mut()
            .append_pair("version", &key.version.to_string());
        let response = self
            .http
            .delete(url)
            .send()
            .await
            .map_err(ManagedError::Transport)?;
        if !response.status().is_success() {
            return Err(response_error(response).await);
        }
        Ok(())
    }

    pub(crate) async fn history(
        &self,
        agent_id: &str,
        before: Option<&str>,
        limit: u16,
    ) -> Result<EventHistoryPage, ManagedError> {
        validate_id("agent", agent_id)?;
        if limit == 0 || limit > MAX_HISTORY_PAGE {
            return Err(ManagedError::Configuration(
                "managed history limit must be from 1 through 256".to_owned(),
            ));
        }
        if let Some(cursor) = before {
            validate_numeric_cursor(cursor)?;
            if cursor == "0" {
                return Err(ManagedError::Configuration(
                    "managed history cursor must be positive".to_owned(),
                ));
            }
        }
        let mut url = self.url(&format!("{}/events/history", agent_path(agent_id)))?;
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("limit", &limit.to_string());
            if let Some(cursor) = before {
                query.append_pair("before", cursor);
            }
        }
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(ManagedError::Transport)?;
        let page: EventHistoryPage = decode_response(response).await?;
        validate_numeric_cursor(&page.latest_cursor)?;
        if page.data.len() > limit as usize {
            return Err(ManagedError::InvalidResponse(
                "history page exceeds the requested limit",
            ));
        }
        let mut previous = None;
        for event in &page.data {
            validate_numeric_cursor(&event.cursor)?;
            if previous.is_some_and(|cursor| !cursor_before(cursor, &event.cursor))
                || before.is_some_and(|cursor| !cursor_before(&event.cursor, cursor))
            {
                return Err(ManagedError::InvalidResponse(
                    "history events are not strictly ordered",
                ));
            }
            previous = Some(event.cursor.as_str());
        }
        Ok(page)
    }

    pub(crate) async fn submit(
        &self,
        agent_id: &str,
        turn_id: Option<&str>,
        idempotency_key: &str,
        input: &PromptInput,
    ) -> Result<TurnView, ManagedError> {
        validate_id("agent", agent_id)?;
        if let Some(turn_id) = turn_id {
            validate_id("turn", turn_id)?;
        }
        validate_idempotency_key(idempotency_key)?;
        let body = serde_json::to_vec(&TurnSubmission { id: turn_id, input })
            .map_err(|_| ManagedError::InvalidResponse("failed to encode prompt"))?;
        let path = format!("{}/turns", agent_path(agent_id));
        let mut last_transport = None;
        for _ in 0..3 {
            match self
                .request(Method::POST, &path, Some(&body), Some(idempotency_key))
                .await
            {
                Ok(response) => return decode_response(response).await,
                Err(ManagedError::Transport(error)) => last_transport = Some(error),
                Err(error) => return Err(error),
            }
        }
        Err(ManagedError::Transport(last_transport.ok_or(
            ManagedError::InvalidResponse("submission retry lost its transport error"),
        )?))
    }

    pub(crate) async fn turn_state(
        &self,
        agent_id: &str,
        turn_id: &str,
    ) -> Result<TurnView, ManagedError> {
        validate_id("agent", agent_id)?;
        validate_id("turn", turn_id)?;
        self.json(
            Method::GET,
            &format!("{}/turns/{turn_id}", agent_path(agent_id)),
            None,
            None,
        )
        .await
    }

    pub(crate) async fn steer(
        &self,
        agent_id: &str,
        turn_id: &str,
        input: &PromptInput,
    ) -> Result<TurnAction, ManagedError> {
        self.turn_action(agent_id, turn_id, "steer", Some(input))
            .await
    }

    pub(crate) async fn cancel(
        &self,
        agent_id: &str,
        turn_id: &str,
    ) -> Result<TurnAction, ManagedError> {
        self.turn_action(agent_id, turn_id, "cancel", None).await
    }

    async fn turn_action(
        &self,
        agent_id: &str,
        turn_id: &str,
        action: &str,
        input: Option<&PromptInput>,
    ) -> Result<TurnAction, ManagedError> {
        validate_id("agent", agent_id)?;
        validate_id("turn", turn_id)?;
        let body = input
            .map(|input| serde_json::to_vec(&TurnSteer { input }))
            .transpose()
            .map_err(|_| ManagedError::InvalidResponse("failed to encode steer"))?;
        self.json(
            Method::POST,
            &format!("{}/turns/{turn_id}/{action}", agent_path(agent_id)),
            body.as_deref(),
            None,
        )
        .await
    }

    pub(crate) fn events(
        &self,
        agent_id: &str,
        cursor: EventCursor,
    ) -> Result<ManagedEventStream, ManagedError> {
        validate_id("agent", agent_id)?;
        Ok(ManagedEventStream {
            client: self.clone(),
            agent_id: agent_id.to_owned(),
            cursor,
            reconnect_delay: DEFAULT_RECONNECT_DELAY,
            response: None,
            buffer: Vec::new(),
        })
    }

    async fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        idempotency_key: Option<&str>,
    ) -> Result<T, ManagedError> {
        let response = self.request(method, path, body, idempotency_key).await?;
        decode_response(response).await
    }

    async fn request(
        &self,
        method: Method,
        path: &str,
        body: Option<&[u8]>,
        idempotency_key: Option<&str>,
    ) -> Result<Response, ManagedError> {
        let mut request = self.http.request(method, self.url(path)?);
        if let Some(body) = body {
            request = request
                .header(CONTENT_TYPE, "application/json")
                .body(body.to_vec());
        }
        if let Some(key) = idempotency_key {
            request = request.header("idempotency-key", key);
        }
        request.send().await.map_err(ManagedError::Transport)
    }

    fn url(&self, path: &str) -> Result<Url, ManagedError> {
        self.base_url
            .join(path)
            .map_err(|_| ManagedError::InvalidResponse("invalid managed route"))
    }
}

async fn decode_response<T: DeserializeOwned>(response: Response) -> Result<T, ManagedError> {
    if !response.status().is_success() {
        return Err(response_error(response).await);
    }
    let bytes = bounded_body(response).await?;
    serde_json::from_slice(&bytes).map_err(|_| ManagedError::InvalidResponse("invalid JSON"))
}

async fn response_error(response: Response) -> ManagedError {
    let status = response.status();
    let body = bounded_body(response).await.ok();
    let parsed = body
        .as_deref()
        .and_then(|body| serde_json::from_slice::<ErrorBody>(body).ok());
    ManagedError::Http {
        status,
        code: parsed
            .as_ref()
            .and_then(|body| body.error.clone())
            .unwrap_or_else(|| format!("http_{}", status.as_u16())),
        message: parsed
            .and_then(|body| body.message)
            .unwrap_or_else(|| format!("managed request failed ({})", status.as_u16())),
    }
}

async fn bounded_body(response: Response) -> Result<Vec<u8>, ManagedError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(ManagedError::ResponseTooLarge);
    }
    let bytes = response.bytes().await.map_err(ManagedError::Transport)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        return Err(ManagedError::ResponseTooLarge);
    }
    Ok(bytes.to_vec())
}

#[derive(Deserialize)]
struct ErrorBody {
    error: Option<String>,
    message: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub(crate) enum PromptInput {
    Text(String),
    Content(Vec<PromptContent>),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum PromptContent {
    Text {
        text: String,
    },
    Image {
        image_url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Audio {
        audio_url: String,
    },
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AgentReceipt {
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) events_url: String,
    pub(crate) websocket_url: String,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AgentList {
    pub(crate) data: Vec<String>,
    #[serde(default)]
    pub(crate) summaries: std::collections::BTreeMap<String, AgentSummary>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AgentSummary {
    pub(crate) title: String,
    pub(crate) created_at: f64,
    pub(crate) updated_at: f64,
    pub(crate) turn_count: u64,
}

#[derive(Clone, Debug, Serialize)]
pub(crate) struct FindSessionsRequest {
    pub(crate) query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<u8>,
}

impl FindSessionsRequest {
    fn validate(&self) -> Result<(), ManagedError> {
        if self.query.trim().is_empty() || self.query.len() > 4_096 {
            return Err(ManagedError::Configuration(
                "managed history query must contain 1-4096 UTF-8 bytes".to_owned(),
            ));
        }
        if self.limit.is_some_and(|limit| !(1..=20).contains(&limit)) {
            return Err(ManagedError::Configuration(
                "managed history limit must be from 1 through 20".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ReadSessionRequest {
    pub(crate) session_id: String,
    pub(crate) turn_ids: Option<Vec<String>>,
}

impl ReadSessionRequest {
    fn validate(&self) -> Result<(), ManagedError> {
        let session_id = uuid::Uuid::parse_str(&self.session_id).map_err(|_| {
            ManagedError::Configuration("managed history session id must be a UUIDv7".to_owned())
        })?;
        if session_id.get_version_num() != 7 {
            return Err(ManagedError::Configuration(
                "managed history session id must be a UUIDv7".to_owned(),
            ));
        }
        if self.turn_ids.as_ref().is_some_and(|ids| ids.len() > 20) {
            return Err(ManagedError::Configuration(
                "managed history turn ids must contain at most 20 entries".to_owned(),
            ));
        }
        for turn_id in self.turn_ids.iter().flatten() {
            validate_id("turn", turn_id)?;
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ReadSessionBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    turn_ids: Option<&'a [String]>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct HistorySource {
    pub(crate) turn_id: String,
    pub(crate) cursor: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct HistoryCitation {
    pub(crate) thread_id: String,
    pub(crate) title: String,
    pub(crate) sources: Vec<HistorySource>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SessionSearchHit {
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) turn_id: String,
    pub(crate) cursor: String,
    pub(crate) score: f64,
    pub(crate) snippet: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct FindSessionsResponse {
    pub(crate) query: String,
    pub(crate) results: Vec<SessionSearchHit>,
    pub(crate) citations: Vec<HistoryCitation>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct SessionTurn {
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) turn_id: String,
    pub(crate) cursor: String,
    pub(crate) user: String,
    pub(crate) assistant: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ReadSessionResponse {
    pub(crate) turns: Vec<SessionTurn>,
    pub(crate) citations: Vec<HistoryCitation>,
}

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct MemoryKey {
    pub(crate) id: u64,
    pub(crate) version: u64,
}

impl MemoryKey {
    fn validate(self) -> Result<(), ManagedError> {
        if self.id == 0
            || self.version == 0
            || self.id > MAX_SAFE_INTEGER
            || self.version > MAX_SAFE_INTEGER
        {
            return Err(ManagedError::Configuration(
                "managed memory id and version must be positive safe integers".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct MemoryRecord {
    pub(crate) key: MemoryKey,
    pub(crate) content: String,
    pub(crate) created_at_ms: i64,
    pub(crate) updated_at_ms: i64,
    pub(crate) last_scanned_at_ms: Option<i64>,
    pub(crate) scan_count: u64,
    pub(crate) last_used_at_ms: Option<i64>,
    pub(crate) use_count: u64,
    pub(crate) probation_until_ms: Option<i64>,
}

#[derive(Deserialize)]
struct MemoryListResponse {
    memories: Vec<MemoryRecord>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AgentCapabilities {
    pub(crate) durable_turns: bool,
    pub(crate) resumable_events: bool,
    pub(crate) live_steer: bool,
    pub(crate) live_cancel: bool,
    pub(crate) workspace: String,
    pub(crate) sandbox_escalation: bool,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ActiveTurn {
    pub(crate) id: String,
    pub(crate) input: PromptInput,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct AgentState {
    pub(crate) agent_id: String,
    pub(crate) session_id: String,
    pub(crate) has_snapshot: bool,
    pub(crate) completed_turns: u64,
    pub(crate) last_active: f64,
    pub(crate) active_turns: Vec<String>,
    pub(crate) active_turn_details: Vec<ActiveTurn>,
    pub(crate) agent_loaded: bool,
    pub(crate) connected_clients: u64,
    pub(crate) capabilities: AgentCapabilities,
    pub(crate) latest_event_cursor: String,
    pub(crate) stream_error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TurnState {
    Accepted,
    Cancelling,
    Retryable,
    Blocked,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TurnView {
    pub(crate) turn_id: String,
    pub(crate) state: TurnState,
    pub(crate) input: PromptInput,
    pub(crate) accepted_cursor: String,
    pub(crate) terminal_cursor: Option<String>,
    pub(crate) created_at: f64,
    pub(crate) accepted_at: f64,
    pub(crate) updated_at: f64,
    pub(crate) attempt_count: u64,
    pub(crate) retry_at: Option<f64>,
    pub(crate) error: Option<String>,
    pub(crate) terminal: Option<ManagedEventData>,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct TurnAction {
    pub(crate) turn_id: String,
    pub(crate) state: String,
}

#[derive(Serialize)]
struct TurnSubmission<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<&'a str>,
    input: &'a PromptInput,
}

#[derive(Serialize)]
struct TurnSteer<'a> {
    input: &'a PromptInput,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct EventHistoryPage {
    pub(crate) data: Vec<ManagedEvent>,
    pub(crate) has_more: bool,
    pub(crate) latest_cursor: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct ManagedEvent {
    pub(crate) cursor: String,
    pub(crate) created_at: Option<f64>,
    pub(crate) turn_id: Option<String>,
    #[serde(flatten)]
    pub(crate) data: ManagedEventData,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ManagedEventData {
    AgentCreated {
        agent_id: String,
        capabilities: AgentCapabilitiesValue,
    },
    TurnAccepted {
        id: String,
        input: PromptInput,
        replayed: bool,
    },
    TurnCancelling {
        id: String,
        error: Option<String>,
        retry_at: Option<f64>,
    },
    TurnCompleted {
        id: String,
        final_message: String,
        usage: Option<Value>,
        usage_error: Option<String>,
    },
    TurnCancelled {
        id: String,
    },
    TurnRetryable {
        id: String,
        error: String,
    },
    TurnBlocked {
        id: String,
        error: String,
    },
    TurnFailed {
        id: String,
        error: String,
    },
    Event {
        // `AgentEvent` retains its payload as `RawValue`. Internally tagged
        // enum deserialization buffers fields through `Value`, so keep this
        // nested object intact and decode it through a raw JSON string at the
        // explicit typed boundary below.
        event: Value,
    },
    StreamFailed {
        error: String,
    },
}

// Capabilities are retained as exact JSON in historical agent-created events so
// new server flags do not make old transcript hydration fail.
type AgentCapabilitiesValue = Value;

impl ManagedEventData {
    pub(crate) fn agent_event(&self) -> Result<Option<AgentEvent>, ManagedError> {
        let Self::Event { event } = self else {
            return Ok(None);
        };
        let encoded = serde_json::to_string(event).map_err(|_| {
            ManagedError::InvalidEvent("agent event could not be encoded".to_owned())
        })?;
        serde_json::from_str(&encoded)
            .map(Some)
            .map_err(|error| ManagedError::InvalidEvent(format!("invalid agent event: {error}")))
    }

    pub(crate) fn turn_id(&self) -> Option<&str> {
        match self {
            Self::TurnAccepted { id, .. }
            | Self::TurnCancelling { id, .. }
            | Self::TurnCompleted { id, .. }
            | Self::TurnCancelled { id }
            | Self::TurnRetryable { id, .. }
            | Self::TurnBlocked { id, .. }
            | Self::TurnFailed { id, .. } => Some(id),
            Self::AgentCreated { .. } | Self::Event { .. } | Self::StreamFailed { .. } => None,
        }
    }

    pub(crate) fn terminal_result(&self, turn_id: &str) -> Option<Result<String, ManagedError>> {
        match self {
            Self::TurnCompleted {
                id, final_message, ..
            } if id == turn_id => Some(Ok(final_message.clone())),
            Self::TurnCancelled { id } if id == turn_id => Some(Err(ManagedError::Turn {
                turn_id: id.clone(),
                state: "cancelled".to_owned(),
                message: "managed turn was cancelled".to_owned(),
            })),
            Self::TurnBlocked { id, error } if id == turn_id => Some(Err(ManagedError::Turn {
                turn_id: id.clone(),
                state: "blocked".to_owned(),
                message: error.clone(),
            })),
            Self::TurnFailed { id, error } if id == turn_id => Some(Err(ManagedError::Turn {
                turn_id: id.clone(),
                state: "failed".to_owned(),
                message: error.clone(),
            })),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum EventCursor {
    Latest,
    At(String),
}

impl EventCursor {
    pub(crate) fn parse(value: impl Into<String>) -> Result<Self, ManagedError> {
        let value = value.into();
        if value == "latest" {
            return Ok(Self::Latest);
        }
        validate_numeric_cursor(&value)?;
        Ok(Self::At(value))
    }

    fn as_str(&self) -> &str {
        match self {
            Self::Latest => "latest",
            Self::At(cursor) => cursor,
        }
    }

    fn observe(&mut self, cursor: String) -> Result<bool, ManagedError> {
        validate_numeric_cursor(&cursor)?;
        let is_new = match self {
            Self::Latest => true,
            Self::At(previous) => cursor_before(previous, &cursor),
        };
        if is_new {
            *self = Self::At(cursor);
        }
        Ok(is_new)
    }
}

pub(crate) struct ManagedEventStream {
    client: ManagedClient,
    agent_id: String,
    cursor: EventCursor,
    reconnect_delay: Duration,
    response: Option<Response>,
    buffer: Vec<u8>,
}

impl ManagedEventStream {
    pub(crate) async fn next(&mut self) -> Result<ManagedEvent, ManagedError> {
        loop {
            if let Some(frame) = take_sse_frame(&mut self.buffer) {
                let parsed = parse_sse_frame(&frame)?;
                if let Some(delay) = parsed.retry {
                    self.reconnect_delay = delay.min(MAX_RECONNECT_DELAY);
                }
                if let Some(control_cursor) = parsed.control_cursor {
                    self.cursor.observe(control_cursor)?;
                }
                let Some(data) = parsed.data else {
                    continue;
                };
                let event: ManagedEvent = serde_json::from_str(&data).map_err(|error| {
                    ManagedError::InvalidEvent(format!("event data is not typed JSON: {error}"))
                })?;
                let cursor = parsed.id.unwrap_or_else(|| event.cursor.clone());
                if event.cursor != cursor {
                    return Err(ManagedError::InvalidEvent(
                        "SSE id does not match the durable event cursor".to_owned(),
                    ));
                }
                if !self.cursor.observe(cursor)? {
                    continue;
                }
                return Ok(event);
            }

            if self.response.is_none() {
                self.connect().await?;
            }
            let chunk = match self.response.as_mut() {
                Some(response) => response.chunk().await,
                None => continue,
            };
            match chunk {
                Ok(Some(bytes)) => self.buffer.extend_from_slice(&bytes),
                Ok(None) | Err(_) => {
                    self.response = None;
                    sleep(self.reconnect_delay).await;
                }
            }
        }
    }

    async fn connect(&mut self) -> Result<(), ManagedError> {
        let mut url = self
            .client
            .url(&format!("{}/events", agent_path(&self.agent_id)))?;
        url.query_pairs_mut()
            .append_pair("cursor", self.cursor.as_str());
        loop {
            match self
                .client
                .http
                .get(url.clone())
                .header(ACCEPT, "text/event-stream")
                .send()
                .await
            {
                Ok(response) if response.status().is_success() => {
                    self.response = Some(response);
                    return Ok(());
                }
                Ok(response)
                    if response.status() == StatusCode::TOO_MANY_REQUESTS
                        || response.status().is_server_error() =>
                {
                    drop(response);
                }
                Ok(response) => return Err(response_error(response).await),
                Err(_) => {}
            }
            sleep(self.reconnect_delay).await;
        }
    }
}

struct ParsedSseFrame {
    id: Option<String>,
    retry: Option<Duration>,
    control_cursor: Option<String>,
    data: Option<String>,
}

fn take_sse_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let (index, delimiter) = find_sse_boundary(buffer)?;
    let frame = buffer[..index].to_vec();
    buffer.drain(..index + delimiter);
    Some(frame)
}

fn find_sse_boundary(buffer: &[u8]) -> Option<(usize, usize)> {
    for (index, window) in buffer.windows(2).enumerate() {
        if matches!(window, b"\n\n" | b"\r\r") {
            return Some((index, 2));
        }
    }
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| (index, 4))
}

fn parse_sse_frame(frame: &[u8]) -> Result<ParsedSseFrame, ManagedError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|_| ManagedError::InvalidEvent("SSE frame is not UTF-8".to_owned()))?;
    let normalized = frame.replace("\r\n", "\n").replace('\r', "\n");
    let mut id = None;
    let mut retry = None;
    let mut control_cursor = None;
    let mut data = Vec::new();
    for line in normalized.lines() {
        if let Some(comment) = line.strip_prefix(':') {
            if let Some(cursor) = comment.trim_start().strip_prefix("cursor ") {
                validate_numeric_cursor(cursor)?;
                control_cursor = Some(cursor.to_owned());
            }
            continue;
        }
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "id" if !value.contains('\0') => {
                validate_numeric_cursor(value)?;
                id = Some(value.to_owned());
            }
            "retry" if value.bytes().all(|byte| byte.is_ascii_digit()) => {
                if let Ok(milliseconds) = value.parse::<u64>() {
                    retry = Some(Duration::from_millis(milliseconds));
                }
            }
            "data" => data.push(value),
            _ => {}
        }
    }
    Ok(ParsedSseFrame {
        id,
        retry,
        control_cursor,
        data: (!data.is_empty()).then(|| data.join("\n")),
    })
}

fn validate_id(kind: &str, value: &str) -> Result<(), ManagedError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(ManagedError::Configuration(format!(
            "managed {kind} id must be 1-128 safe ASCII characters"
        )));
    }
    Ok(())
}

fn validate_idempotency_key(value: &str) -> Result<(), ManagedError> {
    if value.is_empty()
        || value.len() > 256
        || !value.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return Err(ManagedError::Configuration(
            "managed idempotency key must be 1-256 visible ASCII characters".to_owned(),
        ));
    }
    Ok(())
}

fn validate_numeric_cursor(value: &str) -> Result<(), ManagedError> {
    if value.is_empty()
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return Err(ManagedError::Configuration(
            "managed cursor must be an unsigned decimal string".to_owned(),
        ));
    }
    Ok(())
}

fn cursor_before(left: &str, right: &str) -> bool {
    left.len() < right.len() || (left.len() == right.len() && left < right)
}

fn agent_path(agent_id: &str) -> String {
    format!("v1/agents/{agent_id}")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    use axum::{
        Router,
        body::Body,
        extract::{Query, State},
        http::{HeaderMap, Response, StatusCode},
        response::IntoResponse,
        routing::get,
    };

    use super::{
        ApiKey, EventCursor, FindSessionsRequest, ManagedClient, ManagedEventData, MemoryKey,
        ReadSessionRequest, parse_sse_frame, valid_api_key,
    };

    fn key() -> String {
        format!("ncx_live_{}_{}", "a".repeat(12), "b".repeat(43))
    }

    #[test]
    fn validates_account_api_keys_exactly() {
        assert!(valid_api_key(&key()));
        assert!(!valid_api_key("sk-provider-key"));
        assert!(!valid_api_key(&format!(
            "ncx_live_{}_{}",
            "a".repeat(11),
            "b".repeat(43)
        )));
    }

    #[test]
    fn api_key_debug_is_redacted() {
        let secret = key();
        let key = ApiKey::parse(secret.clone()).unwrap();
        let debug = format!("{key:?}");
        assert_eq!(debug, "ApiKey([REDACTED])");
        assert!(!debug.contains(&secret));
    }

    #[test]
    fn parses_multiline_sse_and_control_cursor() {
        let frame = b": cursor 7\r\nid: 8\r\nretry: 25\r\ndata: {\"cursor\":\"8\",\r\ndata: \"type\":\"turn_cancelled\",\"id\":\"turn-1\"}\r\n";
        let parsed = parse_sse_frame(frame).unwrap();
        assert_eq!(parsed.id.as_deref(), Some("8"));
        assert_eq!(parsed.control_cursor.as_deref(), Some("7"));
        assert_eq!(parsed.retry.unwrap().as_millis(), 25);
        let data: serde_json::Value =
            serde_json::from_str(parsed.data.as_deref().unwrap()).unwrap();
        assert_eq!(data["type"], "turn_cancelled");
    }

    #[test]
    fn decimal_cursor_order_does_not_use_integer_width() {
        let mut cursor = EventCursor::parse("9").unwrap();
        assert!(cursor.observe("10".to_owned()).unwrap());
        assert!(!cursor.observe("2".to_owned()).unwrap());
    }

    #[test]
    fn validates_account_history_requests_before_network_io() {
        assert!(
            FindSessionsRequest {
                query: " ".to_owned(),
                limit: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            FindSessionsRequest {
                query: "memory".to_owned(),
                limit: Some(21),
            }
            .validate()
            .is_err()
        );
        let session_id = uuid::Uuid::now_v7().to_string();
        assert!(
            ReadSessionRequest {
                session_id,
                turn_ids: Some(vec!["turn-1".to_owned(); 20]),
            }
            .validate()
            .is_ok()
        );
        assert!(
            ReadSessionRequest {
                session_id: uuid::Uuid::new_v4().to_string(),
                turn_ids: None,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn memory_keys_must_be_positive_safe_integers() {
        assert!(MemoryKey { id: 1, version: 1 }.validate().is_ok());
        assert!(MemoryKey { id: 0, version: 1 }.validate().is_err());
        assert!(
            MemoryKey {
                id: 9_007_199_254_740_992,
                version: 1,
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn terminal_result_is_typed() {
        let completed = ManagedEventData::TurnCompleted {
            id: "turn-1".to_owned(),
            final_message: "done".to_owned(),
            usage: None,
            usage_error: None,
        };
        assert_eq!(
            completed.terminal_result("turn-1").unwrap().unwrap(),
            "done"
        );
    }

    #[derive(Clone)]
    struct StreamState {
        expected_authorization: String,
        cursors: Arc<Mutex<Vec<String>>>,
    }

    #[tokio::test]
    async fn reconnects_strictly_after_the_last_durable_cursor() {
        let secret = key();
        let state = StreamState {
            expected_authorization: format!("Bearer {secret}"),
            cursors: Arc::new(Mutex::new(Vec::new())),
        };
        let app = Router::new()
            .route("/v1/agents/{agent_id}/events", get(stream_events))
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = ManagedClient::new(
            format!("http://{address}").parse().unwrap(),
            ApiKey::parse(secret.clone()).unwrap(),
        )
        .unwrap();
        let mut events = client
            .events("agent-1", EventCursor::parse("1").unwrap())
            .unwrap();
        let first = events.next().await.unwrap();
        assert_eq!(first.cursor, "2");
        assert!(matches!(first.data, ManagedEventData::Event { .. }));
        assert_eq!(first.data.agent_event().unwrap().unwrap().seq, 1);
        let second = events.next().await.unwrap();
        assert_eq!(second.cursor, "3");
        assert_eq!(
            second.data.terminal_result("turn-1").unwrap().unwrap(),
            "done"
        );
        assert_eq!(
            state.cursors.lock().unwrap().as_slice(),
            ["1".to_owned(), "2".to_owned()]
        );
        assert!(!format!("{client:?}").contains(&secret));
        server.abort();
    }

    async fn stream_events(
        State(state): State<StreamState>,
        headers: HeaderMap,
        Query(query): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        if headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some(state.expected_authorization.as_str())
        {
            return Response::builder()
                .status(StatusCode::UNAUTHORIZED)
                .body(Body::from("unauthorized"))
                .unwrap();
        }
        let cursor = query.get("cursor").cloned().unwrap_or_default();
        state.cursors.lock().unwrap().push(cursor.clone());
        let body = match cursor.as_str() {
            "1" => concat!(
                "id: 2\n",
                "event: event\n",
                "data: {\"cursor\":\"2\",\"created_at\":1,\"turn_id\":\"turn-1\",",
                "\"type\":\"event\",\"event\":{\"protocol_version\":1,",
                "\"request_id\":\"request-1\",\"seq\":1,",
                "\"type\":\"assistant.delta\",\"payload\":{\"delta\":\"hi\"}}}\n\n"
            ),
            "2" => concat!(
                "id: 3\n",
                "event: turn_completed\n",
                "data: {\"cursor\":\"3\",\"created_at\":2,\"turn_id\":\"turn-1\",",
                "\"type\":\"turn_completed\",\"id\":\"turn-1\",",
                "\"final_message\":\"done\",\"usage\":null}\n\n"
            ),
            _ => "",
        };
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(Body::from(body))
            .unwrap()
    }
}
