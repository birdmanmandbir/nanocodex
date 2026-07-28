use std::time::Duration;

use crate::{OpenAiAuthSnapshot, ResponsesError, monotonic_now_ns};

use super::{HostConnectRequest, HostConnection, HostError, HostMessage, HostTransport};
use crate::transport::{EncodedRequest, wire::turn_state_from_event};

pub(crate) use crate::transport::wire::{decode_event, parse_raw_json};

pub(crate) struct ConnectionMetadata {
    pub status: u16,
    pub request_id: Option<String>,
    pub server_model: Option<String>,
    pub reasoning_included: bool,
    pub turn_state: Option<String>,
}

pub(crate) struct ResponsesSocket {
    connection: Box<dyn HostConnection>,
    turn_state: Option<String>,
    stream_idle_timeout: Duration,
}

pub(crate) struct ReceivedText {
    pub text: String,
    pub received_ns: u64,
}

impl ResponsesSocket {
    pub(crate) async fn connect(
        host: &dyn HostTransport,
        endpoint: &str,
        auth: &OpenAiAuthSnapshot,
        session_id: &str,
        turn_state: Option<&str>,
        stream_idle_timeout: Duration,
    ) -> Result<(Self, ConnectionMetadata), ResponsesError> {
        let request = HostConnectRequest::new(
            endpoint,
            auth.bearer(),
            auth.account_id(),
            auth.is_fedramp(),
            session_id,
            turn_state,
        );
        let (connection, metadata) = host
            .connect(request)
            .await
            .map_err(|error| match error {
                HostError::HandshakeRejected {
                    status,
                    body,
                    retry_after,
                } => ResponsesError::HandshakeRejected {
                    status,
                    body,
                    retry_after,
                },
                error => ResponsesError::Handshake {
                    reconnectable: error.is_reconnectable(),
                    detail: error.to_string(),
                },
            })?
            .into_parts();
        let metadata = ConnectionMetadata {
            status: metadata.status,
            request_id: metadata.request_id,
            server_model: metadata.server_model,
            reasoning_included: metadata.reasoning_included,
            turn_state: metadata.turn_state,
        };
        Ok((
            Self {
                connection,
                turn_state: metadata.turn_state.clone(),
                stream_idle_timeout,
            },
            metadata,
        ))
    }

    pub(crate) const fn begin_attempt(&mut self) {}

    pub(crate) async fn send(&self, request: EncodedRequest) -> Result<(), ResponsesError> {
        self.connection
            .send(request.raw().get())
            .await
            .map_err(|error| {
                let reconnectable = error.is_reconnectable();
                ResponsesError::Send {
                    reconnectable,
                    detail: error.to_string(),
                }
            })
    }

    pub(crate) async fn next_text_or_idle_timeout(
        &mut self,
    ) -> Result<ReceivedText, ResponsesError> {
        match self
            .connection
            .next(self.stream_idle_timeout)
            .await
            .map_err(|error| {
                let reconnectable = error.is_reconnectable();
                ResponsesError::Receive {
                    reconnectable,
                    detail: error.to_string(),
                }
            })? {
            HostMessage::Text(text) => {
                self.capture_turn_state(&text);
                Ok(ReceivedText {
                    text,
                    received_ns: monotonic_now_ns(),
                })
            }
            HostMessage::Closed { detail } => Err(ResponsesError::Closed { detail }),
            HostMessage::Timeout => Err(ResponsesError::IdleTimeout {
                seconds: self.stream_idle_timeout.as_secs(),
            }),
            HostMessage::Binary => Err(ResponsesError::UnexpectedBinary),
        }
    }

    pub(crate) fn turn_state(&self) -> Option<&str> {
        self.turn_state.as_deref()
    }

    pub(crate) fn reset_turn_state(&mut self) {
        self.turn_state = None;
    }

    fn capture_turn_state(&mut self, text: &str) {
        if self.turn_state.is_none() {
            self.turn_state = turn_state_from_event(text);
        }
    }
}

