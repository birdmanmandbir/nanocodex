//! Evaluator-owned OpenAI judge service for isolated benchmark verifiers.

use std::{collections::BTreeMap, net::Ipv4Addr, str::FromStr as _, sync::Arc};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse as _, Response},
    routing::post,
};
use nanocodex_agent::NanocodexBuilder;
use nanocodex_oai_api::Model;
use nanocodex_tools::Tools;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use uuid::Uuid;

const GUEST_HOST: Ipv4Addr = Ipv4Addr::new(192, 168, 127, 254);

/// A run-scoped judge endpoint backed by the evaluator's selected OpenAI auth.
pub struct JudgeRuntime {
    port: u16,
    token: Arc<str>,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<()>,
}

#[derive(Clone)]
struct JudgeState {
    builder: NanocodexBuilder,
    token: Arc<str>,
}

#[derive(Debug, Deserialize)]
struct JudgeRequest {
    model: String,
    input: Value,
}

#[derive(Debug, Serialize)]
struct JudgeError {
    error: JudgeErrorBody,
}

#[derive(Debug, Serialize)]
struct JudgeErrorBody {
    message: String,
}

/// Judge service startup failed.
#[derive(Debug, thiserror::Error)]
pub enum JudgeRuntimeError {
    /// The loopback listener could not be created or inspected.
    #[error("judge runtime listener failed: {0}")]
    Listener(#[from] std::io::Error),
    /// The deliberately empty verifier tool registry could not be built.
    #[error("judge runtime tool policy failed: {0}")]
    Tools(#[from] nanocodex_tools::ToolsBuildError),
}

impl JudgeRuntime {
    /// Starts a no-tools judge service on host loopback.
    pub async fn start(builder: NanocodexBuilder) -> Result<Self, JudgeRuntimeError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let token: Arc<str> = Uuid::now_v7().simple().to_string().into();
        let state = JudgeState {
            builder: builder.tools(Tools::builder().without_defaults().build()?),
            token: Arc::clone(&token),
        };
        let application = Router::new()
            .route("/v1/responses", post(Self::respond))
            .with_state(state);
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(async move {
            let result = axum::serve(listener, application)
                .with_graceful_shutdown(async move {
                    let _ = receiver.await;
                })
                .await;
            if let Err(error) = result {
                tracing::error!(%error, "judge runtime stopped unexpectedly");
            }
        });
        Ok(Self {
            port,
            token,
            shutdown: Some(shutdown),
            task,
        })
    }

    /// Returns verifier-only values that route through the guest host gateway.
    #[must_use]
    pub fn verifier_environment(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                "NANOCODEX_JUDGE_BASE_URL".to_owned(),
                format!("http://{GUEST_HOST}:{}/v1", self.port),
            ),
            ("NANOCODEX_JUDGE_TOKEN".to_owned(), self.token.to_string()),
        ])
    }

    async fn respond(
        State(state): State<JudgeState>,
        headers: HeaderMap,
        Json(request): Json<JudgeRequest>,
    ) -> Response {
        let authorized = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            == Some(state.token.as_ref());
        if !authorized {
            return Self::error(StatusCode::UNAUTHORIZED, "invalid judge token");
        }
        let model = match Model::from_str(&request.model) {
            Ok(model) => model,
            Err(error) => return Self::error(StatusCode::BAD_REQUEST, error),
        };
        let (instructions, prompt) = match Self::prompt(request.input) {
            Ok(prompt) => prompt,
            Err(error) => return Self::error(StatusCode::BAD_REQUEST, error),
        };
        let mut builder = state.builder.model(model);
        if let Some(instructions) = instructions {
            builder = builder.instructions(instructions);
        }
        let (agent, events) = match builder.build() {
            Ok(agent) => agent,
            Err(error) => {
                return Self::error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("judge agent build failed: {error}"),
                );
            }
        };
        drop(events);
        let result = match agent.prompt(prompt).await {
            Ok(turn) => turn.result().await,
            Err(error) => Err(error),
        };
        let message = match result {
            Ok(result) => result.into_final_message(),
            Err(error) => {
                return Self::error(
                    StatusCode::BAD_GATEWAY,
                    format!("OpenAI judge failed: {error}"),
                );
            }
        };
        let _ = agent.shutdown().await;
        Json(json!({
            "id": format!("judge_{}", Uuid::now_v7().simple()),
            "object": "response",
            "status": "completed",
            "model": model,
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": message}]
            }]
        }))
        .into_response()
    }

    fn prompt(input: Value) -> Result<(Option<String>, String), String> {
        match input {
            Value::String(prompt) if !prompt.trim().is_empty() => Ok((None, prompt)),
            Value::Array(messages) => {
                let mut instructions = Vec::new();
                let mut prompt = Vec::new();
                for message in messages {
                    let role = message
                        .get("role")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "judge input message has no role".to_owned())?;
                    let content = message
                        .get("content")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "judge input message content must be text".to_owned())?;
                    if role == "system" || role == "developer" {
                        instructions.push(content.to_owned());
                    } else {
                        prompt.push(format!("{role}:\n{content}"));
                    }
                }
                if prompt.is_empty() {
                    return Err("judge input contains no user prompt".to_owned());
                }
                Ok((
                    (!instructions.is_empty()).then(|| instructions.join("\n\n")),
                    prompt.join("\n\n"),
                ))
            }
            _ => Err("judge input must be text or an array of text messages".to_owned()),
        }
    }

    fn error(status: StatusCode, message: impl Into<String>) -> Response {
        (
            status,
            Json(JudgeError {
                error: JudgeErrorBody {
                    message: message.into(),
                },
            }),
        )
            .into_response()
    }
}

impl Drop for JudgeRuntime {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        self.task.abort();
    }
}
