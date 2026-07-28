use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{
        Error as WebSocketError, Message, Utf8Bytes,
        client::IntoClientRequest,
        error::ProtocolError,
        http::{HeaderValue, header},
    },
};

use crate::{EncodedRequest, OpenAiAuthSnapshot, ResponsesError, connector::connect_async};
use crate::{monotonic_now_ns, transport::wire::turn_state_from_event};

pub(crate) use crate::transport::wire::{decode_event, parse_raw_json};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(30);
const SOCKET_MESSAGE_CAPACITY: usize = 32;
const RESPONSES_WEBSOCKETS_BETA: &str = "responses_websockets=2026-02-06";
const RESPONSES_LITE_HEADER: &str = "x-openai-internal-codex-responses-lite";
const TURN_STATE_HEADER: &str = "x-codex-turn-state";

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(crate) struct ConnectionMetadata {
    pub status: u16,
    pub request_id: Option<String>,
    pub server_model: Option<String>,
    pub reasoning_included: bool,
    pub turn_state: Option<String>,
}

/// Persistent `OpenAI` Responses WebSocket connection.
pub(crate) struct ResponsesSocket {
    pump: SocketPump,
    turn_state: Option<String>,
    stream_idle_timeout: Duration,
    last_activity_at: Instant,
}

pub(crate) struct ReceivedText {
    pub text: Utf8Bytes,
    pub received_ns: u64,
}

struct SocketPump {
    commands: mpsc::Sender<SocketCommand>,
    messages: mpsc::Receiver<PumpMessage>,
    control_activity: watch::Receiver<Option<Instant>>,
    control_closed: bool,
    task: tokio::task::JoinHandle<()>,
}

struct PumpMessage {
    message: std::result::Result<Message, WebSocketError>,
    received_ns: u64,
    received_at: Instant,
}

enum PumpEvent {
    Message(PumpMessage),
    Control(Instant),
}

enum SocketCommand {
    Send {
        message: Message,
        result: oneshot::Sender<std::result::Result<(), WebSocketError>>,
    },
}

