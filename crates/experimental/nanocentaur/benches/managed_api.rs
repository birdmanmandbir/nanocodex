use std::{
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{HeaderMap, Request, StatusCode, header},
};
use criterion::{Criterion, criterion_group, criterion_main};
use nanocentaur::{
    AdminAuthorizer, AgentEventPayload, AgentIdentity, AgentManager, ApiState, EventCursor,
    FreePaymentGate, ManagedAgentFactory, MockAgentFactory, PolicyStore, TurnActionResponse,
};
use serde::de::DeserializeOwned;
use tempfile::TempDir;
use tokio::sync::Mutex;
use tower::ServiceExt;

const API_KEY: &str = "managed-benchmark-key";

struct ManagedHarness {
    app: Router,
    manager: Arc<AgentManager>,
    events: Mutex<EventCursor>,
    agent_id: String,
    _directory: TempDir,
}

impl ManagedHarness {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("benchmark state directory");
        let policy = Arc::new(
            PolicyStore::open(directory.path().join("policy.sqlite"))
                .expect("benchmark policy store"),
        );
        policy
            .bootstrap(
                "benchmark-client",
                "Managed API benchmark",
                API_KEY,
                "benchmark-principal",
                [],
            )
            .expect("benchmark policy bootstrap");
        let factory: Arc<dyn ManagedAgentFactory> = Arc::new(MockAgentFactory::new(Duration::ZERO));
        let manager = Arc::new(
            AgentManager::new(Arc::clone(&factory), directory.path())
                .expect("benchmark session store"),
        );
        let app = ApiState::new(
            Arc::clone(&manager),
            Arc::clone(&policy),
            Arc::new(AdminAuthorizer::new("managed-benchmark-admin").expect("admin token")),
            Arc::new(FreePaymentGate),
        )
        .router();

        let response = app
            .clone()
            .oneshot(json_request(
                "/v1/agent/new",
                None,
                r#"{"context_key":"managed-benchmark"}"#,
            ))
            .await
            .expect("create agent response");
        assert_eq!(response.status(), StatusCode::CREATED);
        let created: nanocentaur::CreateAgentResponse = response_json(response).await;
        let identity = identity(&policy, &created.agent_id);
        let events = manager
            .events(identity, 0)
            .await
            .expect("benchmark event cursor");

        Self {
            app,
            manager,
            events: Mutex::new(events),
            agent_id: created.agent_id,
            _directory: directory,
        }
    }

    async fn submit_turn(&self, idempotency_key: &str) -> (StatusCode, TurnActionResponse) {
        let response = self
            .app
            .clone()
            .oneshot(json_request(
                &format!("/v1/agent/{}/turn", self.agent_id),
                Some(idempotency_key),
                r#"{"delivery":"enqueue","content":[{"type":"text","text":"Return exactly `done`."}]}"#,
            ))
            .await
            .expect("turn response");
        let status = response.status();
        let action = response_json(response).await;
        (status, action)
    }

    async fn submit_and_wait(&self, idempotency_key: &str) -> TurnActionResponse {
        let (status, action) = self.submit_turn(idempotency_key).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        let mut events = self.events.lock().await;
        loop {
            let event = events
                .recv()
                .await
                .expect("managed event stream remains open");
            if event.turn_id.as_deref() == Some(action.turn_id.as_str())
                && matches!(event.payload, AgentEventPayload::TurnCompleted { .. })
            {
                return action;
            }
        }
    }

    async fn replay_turn(&self, idempotency_key: &str) -> TurnActionResponse {
        let (status, action) = self.submit_turn(idempotency_key).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        action
    }

    async fn read_agent(&self) -> nanocentaur::AgentView {
        let response = self
            .app
            .clone()
            .oneshot(
                Request::get(format!("/v1/agent/{}", self.agent_id))
                    .header("x-api-key", API_KEY)
                    .body(Body::empty())
                    .expect("agent request"),
            )
            .await
            .expect("agent response");
        assert_eq!(response.status(), StatusCode::OK);
        response_json(response).await
    }
}

fn identity(policy: &PolicyStore, agent_id: &str) -> AgentIdentity {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", API_KEY.parse().expect("benchmark API key"));
    let client = policy
        .authenticate(&headers)
        .expect("benchmark client authentication");
    policy
        .agent(&client, agent_id)
        .expect("benchmark agent identity")
}

fn json_request(uri: &str, idempotency_key: Option<&str>, body: &'static str) -> Request<Body> {
    let mut request = Request::post(uri)
        .header("x-api-key", API_KEY)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(idempotency_key) = idempotency_key {
        request = request.header("idempotency-key", idempotency_key);
    }
    request.body(Body::from(body)).expect("JSON request")
}

async fn response_json<T: DeserializeOwned>(response: axum::response::Response) -> T {
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded response body");
    serde_json::from_slice(&body).expect("typed benchmark response")
}

fn benchmark_managed_api(criterion: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("benchmark runtime");
    let harness = Arc::new(runtime.block_on(ManagedHarness::new()));
    runtime.block_on(harness.submit_and_wait("managed-benchmark-replay"));
    let turn_number = AtomicU64::new(1);

    let mut group = criterion.benchmark_group("managed_api");
    group.sample_size(20);
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(2));

    group.bench_function("authorized_agent_view", |bencher| {
        bencher.to_async(&runtime).iter(|| {
            let harness = Arc::clone(&harness);
            async move {
                black_box(harness.read_agent().await);
            }
        });
    });

    group.bench_function("idempotent_turn_replay", |bencher| {
        bencher.to_async(&runtime).iter(|| {
            let harness = Arc::clone(&harness);
            async move {
                black_box(harness.replay_turn("managed-benchmark-replay").await);
            }
        });
    });

    group.bench_function("accepted_turn_to_durable_terminal_event", |bencher| {
        bencher.to_async(&runtime).iter(|| {
            let harness = Arc::clone(&harness);
            let turn_number = turn_number.fetch_add(1, Ordering::Relaxed);
            async move {
                black_box(
                    harness
                        .submit_and_wait(&format!("managed-benchmark-{turn_number}"))
                        .await,
                );
            }
        });
    });
    group.finish();

    runtime
        .block_on(harness.manager.shutdown())
        .expect("managed benchmark shutdown");
}

criterion_group!(benches, benchmark_managed_api);
criterion_main!(benches);
