//! Authenticated loopback forwarding for coordinators over iroh.

use std::{fmt, net::SocketAddr, str::FromStr, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use constant_time_eq::constant_time_eq_32;
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::presets,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use serde::{Deserialize, Serialize};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::JoinSet,
};

const ALPN: &[u8] = b"nanocodex-eval-coordinator/1";
const TICKET_PREFIX: &str = "iroh-eval:";
const TICKET_VERSION: u8 = 1;
const MAX_TICKET_BYTES: usize = 16 * 1024;
const TOKEN_BYTES: usize = 32;
const ONLINE_TIMEOUT: Duration = Duration::from_secs(60);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_STREAMS: usize = 32;

#[cfg(test)]
pub(super) static TEST_ENDPOINT_PERMIT: Semaphore = Semaphore::const_new(1);

/// One ephemeral capability for reaching a coordinator over iroh.
///
/// The ticket contains the coordinator's authenticated iroh address and a
/// random bearer capability. Treat its string representation as a secret.
#[derive(Clone)]
pub struct IrohCoordinatorTicket {
    address: EndpointAddr,
    token: [u8; TOKEN_BYTES],
    encoded: String,
}

/// Running iroh endpoint that forwards authorized streams to one coordinator.
pub struct IrohCoordinatorServer {
    router: Router,
}

/// Worker-side bridge from a loopback TCP listener to an iroh coordinator.
pub struct IrohCoordinatorConnector;

#[derive(Clone)]
struct CoordinatorDialer {
    endpoint: Endpoint,
    ticket: Arc<IrohCoordinatorTicket>,
    connection: Arc<Mutex<Option<iroh::endpoint::Connection>>>,
}

/// Iroh coordinator ticket, endpoint, or forwarding failure.
#[derive(Debug, thiserror::Error)]
pub enum IrohCoordinatorError {
    /// The supplied ticket is malformed or uses an unsupported version.
    #[error("invalid iroh evaluation coordinator ticket: {0}")]
    InvalidTicket(String),
    /// A bridge was asked to expose or target a non-loopback TCP address.
    #[error("invalid iroh evaluation coordinator loopback address: {0}")]
    InvalidLoopback(SocketAddr),
    /// Creating or shutting down an iroh endpoint failed.
    #[error("iroh evaluation coordinator endpoint failed: {0}")]
    Endpoint(String),
    /// An authenticated iroh stream could not be established or forwarded.
    #[error("iroh evaluation coordinator protocol failed: {0}")]
    Protocol(String),
    /// Loopback forwarding failed.
    #[error("iroh evaluation coordinator forwarding failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Serialize, Deserialize)]
struct WireTicket {
    version: u8,
    address: EndpointAddr,
    token: [u8; TOKEN_BYTES],
}

#[derive(Clone, Debug)]
struct CoordinatorProtocol {
    origin: SocketAddr,
    token: [u8; TOKEN_BYTES],
    streams: Arc<Semaphore>,
}

impl IrohCoordinatorTicket {
    fn from_parts(
        address: EndpointAddr,
        token: [u8; TOKEN_BYTES],
    ) -> Result<Self, IrohCoordinatorError> {
        let payload = serde_json::to_vec(&WireTicket {
            version: TICKET_VERSION,
            address: address.clone(),
            token,
        })
        .map_err(|error| IrohCoordinatorError::InvalidTicket(error.to_string()))?;
        let encoded = URL_SAFE_NO_PAD.encode(payload);
        Ok(Self {
            address,
            token,
            encoded,
        })
    }
}

impl fmt::Debug for IrohCoordinatorTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IrohCoordinatorTicket")
            .field("endpoint_id", &self.address.id)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl fmt::Display for IrohCoordinatorTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{TICKET_PREFIX}{}", self.encoded)
    }
}

impl FromStr for IrohCoordinatorTicket {
    type Err = IrohCoordinatorError;

    fn from_str(ticket: &str) -> Result<Self, Self::Err> {
        let encoded = ticket.strip_prefix(TICKET_PREFIX).ok_or_else(|| {
            IrohCoordinatorError::InvalidTicket(format!(
                "expected a ticket beginning with {TICKET_PREFIX}"
            ))
        })?;
        if encoded.is_empty() || encoded.len() > MAX_TICKET_BYTES {
            return Err(IrohCoordinatorError::InvalidTicket(
                "ticket payload has an invalid length".to_owned(),
            ));
        }
        let payload = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|error| IrohCoordinatorError::InvalidTicket(error.to_string()))?;
        let WireTicket {
            version,
            address,
            token,
        } = serde_json::from_slice(&payload)
            .map_err(|error| IrohCoordinatorError::InvalidTicket(error.to_string()))?;
        if version != TICKET_VERSION {
            return Err(IrohCoordinatorError::InvalidTicket(format!(
                "unsupported ticket version {version}"
            )));
        }
        Self::from_parts(address, token)
    }
}