impl ResponsesSocket {
    /// Opens a Responses WebSocket with stable session and cache headers.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid configuration, timeout, or handshake failure.
    pub(crate) async fn connect(
        endpoint: &str,
        auth: &OpenAiAuthSnapshot,
        session_id: &str,
        turn_state: Option<&str>,
        stream_idle_timeout: Duration,
    ) -> Result<(Self, ConnectionMetadata), ResponsesError> {
        let mut request =
            endpoint
                .into_client_request()
                .map_err(|error| ResponsesError::InvalidUrl {
                    detail: error.to_string(),
                })?;
        let authorization =
            HeaderValue::from_str(&format!("Bearer {}", auth.bearer())).map_err(|error| {
                ResponsesError::InvalidAuthorization {
                    detail: error.to_string(),
                }
            })?;
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, authorization);
        if let Some(account_id) = auth.account_id() {
            request.headers_mut().insert(
                "ChatGPT-Account-ID",
                HeaderValue::from_str(account_id).map_err(|error| {
                    ResponsesError::InvalidAuthorization {
                        detail: error.to_string(),
                    }
                })?,
            );
        }
        if auth.is_fedramp() {
            request
                .headers_mut()
                .insert("X-OpenAI-Fedramp", HeaderValue::from_static("true"));
        }
        request.headers_mut().insert(
            "OpenAI-Beta",
            HeaderValue::from_static(RESPONSES_WEBSOCKETS_BETA),
        );
        request
            .headers_mut()
            .insert(RESPONSES_LITE_HEADER, HeaderValue::from_static("true"));
        for name in ["session-id", "thread-id", "x-client-request-id"] {
            request.headers_mut().insert(
                name,
                HeaderValue::from_str(session_id).map_err(|error| {
                    ResponsesError::InvalidSessionId {
                        detail: error.to_string(),
                    }
                })?,
            );
        }
        if let Some(turn_state) = turn_state.and_then(|state| HeaderValue::from_str(state).ok()) {
            request.headers_mut().insert(TURN_STATE_HEADER, turn_state);
        }
        request.headers_mut().insert(
            "x-responsesapi-include-timing-metrics",
            HeaderValue::from_static("true"),
        );
        request.headers_mut().insert(
            header::USER_AGENT,
            HeaderValue::from_static(concat!("nanocodex/", env!("CARGO_PKG_VERSION"))),
        );
        let (socket, response) = timeout(CONNECT_TIMEOUT, connect_async(request))
            .await
            .map_err(|_| ResponsesError::HandshakeTimeout {
                seconds: CONNECT_TIMEOUT.as_secs(),
            })?
            .map_err(map_handshake_error)?;
        let turn_state = header_string(response.headers(), TURN_STATE_HEADER);
        let metadata = ConnectionMetadata {
            status: response.status().as_u16(),
            request_id: header_string(response.headers(), "x-request-id"),
            server_model: header_string(response.headers(), "openai-model"),
            reasoning_included: response.headers().contains_key("x-reasoning-included"),
            turn_state: turn_state.clone(),
        };
        Ok((
            Self {
                pump: SocketPump::new(socket),
                turn_state,
                stream_idle_timeout,
                last_activity_at: Instant::now(),
            },
            metadata,
        ))
    }

    /// Marks all control activity observed before this attempt as its baseline.
    pub(crate) fn begin_attempt(&mut self) {
        self.pump.begin_attempt();
        self.last_activity_at = Instant::now();
    }

    /// Sends an encoded request within the configured send timeout.
    ///
    /// # Errors
    ///
    /// Returns an error when the socket closes, sending fails, or times out.
    pub(crate) async fn send(&self, request: EncodedRequest) -> Result<(), ResponsesError> {
        let message = Message::Text(request.into_string().into());
        timeout(SEND_TIMEOUT, self.pump.send(message))
            .await
            .map_err(|_| ResponsesError::SendTimeout {
                seconds: SEND_TIMEOUT.as_secs(),
            })?
            .map_err(map_send_error)?;
        Ok(())
    }

    /// Receives the next text event within the configured idle timeout.
    ///
    /// # Errors
    ///
    /// Returns an error for timeout, socket failure, closure, or an unexpected frame.
    pub(crate) async fn next_text_or_idle_timeout(
        &mut self,
    ) -> Result<ReceivedText, ResponsesError> {
        let idle = sleep_until(self.last_activity_at + self.stream_idle_timeout);
        tokio::pin!(idle);
        loop {
            tokio::select! {
                biased;
                received = self.pump.next() => {
                    match received.ok_or(ResponsesError::UnexpectedEnd)? {
                        PumpEvent::Control(received_at) => {
                            if self.observe_activity(received_at) {
                                idle.as_mut().reset(self.last_activity_at + self.stream_idle_timeout);
                            }
                        }
                        PumpEvent::Message(received) => {
                            let observed_activity = self.observe_activity(received.received_at);
                            if let Some(text) = self.decode_message(received)? {
                                return Ok(text);
                            }
                            if observed_activity {
                                idle.as_mut().reset(self.last_activity_at + self.stream_idle_timeout);
                            }
                        }
                    }
                }
                () = &mut idle => {
                    return Err(ResponsesError::IdleTimeout {
                        seconds: self.stream_idle_timeout.as_secs(),
                    });
                }
            }
        }
    }

    #[must_use]
    pub(crate) fn turn_state(&self) -> Option<&str> {
        self.turn_state.as_deref()
    }

    pub(crate) fn reset_turn_state(&mut self) {
        self.turn_state = None;
    }

    fn capture_turn_state(&mut self, text: &str) {
        if self.turn_state.is_some() {
            return;
        }
        self.turn_state = turn_state_from_event(text);
    }

    fn observe_activity(&mut self, received_at: Instant) -> bool {
        if received_at <= self.last_activity_at {
            return false;
        }
        self.last_activity_at = received_at;
        true
    }

    fn decode_message(
        &mut self,
        received: PumpMessage,
    ) -> Result<Option<ReceivedText>, ResponsesError> {
        let message = received.message.map_err(map_receive_error)?;
        match message {
            Message::Text(text) => {
                self.capture_turn_state(text.as_str());
                Ok(Some(ReceivedText {
                    text,
                    received_ns: received.received_ns,
                }))
            }
            Message::Binary(_) => Err(ResponsesError::UnexpectedBinary),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => Ok(None),
            Message::Close(frame) => {
                let detail = frame.map_or_else(
                    || "without a reason".to_owned(),
                    |frame| format!("with code {}: {}", frame.code, frame.reason),
                );
                Err(ResponsesError::Closed { detail })
            }
        }
    }
}

