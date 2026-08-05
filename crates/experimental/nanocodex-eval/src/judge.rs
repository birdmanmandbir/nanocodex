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

#[derive(Debug, Deserialize)]
struct ChatCompletionRequest {
    model: String,
    messages: Value,
}

struct JudgeAnswer {
    model: Model,
    message: String,
}

struct JudgeFailure {
    status: StatusCode,
    message: String,
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
            .route("/v1/chat/completions", post(Self::chat_completion))
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
        let base_url = format!("http://{GUEST_HOST}:{}/v1", self.port);
        BTreeMap::from([
            ("NANOCODEX_JUDGE_BASE_URL".to_owned(), base_url.clone()),
            ("NANOCODEX_JUDGE_TOKEN".to_owned(), self.token.to_string()),
            ("OPENAI_BASE_URL".to_owned(), base_url),
            ("OPENAI_API_KEY".to_owned(), self.token.to_string()),
            (
                "NANOCODEX_EVAL_GRADER_MODEL".to_owned(),
                Model::Sol.to_string(),
            ),
        ])
    }

    async fn respond(
        State(state): State<JudgeState>,
        headers: HeaderMap,
        Json(request): Json<JudgeRequest>,
    ) -> Response {
        let answer = match state.answer(&headers, request.model, request.input).await {
            Ok(answer) => answer,
            Err(error) => return Self::error(error.status, error.message),
        };
        Json(json!({
            "id": format!("judge_{}", Uuid::now_v7().simple()),
            "object": "response",
            "status": "completed",
            "model": answer.model,
            "output": [{
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": answer.message}]
            }],
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "total_tokens": 0
            }
        }))
        .into_response()
    }

    async fn chat_completion(
        State(state): State<JudgeState>,
        headers: HeaderMap,
        Json(request): Json<ChatCompletionRequest>,
    ) -> Response {
        let answer = match state
            .answer(&headers, request.model, request.messages)
            .await
        {
            Ok(answer) => answer,
            Err(error) => return Self::error(error.status, error.message),
        };
        Json(json!({
            "id": format!("judge_{}", Uuid::now_v7().simple()),
            "object": "chat.completion",
            "created": 0,
            "model": answer.model,
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": answer.message,
                    "refusal": null
                }
            }],
            "usage": {
                "completion_tokens": 0,
                "prompt_tokens": 0,
                "total_tokens": 0
            }
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
                        .ok_or_else(|| "judge input message has no content".to_owned())?;
                    let content = Self::text_content(content)?;
                    if role == "system" || role == "developer" {
                        instructions.push(content);
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

    fn text_content(content: &Value) -> Result<String, String> {
        if let Some(content) = content.as_str() {
            return Ok(content.to_owned());
        }
        let parts = content
            .as_array()
            .ok_or_else(|| "judge input message content must be text".to_owned())?;
        let text = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            return Err("judge input message contains no text".to_owned());
        }
        Ok(text)
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

impl JudgeState {
    async fn answer(
        &self,
        headers: &HeaderMap,
        requested_model: String,
        input: Value,
    ) -> Result<JudgeAnswer, JudgeFailure> {
        if !self.authorized(headers) {
            return Err(JudgeFailure {
                status: StatusCode::UNAUTHORIZED,
                message: "invalid judge token".to_owned(),
            });
        }
        let model = Model::from_str(&requested_model).map_err(|message| JudgeFailure {
            status: StatusCode::BAD_REQUEST,
            message,
        })?;
        let (instructions, prompt) =
            JudgeRuntime::prompt(input).map_err(|message| JudgeFailure {
                status: StatusCode::BAD_REQUEST,
                message,
            })?;
        let mut builder = self.builder.clone().model(model);
        if let Some(instructions) = instructions {
            builder = builder.instructions(instructions);
        }
        let (agent, events) = match builder.build() {
            Ok(agent) => agent,
            Err(error) => {
                return Err(JudgeFailure {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: format!("judge agent build failed: {error}"),
                });
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
                return Err(JudgeFailure {
                    status: StatusCode::BAD_GATEWAY,
                    message: format!("OpenAI judge failed: {error}"),
                });
            }
        };
        let _ = agent.shutdown().await;
        Ok(JudgeAnswer { model, message })
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            == Some(self.token.as_ref())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_accepts_openai_chat_and_responses_text_shapes() {
        let (instructions, prompt) = JudgeRuntime::prompt(json!([
            {"role": "system", "content": "Grade precisely."},
            {
                "role": "user",
                "content": [{"type": "input_text", "text": "candidate answer"}]
            }
        ]))
        .unwrap();

        assert_eq!(instructions.as_deref(), Some("Grade precisely."));
        assert_eq!(prompt, "user:\ncandidate answer");
    }

    #[tokio::test]
    async fn verifier_environment_routes_both_openai_protocols_through_proxy() {
        let (shutdown, _receiver) = oneshot::channel();
        let runtime = JudgeRuntime {
            port: 43123,
            token: Arc::from("judge-token"),
            shutdown: Some(shutdown),
            task: tokio::spawn(std::future::pending()),
        };

        let environment = runtime.verifier_environment();

        assert_eq!(
            environment.get("OPENAI_BASE_URL").map(String::as_str),
            Some("http://192.168.127.254:43123/v1")
        );
        assert_eq!(
            environment.get("OPENAI_API_KEY").map(String::as_str),
            Some("judge-token")
        );
        assert_eq!(
            environment
                .get("NANOCODEX_EVAL_GRADER_MODEL")
                .map(String::as_str),
            Some("gpt-5.6-sol")
        );
    }
}