impl IrohCoordinatorServer {
    /// Starts a public-relay-capable iroh endpoint for one loopback coordinator.
    ///
    /// The returned ticket is ephemeral and becomes invalid when this server is
    /// replaced. The origin is fixed for the server lifetime, so an authorized
    /// peer cannot use the endpoint as a general-purpose proxy.
    pub async fn bind(
        origin: SocketAddr,
    ) -> Result<(Self, IrohCoordinatorTicket), IrohCoordinatorError> {
        let endpoint = Endpoint::bind(presets::N0)
            .await
            .map_err(|error| IrohCoordinatorError::Endpoint(error.to_string()))?;
        Self::spawn(origin, endpoint, true).await
    }

    pub(super) async fn spawn(
        origin: SocketAddr,
        endpoint: Endpoint,
        wait_until_online: bool,
    ) -> Result<(Self, IrohCoordinatorTicket), IrohCoordinatorError> {
        require_loopback(origin)?;
        let protocol = CoordinatorProtocol {
            origin,
            streams: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
            token: {
                let mut token = [0; TOKEN_BYTES];
                getrandom::fill(&mut token)
                    .map_err(|error| IrohCoordinatorError::Endpoint(error.to_string()))?;
                token
            },
        };
        let token = protocol.token;
        let router = Router::builder(endpoint).accept(ALPN, protocol).spawn();
        if wait_until_online {
            tokio::time::timeout(ONLINE_TIMEOUT, router.endpoint().online())
                .await
                .map_err(|_| {
                    IrohCoordinatorError::Endpoint(format!(
                        "did not connect to a relay within {} seconds",
                        ONLINE_TIMEOUT.as_secs()
                    ))
                })?;
        }
        let ticket = IrohCoordinatorTicket::from_parts(router.endpoint().addr(), token)?;
        Ok((Self { router }, ticket))
    }

    /// Gracefully closes the iroh endpoint and its active forwarded streams.
    pub async fn shutdown(self) -> Result<(), IrohCoordinatorError> {
        self.router
            .shutdown()
            .await
            .map_err(|error| IrohCoordinatorError::Endpoint(error.to_string()))
    }
}

impl IrohCoordinatorConnector {
    /// Forwards one worker-local loopback listener to the supplied coordinator.
    pub async fn serve(
        ticket: IrohCoordinatorTicket,
        listener: TcpListener,
    ) -> Result<(), IrohCoordinatorError> {
        let endpoint = Endpoint::bind(presets::N0)
            .await
            .map_err(|error| IrohCoordinatorError::Endpoint(error.to_string()))?;
        Self::serve_with_endpoint(ticket, listener, endpoint).await
    }

    pub(super) async fn serve_with_endpoint(
        ticket: IrohCoordinatorTicket,
        listener: TcpListener,
        endpoint: Endpoint,
    ) -> Result<(), IrohCoordinatorError> {
        require_loopback(listener.local_addr()?)?;
        let dialer = CoordinatorDialer {
            endpoint,
            ticket: Arc::new(ticket),
            connection: Arc::new(Mutex::new(None)),
        };
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept(), if connections.len() < MAX_CONCURRENT_STREAMS => {
                    let (stream, peer) = accepted?;
                    let dialer = dialer.clone();
                    connections.spawn(async move {
                        if let Err(error) = forward_downstream(dialer, stream).await {
                            tracing::warn!(%peer, %error, "iroh coordinator downstream closed");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%error, "iroh coordinator downstream task failed");
                    }
                }
            }
        }
    }
}

impl CoordinatorDialer {
    async fn open_bi(
        &self,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream), IrohCoordinatorError>
    {
        let mut connection = self.connection.lock().await;
        if let Some(active) = connection.as_ref() {
            match active.open_bi().await {
                Ok(streams) => return Ok(streams),
                Err(error) => {
                    tracing::debug!(%error, "reconnecting closed iroh coordinator connection");
                    *connection = None;
                }
            }
        }
        let active = self
            .endpoint
            .connect(self.ticket.address.clone(), ALPN)
            .await
            .map_err(|error| IrohCoordinatorError::Endpoint(error.to_string()))?;
        let streams = active
            .open_bi()
            .await
            .map_err(|error| IrohCoordinatorError::Endpoint(error.to_string()))?;
        *connection = Some(active);
        Ok(streams)
    }
}