impl Drop for ResponsesSocket {
    fn drop(&mut self) {
        self.connection.close();
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use eyre::Result;

    use crate::{
        EncodedRequest,
        transport::host::{
            ConnectedHost, HostConnectRequest, HostConnection, HostConnectionMetadata, HostError,
            HostFuture, HostMessage, HostTransport,
        },
    };

    use super::{ResponsesSocket, decode_event, parse_raw_json};

    #[derive(Default)]
    struct RecordedHost {
        sent: Vec<String>,
        next_timeout: Option<Duration>,
        closed: bool,
    }

    struct TestHost {
        recorded: Arc<Mutex<RecordedHost>>,
    }

    struct TestConnection {
        recorded: Arc<Mutex<RecordedHost>>,
    }

    impl HostTransport for TestHost {
        fn connect<'a>(
            &'a self,
            request: HostConnectRequest<'a>,
        ) -> HostFuture<'a, Result<ConnectedHost, HostError>> {
            let recorded = Arc::clone(&self.recorded);
            Box::pin(async move {
                assert_eq!(request.endpoint(), "wss://host.test/responses");
                assert_eq!(request.bearer_token(), "test-key");
                assert_eq!(request.session_id(), "session-host");
                Ok(ConnectedHost::new(
                    TestConnection { recorded },
                    HostConnectionMetadata::new(101)
                        .with_request_id("request-host")
                        .with_server_model("gpt-test")
                        .with_reasoning_included(true),
                ))
            })
        }

        fn sleep<'a>(&'a self, _session_id: &'a str, _duration: Duration) -> HostFuture<'a, ()> {
            Box::pin(async {})
        }
    }

    impl HostConnection for TestConnection {
        fn send<'a>(&'a self, message: &'a str) -> HostFuture<'a, Result<(), HostError>> {
            Box::pin(async move {
                self.recorded
                    .lock()
                    .expect("recorded host lock should remain available")
                    .sent
                    .push(message.to_owned());
                Ok(())
            })
        }

        fn next(
            &mut self,
            idle_timeout: Duration,
        ) -> HostFuture<'_, Result<HostMessage, HostError>> {
            Box::pin(async move {
                self.recorded
                    .lock()
                    .expect("recorded host lock should remain available")
                    .next_timeout = Some(idle_timeout);
                Ok(HostMessage::Text(
                    r#"{"type":"response.metadata","headers":{"x-codex-turn-state":"event-state"}}"#
                        .to_owned(),
                ))
            })
        }

        fn close(&mut self) {
            self.recorded
                .lock()
                .expect("recorded host lock should remain available")
                .closed = true;
        }
    }

    #[tokio::test]
    async fn hosted_socket_receives_the_exact_configured_timeout() -> Result<()> {
        let configured = Duration::from_secs(37);
        let recorded = Arc::new(Mutex::new(RecordedHost::default()));
        let host = TestHost {
            recorded: Arc::clone(&recorded),
        };
        let auth = crate::OpenAiAuth::api_key("test-key").snapshot().await?;
        let (mut socket, metadata) = ResponsesSocket::connect(
            &host,
            "wss://host.test/responses",
            &auth,
            "session-host",
            None,
            configured,
        )
        .await?;

        assert_eq!(metadata.status, 101);
        assert_eq!(metadata.request_id.as_deref(), Some("request-host"));
        assert_eq!(metadata.server_model.as_deref(), Some("gpt-test"));
        assert!(metadata.reasoning_included);
        assert_eq!(metadata.turn_state, None);

        socket.begin_attempt();
        socket
            .send(EncodedRequest::new(&serde_json::json!({"probe": true}))?)
            .await?;
        let received = socket.next_text_or_idle_timeout().await?;
        let event = parse_raw_json(&received.text)?;
        let decoded: serde_json::Value = decode_event(event)?;
        assert_eq!(decoded["type"], "response.metadata");
        let _received_ns = received.received_ns;
        assert_eq!(socket.turn_state(), Some("event-state"));
        socket.reset_turn_state();
        assert_eq!(socket.turn_state(), None);
        drop(socket);

        let recorded = recorded
            .lock()
            .expect("recorded host lock should remain available");
        assert_eq!(recorded.sent, [r#"{"probe":true}"#]);
        assert_eq!(recorded.next_timeout, Some(configured));
        assert!(recorded.closed);
        Ok(())
    }
}
