use std::{
    collections::HashMap,
    convert::Infallible,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use tracing::Instrument;

use crate::{
    AdminAuthorizer, AgentManager, AuthorizationError, CreateAgent, CreateAgentResponse,
    CreateTurn, ManagerError, PaymentError, PaymentGate, PaymentOutcome, PolicyError, PolicyStore,
    TurnAction, TurnView, require,
};

/// Application-owned components shared by the REST and SSE handlers.
///
/// Fields remain private so queueing, policy, payment, and secret mechanics
/// cannot be replaced after the router starts.
pub struct ApiState {
    pub(crate) manager: Arc<AgentManager>,
    pub(crate) policy: Arc<PolicyStore>,
    pub(crate) admin: Arc<AdminAuthorizer>,
    pub(crate) payments: Arc<dyn PaymentGate>,
    idempotency_locks: IdempotencyLocks,
    agent_locks: AgentLocks,
}

impl ApiState {
    /// Creates the complete managed HTTP boundary from application-owned
    /// lifecycle, policy, authorization, and payment components.
    #[must_use]
    pub fn new(
        manager: Arc<AgentManager>,
        policy: Arc<PolicyStore>,
        admin: Arc<AdminAuthorizer>,
        payments: Arc<dyn PaymentGate>,
    ) -> Self {
        manager.attach_policy(Arc::clone(&policy));
        Self {
            manager,
            policy,
            admin,
            payments,
            idempotency_locks: IdempotencyLocks::default(),
            agent_locks: AgentLocks::default(),
        }
    }

    /// Builds the REST/SSE router owned by this state.
    pub fn router(self) -> Router {
        router(Arc::new(self))
    }

    pub(crate) async fn policy_call<T>(
        &self,
        operation: impl FnOnce(&PolicyStore) -> Result<T, PolicyError>,
    ) -> Result<T, ApiError> {
        operation(&self.policy).map_err(Into::into)
    }
}

#[derive(Default)]
struct IdempotencyLocks {
    locks: tokio::sync::Mutex<HashMap<IdempotencyScope, Weak<IdempotencyLock>>>,
}

type IdempotencyScope = (String, String);
type IdempotencyLock = tokio::sync::Mutex<()>;

#[derive(Default)]
struct AgentLocks {
    locks: tokio::sync::Mutex<HashMap<String, Weak<IdempotencyLock>>>,
}

impl IdempotencyLocks {
    async fn acquire(&self, agent_id: String, key: String) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            if locks.len() >= 1_024 {
                locks.retain(|_, lock| lock.strong_count() > 0);
            }
            let scope = (agent_id, key);
            if let Some(lock) = locks.get(&scope).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(scope, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}

impl AgentLocks {
    async fn acquire(&self, agent_id: String) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.locks.lock().await;
            if locks.len() >= 1_024 {
                locks.retain(|_, lock| lock.strong_count() > 0);
            }
            if let Some(lock) = locks.get(&agent_id).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(agent_id, Arc::downgrade(&lock));
                lock
            }
        };
        lock.lock_owned().await
    }
}

fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/v1/agent/new", post(create_agent))
        .route("/v1/agent/{agent_id}", get(get_agent).delete(delete_agent))
        .route("/v1/agent/{agent_id}/turn", post(create_turn))
        .route("/v1/agent/{agent_id}/turn/{turn_id}", get(get_turn))
        .route(
            "/v1/agent/{agent_id}/turn/{turn_id}/cancel",
            post(cancel_turn),
        )
        .route("/v1/agent/{agent_id}/events", get(agent_events))
        .route("/v1/agent/{agent_id}/fork", post(fork_latest))
        .route(
            "/v1/agent/{agent_id}/turn/{turn_id}/fork",
            post(fork_from_turn),
        )
        .route("/v1/agent/{agent_id}/evict", post(evict_agent))
        .route("/v1/payment-sessions", post(payment_session))
        .merge(crate::admin::routes())
        .layer(middleware::from_fn(trace_request))
        .with_state(state)
}

