use std::{fmt, time::Duration};

use futures_util::{SinkExt, StreamExt};
use tokio::{sync::mpsc, time::timeout};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::{HeaderValue, header},
    },
};

use super::{
    api::{AdmissionReceipt, RoomError, RoomMembership},
    protocol::{
        MessageId, Ready, RoomClientCommand, RoomCursor, RoomServerMessage, RoomTarget,
        validated_message_text,
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const COMMAND_CAPACITY: usize = 32;
const EVENT_CAPACITY: usize = 256;

impl<R: AdmissionReceipt> RoomMembership<R> {
    /// Connects this retained membership from a durable event cursor.
    ///
    /// The membership cookie remains owned by `self`, so callers can reconnect
    /// from the last observed cursor without receiving or persisting it.
    pub(crate) async fn connect(
        &self,
        cursor: &RoomCursor,
    ) -> Result<(RoomConnection, RoomEvents), RoomError> {
        let mut websocket_url = self.receipt.websocket_url().clone();
        websocket_url
            .query_pairs_mut()
            .clear()
            .append_pair("cursor", cursor.as_str());
        let mut request = websocket_url
            .as_str()
            .into_client_request()
            .map_err(|_| RoomError::InvalidReceipt("room WebSocket URL is invalid"))?;

        let mut cookie = HeaderValue::from_str(self.cookie.as_str())
            .map_err(|_| RoomError::InvalidReceipt("room membership cookie is invalid"))?;
        cookie.set_sensitive(true);
        request.headers_mut().insert(header::COOKIE, cookie);
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_str(&self.origin.origin().ascii_serialization())
                .map_err(|_| RoomError::InvalidReceipt("room origin header is invalid"))?,
        );

        let (mut socket, _) = timeout(CONNECT_TIMEOUT, connect_async(request))
            .await
            .map_err(|_| RoomError::HandshakeTimeout)?
            .map_err(RoomError::WebSocket)?;
        let ready = match socket.next().await {
            Some(Ok(Message::Text(encoded))) => {
                RoomServerMessage::decode(encoded.as_str())?.into_ready()?
            }
            Some(Ok(Message::Close(_))) | None => return Err(RoomError::ClosedBeforeReady),
            Some(Ok(_)) => {
                return Err(RoomError::InvalidReceipt(
                    "room WebSocket did not begin with a text ready message",
                ));
            }
            Some(Err(error)) => return Err(RoomError::WebSocket(error)),
        };
        if ready.room_id != *self.receipt.room_id() || ready.member_id != *self.receipt.member_id()
        {
            return Err(RoomError::InvalidReceipt(
                "room ready message changed the admitted identity",
            ));
        }

        let (commands, command_rx) = mpsc::channel(COMMAND_CAPACITY);
        let (event_tx, events) = mpsc::channel(EVENT_CAPACITY);
        let task = tokio::spawn(pump(socket, command_rx, event_tx));
        Ok((
            RoomConnection { ready, commands },
            RoomEvents { events, task },
        ))
    }
}

pub(crate) struct RoomConnection {
    ready: Ready,
    commands: mpsc::Sender<SocketCommand>,
}

impl RoomConnection {
    pub(crate) fn ready(&self) -> &Ready {
        &self.ready
    }

    pub(crate) async fn say_room(&self, text: &str) -> Result<MessageId, RoomError> {
        self.say(text, RoomTarget::Room).await
    }

    pub(crate) async fn say_agent(&self, text: &str) -> Result<MessageId, RoomError> {
        self.say(text, RoomTarget::Agent).await
    }

    pub(crate) async fn send(
        &self,
        id: &MessageId,
        text: &str,
        target: RoomTarget,
    ) -> Result<(), RoomError> {
        let text = validated_message_text(text)?;
        self.command(RoomClientCommand::Say { id, text, target })
            .await
    }

    pub(crate) async fn ping(&self, nonce: Option<&str>) -> Result<(), RoomError> {
        self.command(RoomClientCommand::Ping { nonce }).await
    }

    pub(crate) async fn close(&self) -> Result<(), RoomError> {
        self.commands
            .send(SocketCommand::Close)
            .await
            .map_err(|_| RoomError::CommandChannelClosed)
    }

    async fn say(&self, text: &str, target: RoomTarget) -> Result<MessageId, RoomError> {
        let id = MessageId::generate();
        self.send(&id, text, target).await?;
        Ok(id)
    }

    async fn command(&self, command: RoomClientCommand<'_>) -> Result<(), RoomError> {
        let encoded = serde_json::to_string(&command)
            .map_err(|_| RoomError::InvalidReceipt("failed to encode room command"))?;
        self.commands
            .send(SocketCommand::Text(encoded))
            .await
            .map_err(|_| RoomError::CommandChannelClosed)
    }
}

impl fmt::Debug for RoomConnection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoomConnection")
            .field("ready", &self.ready)
            .finish_non_exhaustive()
    }
}