impl ProtocolHandler for CoordinatorProtocol {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        let mut streams = JoinSet::new();
        loop {
            tokio::select! {
                accepted = connection.accept_bi() => {
                    let Ok((send, recv)) = accepted else {
                        break;
                    };
                    let Ok(permit) = self.streams.clone().acquire_owned().await else {
                        break;
                    };
                    let protocol = self.clone();
                    streams.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = protocol.forward_upstream(send, recv).await {
                            tracing::warn!(%remote, %error, "iroh coordinator upstream closed");
                        }
                    });
                }
                completed = streams.join_next(), if !streams.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%remote, %error, "iroh coordinator upstream task failed");
                    }
                }
            }
        }
        streams.abort_all();
        while streams.join_next().await.is_some() {}
        Ok(())
    }
}

impl CoordinatorProtocol {
    async fn forward_upstream(
        &self,
        send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) -> Result<(), IrohCoordinatorError> {
        let mut presented = [0; TOKEN_BYTES];
        tokio::time::timeout(AUTH_TIMEOUT, recv.read_exact(&mut presented))
            .await
            .map_err(|_| {
                IrohCoordinatorError::Protocol(format!(
                    "capability was not received within {} seconds",
                    AUTH_TIMEOUT.as_secs()
                ))
            })?
            .map_err(|error| IrohCoordinatorError::Protocol(error.to_string()))?;
        if !constant_time_eq_32(&presented, &self.token) {
            return Err(IrohCoordinatorError::Protocol(
                "remote endpoint presented an invalid capability".to_owned(),
            ));
        }
        let mut origin = TcpStream::connect(self.origin).await?;
        let mut iroh = tokio::io::join(recv, send);
        tokio::io::copy_bidirectional(&mut origin, &mut iroh).await?;
        Ok(())
    }
}

async fn forward_downstream(
    dialer: CoordinatorDialer,
    mut stream: TcpStream,
) -> Result<(), IrohCoordinatorError> {
    let (mut send, recv) = dialer.open_bi().await?;
    send.write_all(&dialer.ticket.token)
        .await
        .map_err(|error| IrohCoordinatorError::Protocol(error.to_string()))?;
    let mut iroh = tokio::io::join(recv, send);
    tokio::io::copy_bidirectional(&mut stream, &mut iroh).await?;
    Ok(())
}