impl SocketPump {
    fn new(mut socket: Socket) -> Self {
        let (commands, mut command_receiver) = mpsc::channel(32);
        let (message_sender, messages) = mpsc::channel(SOCKET_MESSAGE_CAPACITY);
        let (control_activity_sender, control_activity) = watch::channel(None);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    command = command_receiver.recv() => {
                        let Some(command) = command else {
                            break;
                        };
                        match command {
                            SocketCommand::Send { message, result } => {
                                let send_result = socket.send(message).await;
                                let should_stop = send_result.is_err();
                                drop(result.send(send_result));
                                if should_stop {
                                    break;
                                }
                            }
                        }
                    }
                    message = socket.next() => {
                        let Some(message) = message else {
                            break;
                        };
                        match message {
                            Ok(Message::Ping(payload)) => {
                                let received_at = Instant::now();
                                control_activity_sender.send_replace(Some(received_at));
                                if let Err(error) = socket.send(Message::Pong(payload)).await {
                                    drop(message_sender.send(PumpMessage {
                                        message: Err(error),
                                        received_ns: monotonic_now_ns(),
                                        received_at: Instant::now(),
                                    }).await);
                                    break;
                                }
                            }
                            Ok(Message::Pong(_)) => {
                                control_activity_sender.send_replace(Some(Instant::now()));
                            }
                            Ok(message) => {
                                let received_at = Instant::now();
                                let should_stop = matches!(message, Message::Close(_));
                                if message_sender.send(PumpMessage {
                                    message: Ok(message),
                                    received_ns: monotonic_now_ns(),
                                    received_at,
                                }).await.is_err() || should_stop {
                                    break;
                                }
                            }
                            Err(error) => {
                                drop(message_sender.send(PumpMessage {
                                    message: Err(error),
                                    received_ns: monotonic_now_ns(),
                                    received_at: Instant::now(),
                                }).await);
                                break;
                            }
                        }
                    }
                }
            }
        });
        Self {
            commands,
            messages,
            control_activity,
            control_closed: false,
            task,
        }
    }

    fn begin_attempt(&mut self) {
        drop(self.control_activity.borrow_and_update());
    }

    async fn send(&self, message: Message) -> std::result::Result<(), WebSocketError> {
        let (result, receiver) = oneshot::channel();
        self.commands
            .send(SocketCommand::Send { message, result })
            .await
            .map_err(|_| WebSocketError::ConnectionClosed)?;
        receiver
            .await
            .unwrap_or(Err(WebSocketError::ConnectionClosed))
    }

    async fn next(&mut self) -> Option<PumpEvent> {
        loop {
            if self.control_closed {
                return self.messages.recv().await.map(PumpEvent::Message);
            }
            tokio::select! {
                biased;
                message = self.messages.recv() => {
                    return message.map(PumpEvent::Message);
                }
                changed = self.control_activity.changed() => {
                    match changed {
                        Ok(()) => {
                            if let Some(received_at) = *self.control_activity.borrow_and_update() {
                                return Some(PumpEvent::Control(received_at));
                            }
                        }
                        Err(_) => {
                            self.control_closed = true;
                        }
                    }
                }
            }
        }
    }
}