async fn trace_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let uri = request.uri().clone();
    let headers = request.headers().clone();
    let span = tracing::info_span!(
        parent: None,
        "nanocentaur.http.request",
        otel.kind = "server",
        http.request.method = %method,
        url.path = %uri.path(),
        http.response.status_code = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    );
    async move {
        tracing::info!(
            target: "nanocentaur::observed",
            http_request_uri = %uri,
            http_request_headers = ?headers,
            "http request observed"
        );
        let started = Instant::now();
        let response = next.run(request).await;
        tracing::Span::current().record("http.response.status_code", response.status().as_u16());
        tracing::Span::current().record(
            "duration_ns",
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
        );
        response
    }
    .instrument(span)
    .await
}

#[derive(Serialize)]
struct HealthResponse {
    status: HealthStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum HealthStatus {
    Ok,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: HealthStatus::Ok,
    })
}

async fn create_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Json(request): Json<CreateAgent>,
) -> Result<Response, ApiError> {
    tracing::info!(
        target: "nanocentaur::observed",
        request = ?request,
        "create agent request observed"
    );
    let context_key = request.context_key;
    let (identity, created, client) = state
        .policy_call(move |policy| {
            let client = policy.authenticate(&headers)?;
            let (identity, created) =
                policy.create_or_resolve_agent(&client, context_key.as_deref())?;
            Ok((identity, created, client))
        })
        .await?;
    let _agent_guard = state.agent_locks.acquire(identity.id.clone()).await;
    let agent_id = identity.id;
    let identity = state
        .policy_call(move |policy| policy.agent(&client, &agent_id))
        .await?;
    let view = state.manager.register(identity).await?;
    let response = CreateAgentResponse {
        agent_id: view.agent_id,
        created,
        state: view.state,
    };
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(response),
    )
        .into_response())
}

async fn get_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Json<crate::AgentView>, ApiError> {
    let _agent_guard = state.agent_locks.acquire(agent_id.clone()).await;
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.read").await?;
    Ok(Json(state.manager.get(identity).await?))
}

async fn delete_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let _agent_guard = state.agent_locks.acquire(agent_id.clone()).await;
    let headers_for_delete = headers.clone();
    let agent_for_delete = agent_id.clone();
    let client = state
        .policy_call(move |policy| {
            let client = policy.authenticate(&headers_for_delete)?;
            policy.begin_delete_agent(&client, &agent_for_delete)?;
            Ok(client)
        })
        .await?;
    state.manager.delete(&agent_id).await?;
    state
        .policy_call(move |policy| policy.finish_delete_agent(&client, &agent_id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Json(request): Json<CreateTurn>,
) -> Result<Response, ApiError> {
    tracing::info!(
        target: "nanocentaur::observed",
        agent_id = %agent_id,
        request = ?request,
        "create turn request observed"
    );
    let _agent_guard = state.agent_locks.acquire(agent_id.clone()).await;
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.turn").await?;
    let idempotency_key = idempotency_key(&headers)?;
    let _idempotency_guard = if let Some(key) = &idempotency_key {
        Some(
            state
                .idempotency_locks
                .acquire(agent_id.clone(), key.clone())
                .await,
        )
    } else {
        None
    };
    if let Some(key) = idempotency_key.as_deref()
        && let Some(replay) = state
            .manager
            .find_turn_replay(identity.clone(), key, &request)
            .await?
    {
        let status = turn_action_status(replay.response.action);
        let mut response = (status, Json(replay.response)).into_response();
        if let Some(receipt) = replay.payment_receipt {
            insert_receipt(&mut response, &receipt)?;
        }
        return Ok(response);
    }
    crate::manager::validate_turn_request(&request)?;

    let receipt = match state.payments.authorize(&headers).await? {
        PaymentOutcome::Authorized(receipt) => receipt,
        outcome => return payment_response(outcome),
    };
    let receipt_header = HeaderValue::from_str(&receipt.header_value).map_err(|error| {
        tracing::error!(%error, "payment gate returned an invalid receipt header");
        ApiError::Internal
    })?;
    let response = match state
        .manager
        .create_turn_with_receipt(
            identity,
            request,
            idempotency_key,
            Some(receipt.header_value.clone()),
        )
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let mut response = ApiError::from(error).into_response();
            response
                .headers_mut()
                .insert(HeaderName::from_static("payment-receipt"), receipt_header);
            return Ok(response);
        }
    };
    let status = turn_action_status(response.action);
    let mut response = (status, Json(response)).into_response();
    response
        .headers_mut()
        .insert(HeaderName::from_static("payment-receipt"), receipt_header);
    Ok(response)
}