const fn require_loopback(address: SocketAddr) -> Result<(), IrohCoordinatorError> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(IrohCoordinatorError::InvalidLoopback(address))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[test]
    fn tickets_round_trip_without_exposing_the_capability_in_debug() {
        let secret = iroh::SecretKey::from_bytes(&[7; 32]);
        let ticket =
            IrohCoordinatorTicket::from_parts(EndpointAddr::new(secret.public()), [9; TOKEN_BYTES])
                .unwrap();
        let encoded = ticket.to_string();
        let decoded = IrohCoordinatorTicket::from_str(&encoded).unwrap();

        assert_eq!(decoded.address, ticket.address);
        assert_eq!(decoded.token, ticket.token);
        assert!(!format!("{ticket:?}").contains(&encoded));
        for malformed in [
            "",
            "https://example.com",
            TICKET_PREFIX,
            "iroh-eval:not-base64!",
            "iroh-eval:e30",
        ] {
            assert!(
                IrohCoordinatorTicket::from_str(malformed).is_err(),
                "malformed ticket unexpectedly parsed: {malformed}"
            );
        }
        let oversized = format!("{TICKET_PREFIX}{}", "a".repeat(MAX_TICKET_BYTES + 1));
        assert!(IrohCoordinatorTicket::from_str(&oversized).is_err());

        let unsupported = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&WireTicket {
                version: TICKET_VERSION + 1,
                address: ticket.address.clone(),
                token: ticket.token,
            })
            .unwrap(),
        );
        assert!(IrohCoordinatorTicket::from_str(&format!("{TICKET_PREFIX}{unsupported}")).is_err());
    }

    #[test]
    fn forwarding_accepts_only_loopback_tcp_addresses() {
        assert!(require_loopback("127.0.0.1:8789".parse().unwrap()).is_ok());
        assert!(require_loopback("[::1]:8789".parse().unwrap()).is_ok());
        for address in ["0.0.0.0:8789", "10.0.0.1:8789", "[::]:8789"] {
            assert!(require_loopback(address.parse().unwrap()).is_err());
        }
    }

    #[tokio::test]
    async fn dialer_multiplexes_streams_on_one_quic_connection() {
        let _test_permit = TEST_ENDPOINT_PERMIT.acquire().await.unwrap();
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_address = origin.local_addr().unwrap();
        let origin_task = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = origin.accept().await.unwrap();
                let mut request = [0; 4];
                stream.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"ping");
                stream.write_all(b"pong").await.unwrap();
            }
        });
        let server_endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let (server, ticket) = IrohCoordinatorServer::spawn(origin_address, server_endpoint, false)
            .await
            .unwrap();
        let client_endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let dialer = CoordinatorDialer {
            endpoint: client_endpoint,
            ticket: Arc::new(ticket),
            connection: Arc::new(Mutex::new(None)),
        };

        let mut connection_ids = Vec::new();
        for _ in 0..2 {
            let (mut send, mut recv) = dialer.open_bi().await.unwrap();
            connection_ids.push(dialer.connection.lock().await.as_ref().unwrap().stable_id());
            send.write_all(&dialer.ticket.token).await.unwrap();
            send.write_all(b"ping").await.unwrap();
            send.finish().unwrap();
            assert_eq!(recv.read_to_end(4).await.unwrap(), b"pong");
        }

        assert_eq!(connection_ids[0], connection_ids[1]);
        origin_task.await.unwrap();
        drop(dialer);
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn loopback_tcp_crosses_an_authenticated_iroh_stream() {
        const TEST_TIMEOUT: Duration = Duration::from_secs(5);
        const VALID_CONNECTIONS: usize = 8;

        let _test_permit = TEST_ENDPOINT_PERMIT.acquire().await.unwrap();
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_address = origin.local_addr().unwrap();
        let server_endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let (server, ticket) = IrohCoordinatorServer::spawn(origin_address, server_endpoint, false)
            .await
            .unwrap();

        let invalid_client = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let invalid_connection = invalid_client
            .connect(ticket.address.clone(), ALPN)
            .await
            .unwrap();
        let (mut invalid_send, mut invalid_recv) = invalid_connection.open_bi().await.unwrap();
        let mut invalid_token = ticket.token;
        invalid_token[0] ^= 1;
        invalid_send.write_all(&invalid_token).await.unwrap();
        invalid_send.finish().unwrap();
        let _ = tokio::time::timeout(TEST_TIMEOUT, invalid_recv.read_to_end(1))
            .await
            .expect("invalid capability stream did not close");
        assert!(
            tokio::time::timeout(Duration::from_millis(250), origin.accept())
                .await
                .is_err(),
            "an invalid capability reached the coordinator origin"
        );
        invalid_client.close().await;

        let origin_task = tokio::spawn(async move {
            for _ in 0..VALID_CONNECTIONS {
                let (mut stream, _) = origin.accept().await.unwrap();
                let mut request = [0; 4];
                stream.read_exact(&mut request).await.unwrap();
                assert_eq!(&request, b"ping");
                stream.write_all(b"pong").await.unwrap();
            }
        });
        let downstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let downstream_address = downstream.local_addr().unwrap();
        let client_endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let connector = tokio::spawn(IrohCoordinatorConnector::serve_with_endpoint(
            ticket,
            downstream,
            client_endpoint,
        ));

        tokio::time::timeout(TEST_TIMEOUT, async {
            let mut clients = JoinSet::new();
            for _ in 0..VALID_CONNECTIONS {
                clients.spawn(async move {
                    let mut stream = TcpStream::connect(downstream_address).await.unwrap();
                    stream.write_all(b"ping").await.unwrap();
                    let mut response = [0; 4];
                    stream.read_exact(&mut response).await.unwrap();
                    assert_eq!(&response, b"pong");
                    stream.shutdown().await.unwrap();
                });
            }
            while let Some(result) = clients.join_next().await {
                result.unwrap();
            }
        })
        .await
        .expect("iroh loopback forwarding timed out");

        tokio::time::timeout(TEST_TIMEOUT, origin_task)
            .await
            .expect("origin task timed out")
            .unwrap();
        connector.abort();
        let _ = connector.await;
        tokio::time::timeout(TEST_TIMEOUT, server.shutdown())
            .await
            .expect("iroh server shutdown timed out")
            .unwrap();
    }
}