impl Drop for SocketPump {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn map_handshake_error(error: WebSocketError) -> ResponsesError {
    let WebSocketError::Http(response) = error else {
        return ResponsesError::Handshake {
            reconnectable: is_transient_websocket(&error),
            detail: error.to_string(),
        };
    };
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<f64>().ok())
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
    let body = response.body().as_deref().map_or_else(
        || "empty response body".to_owned(),
        |body| String::from_utf8_lossy(body).into_owned(),
    );
    ResponsesError::HandshakeRejected {
        status,
        body,
        retry_after,
    }
}

fn map_send_error(error: WebSocketError) -> ResponsesError {
    ResponsesError::Send {
        reconnectable: is_transient_websocket(&error),
        detail: error.to_string(),
    }
}

fn map_receive_error(error: WebSocketError) -> ResponsesError {
    ResponsesError::Receive {
        reconnectable: is_transient_websocket(&error),
        detail: error.to_string(),
    }
}

const fn is_transient_websocket(error: &WebSocketError) -> bool {
    matches!(
        error,
        WebSocketError::ConnectionClosed
            | WebSocketError::AlreadyClosed
            | WebSocketError::Io(_)
            | WebSocketError::Protocol(
                ProtocolError::HandshakeIncomplete
                    | ProtocolError::ResetWithoutClosingHandshake
                    | ProtocolError::SendAfterClosing
            )
    )
}