const fn turn_action_status(action: TurnAction) -> StatusCode {
    match action {
        TurnAction::Steered => StatusCode::OK,
        TurnAction::Started | TurnAction::Queued => StatusCode::ACCEPTED,
    }
}

async fn get_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((agent_id, turn_id)): Path<(String, String)>,
) -> Result<Json<TurnView>, ApiError> {
    let _agent_guard = state.agent_locks.acquire(agent_id.clone()).await;
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.read").await?;
    Ok(Json(state.manager.get_turn(identity, &turn_id).await?))
}

#[derive(Default, Deserialize)]
struct EventsQuery {
    after_event_id: Option<u64>,
}

async fn agent_events(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Response, ApiError> {
    let _agent_guard = state.agent_locks.acquire(agent_id.clone()).await;
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.read").await?;
    let after_event_id = last_event_id(&headers)?
        .or(query.after_event_id)
        .unwrap_or(0);
    let cursor = state.manager.events(identity, after_event_id).await?;
    let output = stream::unfold(Some(cursor), |cursor| async move {
        let mut cursor = cursor?;
        let event = cursor.recv().await?;
        let sse = Event::default()
            .id(event.id.to_string())
            .event(event.payload.event_name())
            .json_data(&event)
            .unwrap_or_else(|_| Event::default().event("stream.error").data("{}"));
        Some((Ok::<Event, Infallible>(sse), Some(cursor)))
    });
    Ok(Sse::new(output)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

#[derive(Serialize)]
struct CancelTurnResponse {
    cancel_requested: bool,
}

async fn cancel_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((agent_id, turn_id)): Path<(String, String)>,
) -> Result<Json<CancelTurnResponse>, ApiError> {
    let _agent_guard = state.agent_locks.acquire(agent_id.clone()).await;
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.cancel").await?;
    let cancelled = state.manager.cancel_turn(identity, &turn_id).await?;
    Ok(Json(CancelTurnResponse {
        cancel_requested: cancelled,
    }))
}

#[derive(Serialize)]
struct EvictAgentResponse {
    evicted: bool,
}

async fn evict_agent(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Json<EvictAgentResponse>, ApiError> {
    let _agent_guard = state.agent_locks.acquire(agent_id.clone()).await;
    let (identity, _) = authorize_agent(&state, &headers, &agent_id, "agent.evict").await?;
    let evicted = state.manager.evict(identity).await?;
    Ok(Json(EvictAgentResponse { evicted }))
}

async fn fork_latest(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path(agent_id): Path<String>,
) -> Result<Response, ApiError> {
    fork_agent(&state, &headers, &agent_id, None).await
}

async fn fork_from_turn(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
    Path((agent_id, turn_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    fork_agent(&state, &headers, &agent_id, Some(&turn_id)).await
}

async fn fork_agent(
    state: &ApiState,
    headers: &HeaderMap,
    agent_id: &str,
    turn_id: Option<&str>,
) -> Result<Response, ApiError> {
    let _agent_guard = state.agent_locks.acquire(agent_id.to_owned()).await;
    let (source, client) = authorize_agent(state, headers, agent_id, "agent.fork").await?;
    let client_for_fork = client.clone();
    let source_agent_id = agent_id.to_owned();
    let target = state
        .policy_call(move |policy| policy.fork_agent(&client_for_fork, &source_agent_id))
        .await?;
    match state.manager.fork(source, target.clone(), turn_id).await {
        Ok(response) => {
            let client_for_activation = client.clone();
            let target_id = target.id.clone();
            if let Err(error) = state
                .policy_call(move |policy| {
                    policy.activate_agent(&client_for_activation, &target_id)
                })
                .await
            {
                cleanup_failed_fork(state, &client, &target).await;
                return Err(error);
            }
            Ok((StatusCode::CREATED, Json(response)).into_response())
        }
        Err(error) => {
            cleanup_failed_fork(state, &client, &target).await;
            Err(error.into())
        }
    }
}

async fn cleanup_failed_fork(
    state: &ApiState,
    client: &crate::AuthenticatedClient,
    target: &crate::AgentIdentity,
) {
    if let Err(error) = state.manager.delete(&target.id).await {
        tracing::warn!(%error, "failed to clean up fork session state");
    }
    let client = client.clone();
    let target_id = target.id.clone();
    if let Err(error) = state
        .policy_call(move |policy| policy.abort_provisioning_agent(&client, &target_id))
        .await
    {
        tracing::warn!(error = ?error, "failed to clean up fork registry record");
    }
}

async fn payment_session(
    State(state): State<Arc<ApiState>>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let authentication_headers = headers.clone();
    state
        .policy_call(move |policy| policy.authenticate(&authentication_headers).map(drop))
        .await?;
    payment_response(state.payments.authorize(&headers).await?)
}

async fn authorize_agent(
    state: &ApiState,
    headers: &HeaderMap,
    agent_id: &str,
    permission: &'static str,
) -> Result<(crate::AgentIdentity, crate::AuthenticatedClient), ApiError> {
    let headers = headers.clone();
    let agent_id = agent_id.to_owned();
    state
        .policy_call(move |policy| {
            let client = policy.authenticate(&headers)?;
            let identity = policy.agent(&client, &agent_id)?;
            require(&identity.principal, permission)?;
            Ok((identity, client))
        })
        .await
}

fn payment_response(outcome: PaymentOutcome) -> Result<Response, ApiError> {
    match outcome {
        PaymentOutcome::Challenge { www_authenticate } => {
            let mut response = (
                StatusCode::PAYMENT_REQUIRED,
                Json(ErrorResponse::new(
                    "payment_required",
                    "payment authorization required",
                )),
            )
                .into_response();
            response.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_str(&www_authenticate).map_err(|_| ApiError::Internal)?,
            );
            Ok(response)
        }
        PaymentOutcome::Management { body, receipt } => {
            let mut response = Json(body).into_response();
            insert_receipt(&mut response, &receipt.header_value)?;
            Ok(response)
        }
        PaymentOutcome::Authorized(receipt) => {
            let mut response = StatusCode::NO_CONTENT.into_response();
            insert_receipt(&mut response, &receipt.header_value)?;
            Ok(response)
        }
    }
}

fn insert_receipt(response: &mut Response, value: &str) -> Result<(), ApiError> {
    response.headers_mut().insert(
        HeaderName::from_static("payment-receipt"),
        HeaderValue::from_str(value).map_err(|_| ApiError::Internal)?,
    );
    Ok(())
}

fn idempotency_key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    optional_header(headers, "idempotency-key", "Idempotency-Key")
}

fn last_event_id(headers: &HeaderMap) -> Result<Option<u64>, ApiError> {
    optional_header(headers, "last-event-id", "Last-Event-ID")?
        .map(|value| {
            value
                .parse()
                .map_err(|_| ApiError::BadHeader("Last-Event-ID"))
        })
        .transpose()
}

fn optional_header(
    headers: &HeaderMap,
    header_name: &'static str,
    display_name: &'static str,
) -> Result<Option<String>, ApiError> {
    headers
        .get(header_name)
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| ApiError::BadHeader(display_name))
        })
        .transpose()
}