pub(crate) struct RoomEvents {
    events: mpsc::Receiver<Result<RoomServerMessage, RoomError>>,
    task: tokio::task::JoinHandle<()>,
}

impl RoomEvents {
    /// Returns ordered replay and live room messages. Replay pagination control
    /// is acknowledged automatically before `ReplayPaused` is yielded.
    pub(crate) async fn next(&mut self) -> Option<Result<RoomServerMessage, RoomError>> {
        self.events.recv().await
    }
}

impl fmt::Debug for RoomEvents {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RoomEvents").finish_non_exhaustive()
    }
}

impl Drop for RoomEvents {
    fn drop(&mut self) {
        self.task.abort();
    }
}

enum SocketCommand {
    Text(String),
    Close,
}

async fn pump<S>(
    socket: tokio_tungstenite::WebSocketStream<S>,
    mut commands: mpsc::Receiver<SocketCommand>,
    events: mpsc::Sender<Result<RoomServerMessage, RoomError>>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (mut sink, mut stream) = socket.split();
    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(SocketCommand::Text(encoded)) => {
                        if let Err(error) = sink.send(Message::Text(encoded.into())).await {
                            let _ = events.send(Err(RoomError::WebSocket(error))).await;
                            return;
                        }
                    }
                    Some(SocketCommand::Close) | None => {
                        let _ = sink.send(Message::Close(None)).await;
                        return;
                    }
                }
            }
            message = stream.next() => {
                match message {
                    Some(Ok(Message::Text(encoded))) => {
                        let decoded = match RoomServerMessage::decode(encoded.as_str()) {
                            Ok(message) => message,
                            Err(error) => {
                                let _ = events.send(Err(error.into())).await;
                                return;
                            }
                        };
                        if let RoomServerMessage::ReplayPaused { cursor, .. } = &decoded {
                            let ack = RoomClientCommand::Ack { cursor };
                            let encoded = match serde_json::to_string(&ack) {
                                Ok(encoded) => encoded,
                                Err(_) => {
                                    let _ = events.send(Err(RoomError::InvalidReceipt(
                                        "failed to encode replay acknowledgement",
                                    ))).await;
                                    return;
                                }
                            };
                            if let Err(error) = sink.send(Message::Text(encoded.into())).await {
                                let _ = events.send(Err(RoomError::WebSocket(error))).await;
                                return;
                            }
                        }
                        if events.send(Ok(decoded)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if let Err(error) = sink.send(Message::Pong(payload)).await {
                            let _ = events.send(Err(RoomError::WebSocket(error))).await;
                            return;
                        }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {
                        let _ = events.send(Err(RoomError::InvalidReceipt(
                            "managed room server sent a non-text data frame",
                        ))).await;
                        return;
                    }
                    Some(Err(error)) => {
                        let _ = events.send(Err(RoomError::WebSocket(error))).await;
                        return;
                    }
                }
            }
        }
    }
}
