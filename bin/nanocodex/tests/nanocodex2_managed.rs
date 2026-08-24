use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, Response, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};

const AGENT_ID: &str = "agent-1";
const TURN_ID: &str = "turn-live";

#[derive(Clone)]
struct TestState {
    authorization: String,
    authorized_requests: Arc<AtomicUsize>,
    origin: String,
}

#[tokio::test]
async fn run_uses_the_managed_lifecycle_end_to_end() {
    let api_key = format!("ncx_live_{}_{}", "a".repeat(12), "b".repeat(43));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let state = TestState {
        authorization: format!("Bearer {api_key}"),
        authorized_requests: Arc::new(AtomicUsize::new(0)),
        origin: format!("http://{address}"),
    };
    let app = Router::new()
        .route("/v1/agents", post(create_agent))
        .route("/v1/agents/{agent}/turns", post(submit_turn))
        .route("/v1/agents/{agent}/events", get(events))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_nanocodex2"))
        .args([
            "run",
            "answer from managed",
            "--turn-id",
            TURN_ID,
            "--idempotency-key",
            "stable-request",
        ])
        .env("NANOCODEX_MANAGED_URL", &state.origin)
        .env("NANOCODEX_API_KEY", &api_key)
        .env_remove("OPENAI_API_KEY")
        .output()
        .await
        .unwrap();

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    let agent_event: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(agent_event["type"], "assistant.message");
    let terminal: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(terminal["type"], "turn_completed");
    assert_eq!(stderr, "managed answer\n");
    assert!(!stdout.contains(&api_key));
    assert!(!stderr.contains(&api_key));
    assert_eq!(state.authorized_requests.load(Ordering::SeqCst), 3);
    server.abort();
}

async fn create_agent(State(state): State<TestState>, headers: HeaderMap) -> impl IntoResponse {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    json_response(
        StatusCode::CREATED,
        serde_json::json!({
            "agent_id": AGENT_ID,
            "session_id": AGENT_ID,
            "events_url": format!("{}/v1/agents/{AGENT_ID}/events", state.origin),
            "websocket_url": format!("ws://unused/v1/agents/{AGENT_ID}/ws"),
        }),
    )
}

async fn submit_turn(State(state): State<TestState>, headers: HeaderMap) -> impl IntoResponse {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    json_response(
        StatusCode::ACCEPTED,
        serde_json::json!({
            "turn_id": TURN_ID,
            "state": "accepted",
            "input": "answer from managed",
            "accepted_cursor": "1",
            "terminal_cursor": null,
            "created_at": 1,
            "accepted_at": 1,
            "updated_at": 1,
            "attempt_count": 0,
            "retry_at": null,
            "error": null,
            "terminal": null,
        }),
    )
}

async fn events(State(state): State<TestState>, headers: HeaderMap) -> impl IntoResponse {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    let body = concat!(
        "id: 2\n",
        "event: event\n",
        "data: {\"cursor\":\"2\",\"created_at\":2,\"turn_id\":\"turn-live\",",
        "\"type\":\"event\",\"event\":{\"protocol_version\":1,",
        "\"request_id\":\"request-live\",\"seq\":1,",
        "\"type\":\"assistant.message\",\"payload\":{\"message\":\"managed answer\"}}}\n\n",
        "id: 3\n",
        "event: turn_completed\n",
        "data: {\"cursor\":\"3\",\"created_at\":3,\"turn_id\":\"turn-live\",",
        "\"type\":\"turn_completed\",\"id\":\"turn-live\",",
        "\"final_message\":\"managed answer\",\"usage\":null}\n\n"
    );
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from(body))
        .unwrap()
}

fn authorized(state: &TestState, headers: &HeaderMap) -> bool {
    let matches = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        == Some(state.authorization.as_str());
    if matches {
        state.authorized_requests.fetch_add(1, Ordering::SeqCst);
    }
    matches
}

fn unauthorized() -> Response<Body> {
    json_response(
        StatusCode::UNAUTHORIZED,
        serde_json::json!({ "error": "unauthorized" }),
    )
}

fn json_response(status: StatusCode, body: serde_json::Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}