#[derive(Debug)]
pub(crate) enum ApiError {
    Authorization(AuthorizationError),
    Policy(PolicyError),
    Manager(ManagerError),
    Payment(PaymentError),
    BadHeader(&'static str),
    Internal,
}

impl From<AuthorizationError> for ApiError {
    fn from(error: AuthorizationError) -> Self {
        Self::Authorization(error)
    }
}

impl From<PolicyError> for ApiError {
    fn from(error: PolicyError) -> Self {
        Self::Policy(error)
    }
}

impl From<ManagerError> for ApiError {
    fn from(error: ManagerError) -> Self {
        Self::Manager(error)
    }
}

impl From<PaymentError> for ApiError {
    fn from(error: PaymentError) -> Self {
        Self::Payment(error)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Authorization(AuthorizationError::Unauthenticated)
            | Self::Policy(PolicyError::Unauthenticated) => (
                StatusCode::UNAUTHORIZED,
                "unauthenticated",
                "authentication required",
            ),
            Self::Policy(PolicyError::Forbidden) => (
                StatusCode::FORBIDDEN,
                "permission_denied",
                "permission denied",
            ),
            Self::Policy(PolicyError::NotFound) | Self::Manager(ManagerError::NotFound) => {
                (StatusCode::NOT_FOUND, "not_found", "resource not found")
            }
            Self::Manager(ManagerError::SteerQueueFull) => (
                StatusCode::CONFLICT,
                "steer_queue_full",
                "active turn steering queue is full",
            ),
            Self::Manager(ManagerError::IdempotencyConflict) => (
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "Idempotency-Key was already used for a different request",
            ),
            Self::Manager(ManagerError::AgentBusy) => (
                StatusCode::CONFLICT,
                "agent_busy",
                "agent must be idle for this operation",
            ),
            Self::Manager(ManagerError::ForkBoundaryNotFound) => (
                StatusCode::CONFLICT,
                "fork_boundary_not_found",
                "completed fork boundary was not found",
            ),
            Self::Manager(ManagerError::Invalid(message))
            | Self::Policy(PolicyError::Invalid(message)) => {
                (StatusCode::BAD_REQUEST, "invalid_request", message)
            }
            Self::Payment(PaymentError::InvalidCredential) => (
                StatusCode::PAYMENT_REQUIRED,
                "invalid_payment",
                "invalid payment credential",
            ),
            Self::Payment(PaymentError::Configuration(_) | PaymentError::Verification(_))
            | Self::Policy(
                PolicyError::Poisoned
                | PolicyError::Database(_)
                | PolicyError::Json(_)
                | PolicyError::Io(_),
            )
            | Self::Manager(
                ManagerError::ActorStopped | ManagerError::Durability(_) | ManagerError::Agent(_),
            )
            | Self::Authorization(AuthorizationError::InvalidConfiguration(_))
            | Self::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "internal server error",
            ),
            Self::BadHeader(name) => (StatusCode::BAD_REQUEST, "invalid_header", name),
        };
        (status, Json(ErrorResponse::new(code, message))).into_response()
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

impl ErrorResponse {
    const fn new(code: &'static str, message: &'static str) -> Self {
        Self {
            error: ErrorBody { code, message },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
        routing::get,
    };
    use serde::de::DeserializeOwned;
    use tower::ServiceExt;

    use super::*;
    use crate::{
        AdminAuthorizer, CapabilityEgress, CreateSecret, EgressContext, EgressProvider,
        FreePaymentGate, ManagedAgentFactory, ManagedEgress, MockAgentFactory, PolicyStore,
        SecretDelivery, SecretError, SecretGuestConfig, SecretHttpMethod, SecretManager, SecretRef,
        SecretRequestRule,
    };

    struct ConstantSecret;

    struct CountingPaymentGate {
        authorizations: AtomicUsize,
    }

    #[async_trait]
    impl PaymentGate for CountingPaymentGate {
        async fn authorize(&self, _headers: &HeaderMap) -> Result<PaymentOutcome, PaymentError> {
            self.authorizations.fetch_add(1, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_millis(25)).await;
            Ok(PaymentOutcome::Authorized(crate::PaymentReceipt {
                header_value: "counted".to_owned(),
            }))
        }
    }

    #[async_trait]
    impl SecretManager for ConstantSecret {
        async fn resolve(&self, _reference: &SecretRef) -> Result<String, SecretError> {
            Ok("host-only".to_owned())
        }
    }

    async fn response_json<T: DeserializeOwned>(response: Response) -> T {
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
    }

    fn test_api_key_headers() -> HeaderMap {
        HeaderMap::from_iter([(
            HeaderName::from_static("x-api-key"),
            HeaderValue::from_static("test-key"),
        )])
    }

    fn test_app(delay: Duration) -> Router {
        let factory: Arc<dyn ManagedAgentFactory> = Arc::new(MockAgentFactory::new(delay));
        let directory = tempfile::tempdir().unwrap().keep();
        let policy = Arc::new(PolicyStore::in_memory().unwrap());
        policy
            .bootstrap("test", "Test", "test-key", "test", [])
            .unwrap();
        let state = Arc::new(ApiState::new(
            Arc::new(AgentManager::new(factory, directory).unwrap()),
            policy,
            Arc::new(AdminAuthorizer::new("admin-key").unwrap()),
            Arc::new(FreePaymentGate),
        ));
        router(state)
    }

    async fn create_test_agent(app: &Router, context_key: &str) -> String {
        let response = app
            .clone()
            .oneshot(
                Request::post("/v1/agent/new")
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(format!(r#"{{"context_key":"{context_key}"}}"#)))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        response_json::<CreateAgentResponse>(response)
            .await
            .agent_id
    }

    async fn wait_for_test_turn(app: &Router, agent_id: &str, turn_id: &str) -> TurnView {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let response = app
                    .clone()
                    .oneshot(
                        Request::get(format!("/v1/agent/{agent_id}/turn/{turn_id}"))
                            .header("x-api-key", "test-key")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                let turn = response_json::<TurnView>(response).await;
                if turn.state.is_terminal() {
                    return turn;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn context_key_creates_or_returns_the_same_agent() {
        let app = test_app(Duration::from_millis(5));
        let first = create_test_agent(&app, "context:1").await;
        let response = app
            .oneshot(
                Request::post("/v1/agent/new")
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"context_key":"context:1"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json::<CreateAgentResponse>(response).await;
        assert_eq!(body.agent_id, first);
        assert!(!body.created);
    }

    #[tokio::test]
    async fn concurrent_idempotent_turns_are_authorized_for_payment_once() {
        let factory: Arc<dyn ManagedAgentFactory> =
            Arc::new(MockAgentFactory::new(Duration::from_millis(50)));
        let directory = tempfile::tempdir().unwrap().keep();
        let policy = Arc::new(PolicyStore::in_memory().unwrap());
        policy
            .bootstrap("test", "Test", "test-key", "test", [])
            .unwrap();
        let payments = Arc::new(CountingPaymentGate {
            authorizations: AtomicUsize::new(0),
        });
        let state = Arc::new(ApiState::new(
            Arc::new(AgentManager::new(factory, directory).unwrap()),
            policy,
            Arc::new(AdminAuthorizer::new("admin-key").unwrap()),
            payments.clone(),
        ));
        let app = router(state);
        let agent_id = create_test_agent(&app, "context:paid-idempotency").await;
        let request = || {
            Request::post(format!("/v1/agent/{agent_id}/turn"))
                .header("x-api-key", "test-key")
                .header("idempotency-key", "same-paid-turn")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(r#"{"content":[{"type":"text","text":"once"}]}"#))
                .unwrap()
        };

        let (first, second) = tokio::join!(
            app.clone().oneshot(request()),
            app.clone().oneshot(request())
        );
        let first = first.unwrap();
        let second = second.unwrap();
        let statuses = [first.status(), second.status()];
        assert_eq!(statuses, [StatusCode::ACCEPTED, StatusCode::ACCEPTED]);
        assert_eq!(first.headers()["payment-receipt"], "counted");
        assert_eq!(second.headers()["payment-receipt"], "counted");
        assert_eq!(payments.authorizations.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn invalid_turns_are_rejected_before_payment_authorization() {
        let factory: Arc<dyn ManagedAgentFactory> =
            Arc::new(MockAgentFactory::new(Duration::from_millis(5)));
        let directory = tempfile::tempdir().unwrap().keep();
        let policy = Arc::new(PolicyStore::in_memory().unwrap());
        policy
            .bootstrap("test", "Test", "test-key", "test", [])
            .unwrap();
        let payments = Arc::new(CountingPaymentGate {
            authorizations: AtomicUsize::new(0),
        });
        let state = Arc::new(ApiState::new(
            Arc::new(AgentManager::new(factory, directory).unwrap()),
            policy,
            Arc::new(AdminAuthorizer::new("admin-key").unwrap()),
            payments.clone(),
        ));
        let app = router(state);
        let agent_id = create_test_agent(&app, "context:invalid-unpaid").await;
        let response = app
            .oneshot(
                Request::post(format!("/v1/agent/{agent_id}/turn"))
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"content":[]}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(payments.authorizations.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn idempotency_key_reuse_with_different_content_is_rejected() {
        let app = test_app(Duration::from_millis(50));
        let agent_id = create_test_agent(&app, "context:idempotency-conflict").await;
        let send = |text: &'static str| {
            Request::post(format!("/v1/agent/{agent_id}/turn"))
                .header("x-api-key", "test-key")
                .header("idempotency-key", "same-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(format!(
                    r#"{{"content":[{{"type":"text","text":"{text}"}}]}}"#
                )))
                .unwrap()
        };
        let first = app.clone().oneshot(send("first")).await.unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let conflict = app.oneshot(send("different")).await.unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let body = response_json::<serde_json::Value>(conflict).await;
        assert_eq!(body["error"]["code"], "idempotency_conflict");
    }

    #[tokio::test]
    async fn follow_on_messages_steer_unless_enqueue_is_explicit() {
        let app = test_app(Duration::from_millis(100));
        let agent_id = create_test_agent(&app, "context:steer").await;
        let send = |body: &'static str| {
            Request::post(format!("/v1/agent/{agent_id}/turn"))
                .header("x-api-key", "test-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let started = app
            .clone()
            .oneshot(send(r#"{"content":[{"type":"text","text":"one"}]}"#))
            .await
            .unwrap();
        let started = response_json::<crate::TurnActionResponse>(started).await;
        assert_eq!(started.action, TurnAction::Started);
        tokio::time::sleep(Duration::from_millis(10)).await;

        let steered = app
            .clone()
            .oneshot(send(r#"{"content":[{"type":"text","text":"two"}]}"#))
            .await
            .unwrap();
        let steered = response_json::<crate::TurnActionResponse>(steered).await;
        assert_eq!(steered.action, TurnAction::Steered);
        assert_eq!(steered.turn_id, started.turn_id);

        let queued = app
            .oneshot(send(
                r#"{"delivery":"enqueue","content":[{"type":"text","text":"three"}]}"#,
            ))
            .await
            .unwrap();
        let queued = response_json::<crate::TurnActionResponse>(queued).await;
        assert_eq!(queued.action, TurnAction::Queued);
        assert_ne!(queued.turn_id, started.turn_id);
    }

    #[tokio::test]
    async fn nested_turn_result_route_returns_content_blocks() {
        let app = test_app(Duration::from_millis(5));
        let agent_id = create_test_agent(&app, "context:result").await;
        let created = app
            .clone()
            .oneshot(
                Request::post(format!("/v1/agent/{agent_id}/turn"))
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"content":[{"type":"text","text":"hello"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let turn_id = response_json::<crate::TurnActionResponse>(created)
            .await
            .turn_id;
        wait_for_test_turn(&app, &agent_id, &turn_id).await;
        let result = app
            .oneshot(
                Request::get(format!("/v1/agent/{agent_id}/turn/{turn_id}"))
                    .header("x-api-key", "test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let result = response_json::<TurnView>(result).await;
        assert_eq!(result.state, crate::TurnStatus::Completed);
        assert!(matches!(
            result.output.first(),
            Some(crate::ContentBlock::Text { .. })
        ));
    }

    #[tokio::test]
    async fn completed_agent_can_fork_through_the_nested_route() {
        let app = test_app(Duration::from_millis(5));
        let agent_id = create_test_agent(&app, "context:fork").await;
        let created = app
            .clone()
            .oneshot(
                Request::post(format!("/v1/agent/{agent_id}/turn"))
                    .header("x-api-key", "test-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"content":[{"type":"text","text":"establish a boundary"}]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let turn_id = response_json::<crate::TurnActionResponse>(created)
            .await
            .turn_id;
        wait_for_test_turn(&app, &agent_id, &turn_id).await;

        let forked = app
            .oneshot(
                Request::post(format!("/v1/agent/{agent_id}/turn/{turn_id}/fork"))
                    .header("x-api-key", "test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forked.status(), StatusCode::CREATED);
        let forked = response_json::<crate::ForkResponse>(forked).await;
        assert_ne!(forked.agent_id, agent_id);
        assert_eq!(
            forked.forked_from.turn_id.as_deref(),
            Some(turn_id.as_str())
        );
    }

    #[tokio::test]
    async fn admin_routes_reject_agent_credentials() {
        let app = test_app(Duration::from_millis(5));
        let response = app
            .oneshot(
                Request::get("/admin/v1/principals")
                    .header("x-api-key", "test-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_can_create_and_grant_a_typed_secret() {
        let app = test_app(Duration::from_millis(5));
        let create = app
            .clone()
            .oneshot(
                Request::post("/admin/v1/secrets")
                    .header("authorization", "Bearer admin-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{
                            "id":"openai",
                            "name":"OpenAI",
                            "source":{"provider":"environment","key":"OPENAI"},
                            "upstream":"https://api.openai.com",
                            "rules":[{"methods":["POST"],"path_prefixes":["/v1/"]}],
                            "delivery":{"type":"inject_header","header":"authorization","prefix":"Bearer "},
                            "guest":{"base_url_env":"OPENAI_BASE_URL"}
                        }"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::CREATED);
        let created = response_json::<crate::SecretView>(create).await;
        assert_eq!(created.id, "openai");

        let grant = app
            .clone()
            .oneshot(
                Request::put("/admin/v1/principals/test/secrets/openai")
                    .header("authorization", "Bearer admin-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(grant.status(), StatusCode::NO_CONTENT);
        let effective = app
            .oneshot(
                Request::get("/admin/v1/principals/test/effective-secrets")
                    .header("authorization", "Bearer admin-key")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let effective = response_json::<Vec<crate::SecretView>>(effective).await;
        assert_eq!(effective.len(), 1);
        assert_eq!(effective[0].source.key(), "OPENAI");
    }

    #[tokio::test]
    async fn managed_secret_proxy_rechecks_live_grants() {
        let observed = Arc::new(Mutex::new(Vec::<String>::new()));
        let upstream_observed = Arc::clone(&observed);
        let upstream = Router::new().route(
            "/v1/models",
            get(move |headers: HeaderMap| {
                let observed = Arc::clone(&upstream_observed);
                async move {
                    observed.lock().unwrap().push(
                        headers
                            .get(header::AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or_default()
                            .to_owned(),
                    );
                    "ok"
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_address = listener.local_addr().unwrap();
        let upstream_server =
            tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let policy = Arc::new(PolicyStore::in_memory().unwrap());
        policy
            .bootstrap("test", "Test", "test-key", "test", [])
            .unwrap();
        policy
            .create_secret(CreateSecret {
                id: Some("openai".to_owned()),
                name: "OpenAI".to_owned(),
                source: SecretRef::new("test", "openai"),
                upstream: format!("http://{upstream_address}"),
                rules: vec![
                    SecretRequestRule::new()
                        .method(SecretHttpMethod::Get)
                        .path_prefix("/v1/"),
                ],
                delivery: SecretDelivery::inject_header("authorization", "Bearer "),
                guest: SecretGuestConfig::new("OPENAI_BASE_URL"),
            })
            .unwrap();
        policy.set_principal_secret("test", "openai", true).unwrap();
        let client = policy.authenticate(&test_api_key_headers()).unwrap();
        let (identity, _) = policy
            .create_or_resolve_agent(&client, Some("secret-route"))
            .unwrap();
        let egress = ManagedEgress::new(
            Arc::clone(&policy),
            Arc::new(ConstantSecret),
            CapabilityEgress::new(),
        );
        let lease = egress
            .acquire(&EgressContext::new(identity.id, "test"), &BTreeSet::new())
            .await
            .unwrap();
        let route = lease.guest_environment().get("OPENAI_BASE_URL").unwrap();
        let proxy = lease.guest_environment().get("HTTP_PROXY").unwrap();
        let client = reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(proxy).unwrap())
            .build()
            .unwrap();
        let response = client
            .get(format!("{route}/v1/models"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(response.text().await.unwrap(), "ok");
        assert_eq!(observed.lock().unwrap().as_slice(), ["Bearer host-only"]);

        policy
            .set_principal_secret("test", "openai", false)
            .unwrap();
        let denied = client.get(format!("{route}/v1/models")).send().await;
        assert!(denied.is_err() || !denied.unwrap().status().is_success());
        drop(lease);
        upstream_server.abort();
    }
}