fn header_string(
    headers: &tokio_tungstenite::tungstenite::http::HeaderMap,
    name: &str,
) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use std::{
        env,
        future::pending,
        process::{Command, Stdio},
        time::Duration,
    };

    use eyre::{Result, eyre};
    use futures_util::{SinkExt, StreamExt};
    use tokio::{
        net::TcpListener,
        sync::{mpsc, watch},
        time::{Instant, advance, timeout},
    };
    use tokio_tungstenite::{
        accept_async, accept_hdr_async,
        tungstenite::{Message, handshake::server::Request},
    };

    use super::{
        PumpMessage, ResponsesSocket, SOCKET_MESSAGE_CAPACITY, SocketPump, parse_raw_json,
        turn_state_from_event,
    };

    #[test]
    fn only_decodes_turn_state_metadata_events() {
        assert_eq!(
            turn_state_from_event(
                r#"{"headers":{"X-Codex-Turn-State":"state-1"},"type":"response.metadata"}"#,
            )
            .as_deref(),
            Some("state-1")
        );
        assert_eq!(
            turn_state_from_event(
                r#"{"type":"response.output_text.delta","delta":"ordinary output"}"#,
            ),
            None
        );
    }

    #[tokio::test]
    #[allow(
        clippy::result_large_err,
        reason = "tungstenite fixes the handshake callback's error response type"
    )]
    async fn respects_http_proxy_for_websocket_connections() -> Result<()> {
        run_proxy_test(
            "HTTP_PROXY",
            "ws://unreachable.nanocodex.invalid/v1/responses",
            "unreachable.nanocodex.invalid:80",
            None,
        )
        .await
    }

    #[tokio::test]
    #[allow(
        clippy::result_large_err,
        reason = "tungstenite fixes the handshake callback's error response type"
    )]
    async fn respects_https_proxy_for_secure_websocket_connections() -> Result<()> {
        run_proxy_test(
            "HTTPS_PROXY",
            "wss://unreachable.nanocodex.invalid/v1/responses",
            "unreachable.nanocodex.invalid:443",
            Some(502),
        )
        .await
    }

    #[allow(
        clippy::result_large_err,
        reason = "tungstenite fixes the handshake callback's error response type"
    )]
    async fn run_proxy_test(
        proxy_env: &str,
        endpoint: &str,
        expected_authority: &str,
        rejection_status: Option<u16>,
    ) -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_address = listener.local_addr()?;
        let test_binary = env::current_exe()?;
        let mut command = Command::new(test_binary);
        command
            .args([
                "--exact",
                "transport::socket::tests::proxy_connection_child",
                "--ignored",
                "--nocapture",
            ])
            .env("NANOCODEX_HTTP_PROXY_TEST_CHILD", "1")
            .env("NANOCODEX_HTTP_PROXY_TEST_ENDPOINT", endpoint)
            .env_remove("HTTP_PROXY")
            .env_remove("http_proxy")
            .env_remove("HTTPS_PROXY")
            .env_remove("https_proxy")
            .env_remove("ALL_PROXY")
            .env_remove("all_proxy")
            .env(proxy_env, format!("http://{proxy_address}"))
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(status) = rejection_status {
            command.env(
                "NANOCODEX_HTTP_PROXY_TEST_EXPECT_REJECTION",
                status.to_string(),
            );
        }
        let child = command.spawn()?;

        let accepted = timeout(Duration::from_secs(5), listener.accept()).await;
        let (stream, _) = if let Ok(connection) = accepted {
            connection?
        } else {
            let output = child.wait_with_output()?;
            return Err(eyre!(
                "WebSocket transport never contacted {proxy_env}; child status: {}; stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ));
        };

        let mut request = Vec::new();
        loop {
            stream.readable().await?;
            let mut bytes = [0_u8; 1024];
            match stream.try_read(&mut bytes) {
                Ok(0) => return Err(eyre!("proxy client closed before CONNECT completed")),
                Ok(read) => {
                    request.extend_from_slice(&bytes[..read]);
                    if request.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }
        let request = String::from_utf8(request)?;
        assert!(
            request.starts_with(&format!("CONNECT {expected_authority} HTTP/1.1\r\n")),
            "unexpected proxy request: {request:?}"
        );

        let response = rejection_status.map_or_else(
            || "HTTP/1.1 200 Connection Established\r\n\r\n".to_owned(),
            |status| format!("HTTP/1.1 {status} Bad Gateway\r\nContent-Length: 0\r\n\r\n"),
        );
        let mut written = 0;
        while written < response.len() {
            stream.writable().await?;
            match stream.try_write(&response.as_bytes()[written..]) {
                Ok(0) => return Err(eyre!("proxy client closed before CONNECT response")),
                Ok(count) => written += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
        }

        if rejection_status.is_none() {
            let socket =
                accept_hdr_async(stream, |_request: &Request, response| Ok(response)).await?;
            drop(socket);
        }
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "proxy child failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }

    #[tokio::test]
    #[ignore = "run only as the isolated child of the proxy connection tests"]
    async fn proxy_connection_child() -> Result<()> {
        if env::var_os("NANOCODEX_HTTP_PROXY_TEST_CHILD").is_none() {
            return Ok(());
        }
        let endpoint = env::var("NANOCODEX_HTTP_PROXY_TEST_ENDPOINT")?;
        let auth = crate::OpenAiAuth::api_key("test-key").snapshot().await?;
        let result = ResponsesSocket::connect(
            &endpoint,
            &auth,
            "session-proxy",
            None,
            Duration::from_secs(300),
        )
        .await;
        let expected_rejection = env::var("NANOCODEX_HTTP_PROXY_TEST_EXPECT_REJECTION")
            .ok()
            .map(|status| status.parse::<u16>())
            .transpose()?;
        match (result, expected_rejection) {
            (Ok(_), None) | (Err(_), Some(_)) => Ok(()),
            (Err(error), None) => Err(error.into()),
            (Ok(_), Some(expected)) => Err(eyre!(
                "proxy connection succeeded; expected HTTP {expected} rejection"
            )),
        }
    }

    #[tokio::test]
    #[allow(
        clippy::result_large_err,
        reason = "tungstenite fixes the handshake callback's error response type"
    )]
    async fn answers_ping_while_response_consumer_is_idle() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let keepalive = b"keepalive".to_vec();
        let expected_keepalive = keepalive.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut socket = accept_hdr_async(stream, |request: &Request, response| {
                assert_eq!(
                    request
                        .headers()
                        .get("authorization")
                        .and_then(|v| v.to_str().ok()),
                    Some("Bearer subscription-token")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("ChatGPT-Account-ID")
                        .and_then(|v| v.to_str().ok()),
                    Some("account-test")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("X-OpenAI-Fedramp")
                        .and_then(|v| v.to_str().ok()),
                    Some("true")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("session-id")
                        .and_then(|v| v.to_str().ok()),
                    Some("session-test")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("thread-id")
                        .and_then(|v| v.to_str().ok()),
                    Some("session-test")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("x-client-request-id")
                        .and_then(|v| v.to_str().ok()),
                    Some("session-test")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("OpenAI-Beta")
                        .and_then(|v| v.to_str().ok()),
                    Some("responses_websockets=2026-02-06")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("x-openai-internal-codex-responses-lite")
                        .and_then(|v| v.to_str().ok()),
                    Some("true")
                );
                assert_eq!(
                    request
                        .headers()
                        .get("x-codex-turn-state")
                        .and_then(|value| value.to_str().ok()),
                    Some("turn-state-test")
                );
                Ok(response)
            })
            .await?;
            socket.send(Message::Ping(keepalive.into())).await?;
            let reply = timeout(Duration::from_secs(1), socket.next())
                .await
                .map_err(|_| eyre!("client did not answer WebSocket ping"))?
                .ok_or_else(|| eyre!("client closed before answering WebSocket ping"))??;
            assert_eq!(reply, Message::Pong(expected_keepalive.into()));
            socket
                .send(Message::Text(r#"{"type":"probe"}"#.into()))
                .await?;
            socket.send(Message::Binary(b"{}".to_vec().into())).await?;
            Result::<()>::Ok(())
        });

        let endpoint = format!("ws://{address}");
        let auth = crate::OpenAiAuthSnapshot::new(
            crate::OpenAiAuthMode::ChatGpt,
            "subscription-token",
            Some("account-test"),
            true,
            1,
        );
        let (mut socket, _) = ResponsesSocket::connect(
            &endpoint,
            &auth,
            "session-test",
            Some("turn-state-test"),
            Duration::from_secs(300),
        )
        .await?;

        server.await??;
        assert!(
            socket.pump.control_activity.has_changed()?,
            "the socket pump must publish Ping/Pong activity"
        );
        socket.begin_attempt();
        let text = socket.next_text_or_idle_timeout().await?;
        assert_eq!(
            parse_raw_json(text.text.as_str())?.get(),
            r#"{"type":"probe"}"#
        );
        assert!(matches!(
            socket.next_text_or_idle_timeout().await,
            Err(crate::ResponsesError::UnexpectedBinary)
        ));
        Ok(())
    }

    #[tokio::test]
    async fn response_socket_backlog_is_bounded_while_the_consumer_is_idle() -> Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await?;
            let mut socket = accept_async(stream).await?;
            for index in 0..SOCKET_MESSAGE_CAPACITY * 2 {
                socket
                    .send(Message::Text(format!(r#"{{"index":{index}}}"#).into()))
                    .await?;
            }
            Result::<()>::Ok(())
        });

        let auth = crate::OpenAiAuthSnapshot::new(
            crate::OpenAiAuthMode::ApiKey,
            "test-key",
            None::<&str>,
            false,
            0,
        );
        let (socket, _) = ResponsesSocket::connect(
            &format!("ws://{address}"),
            &auth,
            "bounded-backlog",
            None,
            Duration::from_secs(300),
        )
        .await?;
        server.await??;

        timeout(Duration::from_secs(1), async {
            while socket.pump.messages.len() < SOCKET_MESSAGE_CAPACITY {
                tokio::task::yield_now().await;
            }
        })
        .await
        .map_err(|_| eyre!("socket pump did not receive the test backlog"))?;
        for _ in 0..SOCKET_MESSAGE_CAPACITY {
            tokio::task::yield_now().await;
        }

        assert!(
            socket.pump.messages.len() <= SOCKET_MESSAGE_CAPACITY,
            "idle response consumer accumulated {} frames",
            socket.pump.messages.len()
        );
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn control_activity_resets_the_deadline_without_becoming_a_model_event() {
        let (mut socket, messages, control_activity) =
            test_socket_with_control_activity(Duration::from_millis(100));
        socket.begin_attempt();
        let received = tokio::spawn(async move { socket.next_text_or_idle_timeout().await });
        tokio::task::yield_now().await;

        for _ in 0..2 {
            advance(Duration::from_millis(90)).await;
            control_activity.send_replace(Some(Instant::now()));
            tokio::task::yield_now().await;
            assert!(
                !received.is_finished(),
                "a WebSocket control frame must not surface as a model event"
            );
        }

        advance(Duration::from_millis(90)).await;
        messages
            .send(PumpMessage {
                message: Ok(Message::Text(r#"{"type":"after-controls"}"#.into())),
                received_ns: 1,
                received_at: Instant::now(),
            })
            .await
            .expect("test socket should still be receiving messages");
        let received = received
            .await
            .expect("test receive task should finish")
            .expect("control activity should keep the receive alive");
        assert_eq!(received.text.as_str(), r#"{"type":"after-controls"}"#);
    }

    #[tokio::test(start_paused = true)]
    async fn pre_attempt_control_activity_does_not_extend_true_silence() {
        let (mut socket, _messages, control_activity) =
            test_socket_with_control_activity(Duration::from_millis(100));
        control_activity.send_replace(Some(Instant::now()));
        socket.begin_attempt();

        let received = tokio::spawn(async move { socket.next_text_or_idle_timeout().await });
        tokio::task::yield_now().await;
        advance(Duration::from_millis(99)).await;
        assert!(!received.is_finished());
        advance(Duration::from_millis(2)).await;
        tokio::task::yield_now().await;

        assert!(matches!(
            received.await,
            Ok(Err(crate::ResponsesError::IdleTimeout { seconds: 0 }))
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn stale_ping_after_text_does_not_move_silence_deadline_backward() {
        let (mut socket, messages, control_activity) =
            test_socket_with_control_activity(Duration::from_millis(100));
        socket.begin_attempt();

        advance(Duration::from_millis(10)).await;
        control_activity.send_replace(Some(Instant::now()));
        advance(Duration::from_millis(10)).await;
        messages
            .send(PumpMessage {
                message: Ok(Message::Text(r#"{"type":"newer-text"}"#.into())),
                received_ns: 1,
                received_at: Instant::now(),
            })
            .await
            .expect("test socket should still be receiving messages");

        let text = socket
            .next_text_or_idle_timeout()
            .await
            .expect("queued text should arrive");
        assert_eq!(text.text.as_str(), r#"{"type":"newer-text"}"#);

        let silence = tokio::spawn(async move { socket.next_text_or_idle_timeout().await });
        tokio::task::yield_now().await;
        advance(Duration::from_millis(99)).await;
        assert!(
            !silence.is_finished(),
            "a stale Ping timestamp must not shorten the newer text deadline"
        );
        advance(Duration::from_millis(2)).await;
        tokio::task::yield_now().await;

        assert!(matches!(
            silence.await,
            Ok(Err(crate::ResponsesError::IdleTimeout { seconds: 0 }))
        ));
    }

    fn test_socket_with_control_activity(
        stream_idle_timeout: Duration,
    ) -> (
        ResponsesSocket,
        mpsc::Sender<PumpMessage>,
        watch::Sender<Option<Instant>>,
    ) {
        let (commands, command_receiver) = mpsc::channel(1);
        let (message_sender, messages) = mpsc::channel::<PumpMessage>(SOCKET_MESSAGE_CAPACITY);
        let (control_activity_sender, control_activity) = watch::channel(None);
        let task = tokio::spawn(async move {
            pending::<()>().await;
            drop(command_receiver);
        });
        (
            ResponsesSocket {
                pump: SocketPump {
                    commands,
                    messages,
                    control_activity,
                    control_closed: false,
                    task,
                },
                turn_state: None,
                stream_idle_timeout,
                last_activity_at: Instant::now(),
            },
            message_sender,
            control_activity_sender,
        )
    }
}
