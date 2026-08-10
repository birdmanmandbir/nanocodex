//! Durable Iroh identities, admission, and bilateral peer connectivity.

use std::{
    collections::HashMap,
    fmt, fs,
    io::Write as _,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use constant_time_eq::constant_time_eq_32;
use iroh::{
    Endpoint, EndpointAddr,
    endpoint::presets,
    protocol::{AcceptError, ProtocolHandler, Router},
};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::JoinSet,
};
use tokio_util::task::AbortOnDropHandle;

const HUB_ALPN: &[u8] = b"nanocodex-network/hub/1";
const NODE_ALPN: &[u8] = b"nanocodex-network/node/1";
const TICKET_PREFIX: &str = "nanocodex-net:";
const TICKET_VERSION: u8 = 1;
const CONTROL_VERSION: u8 = 1;
const IDENTITY_VERSION: u8 = 1;
const MAX_IDENTITY_BYTES: u64 = 4 * 1024;
const MAX_TICKET_BYTES: usize = 16 * 1024;
const MAX_CONTROL_BYTES: usize = 16 * 1024;
const MAX_PENDING_GRANTS: usize = 128;
const TOKEN_BYTES: usize = 32;
const NONCE_BYTES: usize = 32;
const ONLINE_TIMEOUT: Duration = Duration::from_secs(60);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONCURRENT_STREAMS: usize = 32;
const STREAM_CONTROL: u8 = 1;
const STREAM_FORWARD: u8 = 2;

#[cfg(test)]
static TEST_ENDPOINT_PERMIT: Semaphore = Semaphore::const_new(1);

/// Durable hub identity and shared bootstrap admission capability.
///
/// The private key and bearer token are persisted at a caller-selected path.
/// Treat that file as authority over the network.
#[derive(Clone)]
pub struct JoinAuthority {
    secret_key: iroh::SecretKey,
    token: [u8; TOKEN_BYTES],
}

/// Durable cryptographic identity for one Iroh node endpoint.
#[derive(Clone)]
pub struct NodeIdentity {
    secret_key: iroh::SecretKey,
}

/// One shared bootstrap capability for reaching a hub over Iroh.
///
/// The ticket contains the hub's authenticated Iroh address and a
/// shared bearer capability. Treat its string representation as a secret.
#[derive(Clone)]
pub struct JoinTicket {
    address: EndpointAddr,
    token: [u8; TOKEN_BYTES],
    encoded: String,
}

/// Running Iroh rendezvous and admission endpoint.
pub struct Hub {
    router: Router,
    nodes: Arc<NodeRegistry>,
    tcp_target: Arc<Mutex<Option<SocketAddr>>>,
}

/// One durable node joined to a network.
pub struct Node {
    router: Router,
    dialer: HubDialer,
}

/// Optional bounded adapter between loopback TCP and network streams.
pub struct TcpBridge;

#[derive(Clone)]
struct HubDialer {
    endpoint: Endpoint,
    ticket: Arc<JoinTicket>,
    grants: Arc<Mutex<Vec<DirectGrant>>>,
    connection: Arc<Mutex<Option<ActiveHubConnection>>>,
}

struct ActiveHubConnection {
    connection: iroh::endpoint::Connection,
    _control_task: AbortOnDropHandle<()>,
}

#[derive(Default)]
struct NodeRegistry {
    peers: Mutex<HashMap<iroh::EndpointId, Arc<RegisteredNode>>>,
    generation: AtomicU64,
}

struct RegisteredNode {
    generation: u64,
    address: EndpointAddr,
    control: Mutex<ControlStreams>,
}

struct ControlStreams {
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
}

#[derive(Clone, Copy)]
struct DirectGrant {
    token: [u8; TOKEN_BYTES],
    requester: iroh::EndpointId,
}

/// Identity, admission, Iroh endpoint, or forwarding failure.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    /// The supplied ticket is malformed or uses an unsupported version.
    #[error("invalid network join ticket: {0}")]
    InvalidTicket(String),
    /// A bridge was asked to expose or target a non-loopback TCP address.
    #[error("invalid network loopback bridge address: {0}")]
    InvalidLoopback(SocketAddr),
    /// Creating or shutting down an Iroh endpoint failed.
    #[error("network Iroh endpoint failed: {0}")]
    Endpoint(String),
    /// Durable identity I/O failed.
    #[error("failed to {operation} network identity at {}: {source}", path.display())]
    IdentityIo {
        /// Identity operation that failed.
        operation: &'static str,
        /// Durable identity path.
        path: PathBuf,
        /// Underlying filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// The durable identity is malformed or unsupported.
    #[error("invalid network identity at {}: {message}", path.display())]
    InvalidIdentity {
        /// Durable identity path.
        path: PathBuf,
        /// Bounded validation diagnostic.
        message: String,
    },
    /// A bilateral operation targeted a node that is not currently registered.
    #[error("Iroh node {0} is not connected to the hub")]
    NodeNotConnected(iroh::EndpointId),
    /// A direct bilateral session requires two distinct node identities.
    #[error("a bilateral Iroh session requires two distinct nodes")]
    SameNode,
    /// An authenticated Iroh stream could not be established or forwarded.
    #[error("network protocol failed: {0}")]
    Protocol(String),
    /// Loopback forwarding failed.
    #[error("network loopback forwarding failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Serialize, Deserialize)]
struct WireTicket {
    version: u8,
    address: EndpointAddr,
    token: [u8; TOKEN_BYTES],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireIdentity {
    version: u8,
    secret_key: [u8; 32],
    token: [u8; TOKEN_BYTES],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireNodeIdentity {
    version: u8,
    secret_key: [u8; 32],
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ControlMessage {
    Hello {
        version: u8,
        address: EndpointAddr,
    },
    Challenge {
        nonce: [u8; NONCE_BYTES],
    },
    ChallengeResponse {
        nonce: [u8; NONCE_BYTES],
    },
    Ready,
    Grant {
        token: [u8; TOKEN_BYTES],
        requester: iroh::EndpointId,
    },
    Granted,
    Revoke {
        token: [u8; TOKEN_BYTES],
    },
    Revoked,
    Dial {
        address: EndpointAddr,
        token: [u8; TOKEN_BYTES],
        nonce: [u8; NONCE_BYTES],
    },
    Dialed {
        nonce: [u8; NONCE_BYTES],
    },
    Rejected {
        message: String,
    },
}

#[derive(Serialize, Deserialize)]
struct DirectRequest {
    token: [u8; TOKEN_BYTES],
    nonce: [u8; NONCE_BYTES],
}

#[derive(Serialize, Deserialize)]
struct DirectResponse {
    nonce: [u8; NONCE_BYTES],
}

#[derive(Clone)]
struct HubProtocol {
    token: [u8; TOKEN_BYTES],
    streams: Arc<Semaphore>,
    nodes: Arc<NodeRegistry>,
    tcp_target: Arc<Mutex<Option<SocketAddr>>>,
}

#[derive(Clone)]
struct NodeProtocol {
    grants: Arc<Mutex<Vec<DirectGrant>>>,
}

impl fmt::Debug for HubProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HubProtocol")
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for NodeProtocol {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeProtocol")
            .finish_non_exhaustive()
    }
}

impl JoinAuthority {
    /// Loads or atomically creates authority at the caller-owned file path.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, NetworkError> {
        let (directory, path) = identity_paths(path.as_ref());
        if let Some(encoded) = read_identity(&path)? {
            let identity = Self::decode(&path, &encoded)?;
            secure_identity_file(&path)?;
            return Ok(identity);
        }

        ensure_identity_directory(&directory)?;

        let identity = Self::generate()?;
        let encoded = serde_json::to_vec(&WireIdentity {
            version: IDENTITY_VERSION,
            secret_key: identity.secret_key.to_bytes(),
            token: identity.token,
        })
        .map_err(|error| NetworkError::InvalidIdentity {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if persist_identity(&directory, &path, &encoded)? {
            Ok(identity)
        } else {
            let encoded = read_identity(&path)?.ok_or_else(|| NetworkError::InvalidIdentity {
                path: path.clone(),
                message: "identity disappeared during concurrent creation".to_owned(),
            })?;
            Self::decode(&path, &encoded)
        }
    }

    /// Returns the stable public identity authenticated by Iroh.
    #[must_use]
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.secret_key.public()
    }

    fn generate() -> Result<Self, NetworkError> {
        let mut secret_key = [0; 32];
        let mut token = [0; TOKEN_BYTES];
        getrandom::fill(&mut secret_key)
            .and_then(|()| getrandom::fill(&mut token))
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        Ok(Self {
            secret_key: iroh::SecretKey::from_bytes(&secret_key),
            token,
        })
    }

    fn decode(path: &Path, encoded: &[u8]) -> Result<Self, NetworkError> {
        let WireIdentity {
            version,
            secret_key,
            token,
        } = serde_json::from_slice(encoded).map_err(|error| NetworkError::InvalidIdentity {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if version != IDENTITY_VERSION {
            return Err(NetworkError::InvalidIdentity {
                path: path.to_path_buf(),
                message: format!("unsupported identity version {version}"),
            });
        }
        Ok(Self {
            secret_key: iroh::SecretKey::from_bytes(&secret_key),
            token,
        })
    }
}

impl NodeIdentity {
    /// Loads or atomically creates one node identity at the caller-owned file path.
    pub fn load_or_create(path: impl AsRef<Path>) -> Result<Self, NetworkError> {
        let (directory, path) = identity_paths(path.as_ref());
        if let Some(encoded) = read_identity(&path)? {
            let identity = Self::decode(&path, &encoded)?;
            secure_identity_file(&path)?;
            return Ok(identity);
        }

        ensure_identity_directory(&directory)?;

        let mut secret_key = [0; 32];
        getrandom::fill(&mut secret_key)
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        let identity = Self {
            secret_key: iroh::SecretKey::from_bytes(&secret_key),
        };
        let encoded = serde_json::to_vec(&WireNodeIdentity {
            version: IDENTITY_VERSION,
            secret_key,
        })
        .map_err(|error| NetworkError::InvalidIdentity {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if persist_identity(&directory, &path, &encoded)? {
            Ok(identity)
        } else {
            let encoded = read_identity(&path)?.ok_or_else(|| NetworkError::InvalidIdentity {
                path: path.clone(),
                message: "identity disappeared during concurrent creation".to_owned(),
            })?;
            Self::decode(&path, &encoded)
        }
    }

    /// Returns the stable public identity authenticated by Iroh.
    #[must_use]
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.secret_key.public()
    }

    #[cfg(test)]
    fn generate() -> Result<Self, NetworkError> {
        let mut secret_key = [0; 32];
        getrandom::fill(&mut secret_key)
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        Ok(Self {
            secret_key: iroh::SecretKey::from_bytes(&secret_key),
        })
    }

    fn decode(path: &Path, encoded: &[u8]) -> Result<Self, NetworkError> {
        let WireNodeIdentity {
            version,
            secret_key,
        } = serde_json::from_slice(encoded).map_err(|error| NetworkError::InvalidIdentity {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if version != IDENTITY_VERSION {
            return Err(NetworkError::InvalidIdentity {
                path: path.to_path_buf(),
                message: format!("unsupported identity version {version}"),
            });
        }
        Ok(Self {
            secret_key: iroh::SecretKey::from_bytes(&secret_key),
        })
    }
}

impl fmt::Debug for JoinAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinAuthority")
            .field("endpoint_id", &self.secret_key.public())
            .field("secret_key", &"<redacted>")
            .field("token", &"<redacted>")
            .finish()
    }
}

impl fmt::Debug for NodeIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeIdentity")
            .field("endpoint_id", &self.endpoint_id())
            .field("secret_key", &"<redacted>")
            .finish()
    }
}

impl JoinTicket {
    fn from_parts(address: EndpointAddr, token: [u8; TOKEN_BYTES]) -> Result<Self, NetworkError> {
        let payload = serde_json::to_vec(&WireTicket {
            version: TICKET_VERSION,
            address: address.clone(),
            token,
        })
        .map_err(|error| NetworkError::InvalidTicket(error.to_string()))?;
        let encoded = URL_SAFE_NO_PAD.encode(payload);
        Ok(Self {
            address,
            token,
            encoded,
        })
    }
}

impl fmt::Debug for JoinTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("JoinTicket")
            .field("endpoint_id", &self.address.id)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl fmt::Display for JoinTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{TICKET_PREFIX}{}", self.encoded)
    }
}

impl FromStr for JoinTicket {
    type Err = NetworkError;

    fn from_str(ticket: &str) -> Result<Self, Self::Err> {
        let encoded = ticket.strip_prefix(TICKET_PREFIX).ok_or_else(|| {
            NetworkError::InvalidTicket(format!("expected a ticket beginning with {TICKET_PREFIX}"))
        })?;
        if encoded.is_empty() || encoded.len() > MAX_TICKET_BYTES {
            return Err(NetworkError::InvalidTicket(
                "ticket payload has an invalid length".to_owned(),
            ));
        }
        let payload = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|error| NetworkError::InvalidTicket(error.to_string()))?;
        let WireTicket {
            version,
            address,
            token,
        } = serde_json::from_slice(&payload)
            .map_err(|error| NetworkError::InvalidTicket(error.to_string()))?;
        if version != TICKET_VERSION {
            return Err(NetworkError::InvalidTicket(format!(
                "unsupported ticket version {version}"
            )));
        }
        Self::from_parts(address, token)
    }
}

impl Hub {
    /// Starts a public-relay-capable Iroh endpoint with a durable identity.
    ///
    /// The returned ticket contains current routing hints for the persistent
    /// endpoint identity and shared join capability.
    pub async fn bind(identity: &JoinAuthority) -> Result<(Self, JoinTicket), NetworkError> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(identity.secret_key.clone())
            .bind()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        Self::spawn_with_token(endpoint, true, identity.token).await
    }

    /// Starts a hub from an application-configured endpoint.
    #[doc(hidden)]
    pub async fn bind_with_endpoint(
        endpoint: Endpoint,
        wait_until_online: bool,
    ) -> Result<(Self, JoinTicket), NetworkError> {
        let identity = JoinAuthority::generate()?;
        Self::spawn_with_token(endpoint, wait_until_online, identity.token).await
    }

    async fn spawn_with_token(
        endpoint: Endpoint,
        wait_until_online: bool,
        token: [u8; TOKEN_BYTES],
    ) -> Result<(Self, JoinTicket), NetworkError> {
        let nodes = Arc::new(NodeRegistry::default());
        let tcp_target = Arc::new(Mutex::new(None));
        let protocol = HubProtocol {
            streams: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
            token,
            nodes: nodes.clone(),
            tcp_target: tcp_target.clone(),
        };
        let token = protocol.token;
        let router = Router::builder(endpoint).accept(HUB_ALPN, protocol).spawn();
        if wait_until_online {
            tokio::time::timeout(ONLINE_TIMEOUT, router.endpoint().online())
                .await
                .map_err(|_| {
                    NetworkError::Endpoint(format!(
                        "did not connect to a relay within {} seconds",
                        ONLINE_TIMEOUT.as_secs()
                    ))
                })?;
        }
        let ticket = JoinTicket::from_parts(router.endpoint().addr(), token)?;
        Ok((
            Self {
                router,
                nodes,
                tcp_target,
            },
            ticket,
        ))
    }

    /// Returns the durable endpoint identities currently registered over live connections.
    pub async fn connected_nodes(&self) -> Vec<iroh::EndpointId> {
        let mut nodes = self
            .nodes
            .peers
            .lock()
            .await
            .keys()
            .copied()
            .collect::<Vec<_>>();
        nodes.sort_unstable();
        nodes
    }

    /// Proves a direct, mutually authenticated node-to-node path.
    ///
    /// The hub grants one single-use capability to `provider`, pinned
    /// to `requester`'s Iroh identity. It then asks `requester` to dial the
    /// provider directly and echo a fresh nonce. Session traffic does not pass
    /// through the hub.
    pub async fn prove_direct_path(
        &self,
        requester: iroh::EndpointId,
        provider: iroh::EndpointId,
    ) -> Result<(), NetworkError> {
        if requester == provider {
            return Err(NetworkError::SameNode);
        }
        let (requester_peer, provider_peer) = {
            let nodes = self.nodes.peers.lock().await;
            let requester_peer = nodes
                .get(&requester)
                .cloned()
                .ok_or(NetworkError::NodeNotConnected(requester))?;
            let provider_peer = nodes
                .get(&provider)
                .cloned()
                .ok_or(NetworkError::NodeNotConnected(provider))?;
            (requester_peer, provider_peer)
        };
        let token = random_bytes::<TOKEN_BYTES>()?;
        let nonce = random_bytes::<NONCE_BYTES>()?;
        expect_control(
            &provider_peer,
            &ControlMessage::Grant { token, requester },
            |message| matches!(message, ControlMessage::Granted),
            "node rejected a bilateral session grant",
        )
        .await?;
        let dial = expect_control(
            &requester_peer,
            &ControlMessage::Dial {
                address: provider_peer.address.clone(),
                token,
                nonce,
            },
            |message| matches!(message, ControlMessage::Dialed { nonce: echoed } if *echoed == nonce),
            "node failed a bilateral direct-path challenge",
        )
        .await;
        if dial.is_err() {
            let _ = expect_control(
                &provider_peer,
                &ControlMessage::Revoke { token },
                |message| matches!(message, ControlMessage::Revoked),
                "node failed to revoke an unused bilateral grant",
            )
            .await;
        }
        dial
    }

    /// Gracefully closes the Iroh endpoint and its active sessions.
    pub async fn shutdown(self) -> Result<(), NetworkError> {
        self.router
            .shutdown()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))
    }
}

impl Node {
    /// Joins a network with a public-relay-capable durable Iroh endpoint.
    pub async fn join(ticket: JoinTicket, identity: &NodeIdentity) -> Result<Self, NetworkError> {
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(identity.secret_key.clone())
            .bind()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        Self::join_with_endpoint(ticket, endpoint).await
    }

    /// Joins from an application-configured endpoint.
    #[doc(hidden)]
    pub async fn join_with_endpoint(
        ticket: JoinTicket,
        endpoint: Endpoint,
    ) -> Result<Self, NetworkError> {
        let grants = Arc::new(Mutex::new(Vec::new()));
        let router = Router::builder(endpoint.clone())
            .accept(
                NODE_ALPN,
                NodeProtocol {
                    grants: grants.clone(),
                },
            )
            .spawn();
        let dialer = HubDialer {
            endpoint,
            ticket: Arc::new(ticket),
            grants,
            connection: Arc::new(Mutex::new(None)),
        };
        dialer.ensure_connected().await?;
        Ok(Self { router, dialer })
    }

    /// Returns the node's durable authenticated endpoint identity.
    #[must_use]
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.router.endpoint().id()
    }

    /// Gracefully leaves the network and closes direct sessions.
    pub async fn shutdown(self) -> Result<(), NetworkError> {
        self.router
            .shutdown()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))
    }
}

impl TcpBridge {
    /// Publishes one fixed loopback TCP service through a hub.
    pub async fn publish(hub: &Hub, target: SocketAddr) -> Result<(), NetworkError> {
        require_loopback(target)?;
        let mut configured = hub.tcp_target.lock().await;
        if configured.is_some() {
            return Err(NetworkError::Protocol(
                "the hub already publishes a TCP target".to_owned(),
            ));
        }
        *configured = Some(target);
        Ok(())
    }

    /// Forwards a node-local loopback listener to the hub's published service.
    pub async fn connect(node: &Node, listener: TcpListener) -> Result<(), NetworkError> {
        require_loopback(listener.local_addr()?)?;
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept(), if connections.len() < MAX_CONCURRENT_STREAMS => {
                    let (stream, peer) = accepted?;
                    let dialer = node.dialer.clone();
                    connections.spawn(async move {
                        if let Err(error) = forward_downstream(dialer, stream).await {
                            tracing::warn!(%peer, %error, "Iroh hub downstream closed");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%error, "Iroh hub downstream task failed");
                    }
                }
            }
        }
    }
}

impl HubDialer {
    async fn ensure_connected(&self) -> Result<(), NetworkError> {
        let mut active = self.connection.lock().await;
        if active
            .as_ref()
            .is_some_and(|active| active.connection.close_reason().is_none())
        {
            return Ok(());
        }
        *active = Some(self.establish_connection().await?);
        Ok(())
    }

    async fn establish_connection(&self) -> Result<ActiveHubConnection, NetworkError> {
        let connection = self
            .endpoint
            .connect(self.ticket.address.clone(), HUB_ALPN)
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        write_stream_prefix(&mut send, &self.ticket.token, STREAM_CONTROL).await?;
        write_frame(
            &mut send,
            &ControlMessage::Hello {
                version: CONTROL_VERSION,
                address: self.endpoint.addr(),
            },
        )
        .await?;
        let challenge = read_control(&mut recv).await?;
        let ControlMessage::Challenge { nonce } = challenge else {
            return Err(unexpected_control(
                "hub did not challenge node registration",
                challenge,
            ));
        };
        write_frame(&mut send, &ControlMessage::ChallengeResponse { nonce }).await?;
        let ready = read_control(&mut recv).await?;
        if !matches!(ready, ControlMessage::Ready) {
            return Err(unexpected_control("hub rejected node registration", ready));
        }
        let endpoint = self.endpoint.clone();
        let grants = self.grants.clone();
        let control_task = AbortOnDropHandle::new(tokio::spawn(async move {
            if let Err(error) = node_control_loop(endpoint, grants, send, recv).await {
                tracing::debug!(%error, "Iroh node control channel closed");
            }
        }));
        Ok(ActiveHubConnection {
            connection,
            _control_task: control_task,
        })
    }

    async fn open_bi(
        &self,
    ) -> Result<(iroh::endpoint::SendStream, iroh::endpoint::RecvStream), NetworkError> {
        let mut connection = self.connection.lock().await;
        if let Some(active) = connection.as_ref() {
            match active.connection.open_bi().await {
                Ok(streams) => return Ok(streams),
                Err(error) => {
                    tracing::debug!(%error, "reconnecting closed Iroh hub connection");
                    *connection = None;
                }
            }
        }
        let active = self.establish_connection().await?;
        let streams = active
            .connection
            .open_bi()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        *connection = Some(active);
        Ok(streams)
    }
}

impl ProtocolHandler for HubProtocol {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        let registered = match self.register_node(remote, &connection).await {
            Ok(registered) => registered,
            Err(error) => {
                tracing::warn!(%remote, %error, "Iroh node registration failed");
                return Ok(());
            }
        };
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
                            tracing::warn!(%remote, %error, "Iroh hub upstream closed");
                        }
                    });
                }
                completed = streams.join_next(), if !streams.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%remote, %error, "Iroh hub upstream task failed");
                    }
                }
            }
        }
        streams.abort_all();
        while streams.join_next().await.is_some() {}
        let mut nodes = self.nodes.peers.lock().await;
        if nodes
            .get(&remote)
            .is_some_and(|node| node.generation == registered.generation)
        {
            nodes.remove(&remote);
        }
        Ok(())
    }
}

impl HubProtocol {
    async fn register_node(
        &self,
        remote: iroh::EndpointId,
        connection: &iroh::endpoint::Connection,
    ) -> Result<Arc<RegisteredNode>, NetworkError> {
        let (mut send, mut recv) = tokio::time::timeout(AUTH_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| {
                NetworkError::Protocol(
                    "node did not register before the authentication timeout".to_owned(),
                )
            })?
            .map_err(|error| NetworkError::Protocol(error.to_string()))?;
        read_stream_prefix(&mut recv, &self.token, STREAM_CONTROL).await?;
        let hello = read_control(&mut recv).await?;
        let ControlMessage::Hello { version, address } = hello else {
            return Err(unexpected_control(
                "node did not send a registration hello",
                hello,
            ));
        };
        if version != CONTROL_VERSION {
            return Err(NetworkError::Protocol(format!(
                "node uses unsupported control version {version}"
            )));
        }
        if address.id != remote {
            return Err(NetworkError::Protocol(
                "node registration address does not match its authenticated endpoint identity"
                    .to_owned(),
            ));
        }
        let nonce = random_bytes::<NONCE_BYTES>()?;
        write_frame(&mut send, &ControlMessage::Challenge { nonce }).await?;
        let response = read_control(&mut recv).await?;
        if !matches!(response, ControlMessage::ChallengeResponse { nonce: echoed } if echoed == nonce)
        {
            return Err(unexpected_control(
                "node failed its registration challenge",
                response,
            ));
        }
        write_frame(&mut send, &ControlMessage::Ready).await?;
        let registered = Arc::new(RegisteredNode {
            generation: self.nodes.generation.fetch_add(1, Ordering::Relaxed),
            address,
            control: Mutex::new(ControlStreams { send, recv }),
        });
        self.nodes
            .peers
            .lock()
            .await
            .insert(remote, registered.clone());
        tracing::info!(node_endpoint_id = %remote, "registered durable Iroh node identity");
        Ok(registered)
    }

    async fn forward_upstream(
        &self,
        send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) -> Result<(), NetworkError> {
        read_stream_prefix(&mut recv, &self.token, STREAM_FORWARD).await?;
        let target = self
            .tcp_target
            .lock()
            .await
            .as_ref()
            .copied()
            .ok_or_else(|| {
                NetworkError::Protocol("the hub has no published TCP target".to_owned())
            })?;
        let mut origin = TcpStream::connect(target).await?;
        let mut transport = tokio::io::join(recv, send);
        tokio::io::copy_bidirectional(&mut origin, &mut transport).await?;
        Ok(())
    }
}

async fn forward_downstream(dialer: HubDialer, mut stream: TcpStream) -> Result<(), NetworkError> {
    let (mut send, recv) = dialer.open_bi().await?;
    write_stream_prefix(&mut send, &dialer.ticket.token, STREAM_FORWARD).await?;
    let mut transport = tokio::io::join(recv, send);
    tokio::io::copy_bidirectional(&mut stream, &mut transport).await?;
    Ok(())
}

impl ProtocolHandler for NodeProtocol {
    async fn accept(&self, connection: iroh::endpoint::Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        if let Err(error) = self.accept_direct(remote, connection).await {
            tracing::warn!(%remote, %error, "Iroh bilateral node session failed");
        }
        Ok(())
    }
}

impl NodeProtocol {
    async fn accept_direct(
        &self,
        remote: iroh::EndpointId,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), NetworkError> {
        let (mut send, mut recv) = tokio::time::timeout(AUTH_TIMEOUT, connection.accept_bi())
            .await
            .map_err(|_| {
                NetworkError::Protocol(
                    "bilateral peer did not open a stream before the authentication timeout"
                        .to_owned(),
                )
            })?
            .map_err(|error| NetworkError::Protocol(error.to_string()))?;
        let DirectRequest { token, nonce } = read_frame(&mut recv).await?;
        let mut grants = self.grants.lock().await;
        let Some(index) = grants.iter().position(|grant| {
            grant.requester == remote && constant_time_eq_32(&grant.token, &token)
        }) else {
            return Err(NetworkError::Protocol(
                "bilateral peer presented no matching identity-bound grant".to_owned(),
            ));
        };
        grants.swap_remove(index);
        drop(grants);
        write_frame(&mut send, &DirectResponse { nonce }).await?;
        send.finish()
            .map_err(|error| NetworkError::Protocol(error.to_string()))?;
        send.stopped()
            .await
            .map_err(|error| NetworkError::Protocol(error.to_string()))?;
        Ok(())
    }
}

async fn node_control_loop(
    endpoint: Endpoint,
    grants: Arc<Mutex<Vec<DirectGrant>>>,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) -> Result<(), NetworkError> {
    loop {
        let message = read_control(&mut recv).await?;
        let response = match message {
            ControlMessage::Grant { token, requester } => {
                let mut grants = grants.lock().await;
                if grants.len() >= MAX_PENDING_GRANTS {
                    ControlMessage::Rejected {
                        message: "node has too many pending bilateral grants".to_owned(),
                    }
                } else {
                    grants.push(DirectGrant { token, requester });
                    ControlMessage::Granted
                }
            }
            ControlMessage::Revoke { token } => {
                let mut grants = grants.lock().await;
                grants.retain(|grant| !constant_time_eq_32(&grant.token, &token));
                ControlMessage::Revoked
            }
            ControlMessage::Dial {
                address,
                token,
                nonce,
            } => match direct_ping(&endpoint, address, token, nonce).await {
                Ok(()) => ControlMessage::Dialed { nonce },
                Err(error) => ControlMessage::Rejected {
                    message: error.to_string(),
                },
            },
            other => ControlMessage::Rejected {
                message: format!("unexpected hub control message {}", control_name(&other)),
            },
        };
        write_frame(&mut send, &response).await?;
    }
}

async fn direct_ping(
    endpoint: &Endpoint,
    address: EndpointAddr,
    token: [u8; TOKEN_BYTES],
    nonce: [u8; NONCE_BYTES],
) -> Result<(), NetworkError> {
    let connection = endpoint
        .connect(address, NODE_ALPN)
        .await
        .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
    write_frame(&mut send, &DirectRequest { token, nonce }).await?;
    send.finish()
        .map_err(|error| NetworkError::Protocol(error.to_string()))?;
    let response: DirectResponse = read_frame(&mut recv).await?;
    recv.read_to_end(0)
        .await
        .map_err(|error| NetworkError::Protocol(error.to_string()))?;
    if response.nonce != nonce {
        return Err(NetworkError::Protocol(
            "bilateral peer returned the wrong challenge nonce".to_owned(),
        ));
    }
    Ok(())
}

async fn expect_control(
    node: &RegisteredNode,
    request: &ControlMessage,
    accepts: impl FnOnce(&ControlMessage) -> bool,
    failure: &'static str,
) -> Result<(), NetworkError> {
    let mut control = node.control.lock().await;
    tokio::time::timeout(AUTH_TIMEOUT, write_frame(&mut control.send, request))
        .await
        .map_err(|_| NetworkError::Protocol(format!("{failure}: send timed out")))??;
    let response = tokio::time::timeout(AUTH_TIMEOUT, read_frame(&mut control.recv))
        .await
        .map_err(|_| NetworkError::Protocol(format!("{failure}: response timed out")))??;
    if accepts(&response) {
        Ok(())
    } else {
        Err(unexpected_control(failure, response))
    }
}

async fn read_control(
    recv: &mut iroh::endpoint::RecvStream,
) -> Result<ControlMessage, NetworkError> {
    tokio::time::timeout(AUTH_TIMEOUT, read_frame(recv))
        .await
        .map_err(|_| NetworkError::Protocol("control response timed out".to_owned()))?
}

async fn write_stream_prefix(
    send: &mut iroh::endpoint::SendStream,
    token: &[u8; TOKEN_BYTES],
    kind: u8,
) -> Result<(), NetworkError> {
    send.write_all(token)
        .await
        .map_err(|error| NetworkError::Protocol(error.to_string()))?;
    send.write_all(&[kind])
        .await
        .map_err(|error| NetworkError::Protocol(error.to_string()))
}

async fn read_stream_prefix(
    recv: &mut iroh::endpoint::RecvStream,
    token: &[u8; TOKEN_BYTES],
    expected_kind: u8,
) -> Result<(), NetworkError> {
    let mut presented = [0; TOKEN_BYTES];
    let mut kind = [0; 1];
    tokio::time::timeout(AUTH_TIMEOUT, async {
        recv.read_exact(&mut presented).await?;
        recv.read_exact(&mut kind).await
    })
    .await
    .map_err(|_| {
        NetworkError::Protocol(format!(
            "capability was not received within {} seconds",
            AUTH_TIMEOUT.as_secs()
        ))
    })?
    .map_err(|error| NetworkError::Protocol(error.to_string()))?;
    if !constant_time_eq_32(&presented, token) {
        return Err(NetworkError::Protocol(
            "remote endpoint presented an invalid capability".to_owned(),
        ));
    }
    if kind[0] != expected_kind {
        return Err(NetworkError::Protocol(
            "remote endpoint opened an unexpected stream kind".to_owned(),
        ));
    }
    Ok(())
}

async fn write_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> Result<(), NetworkError> {
    let encoded =
        serde_json::to_vec(value).map_err(|error| NetworkError::Protocol(error.to_string()))?;
    if encoded.len() > MAX_CONTROL_BYTES {
        return Err(NetworkError::Protocol(
            "Iroh control frame exceeds the encoded size limit".to_owned(),
        ));
    }
    writer
        .write_all(&(encoded.len() as u32).to_be_bytes())
        .await
        .map_err(|error| NetworkError::Protocol(error.to_string()))?;
    writer
        .write_all(&encoded)
        .await
        .map_err(|error| NetworkError::Protocol(error.to_string()))
}

async fn read_frame<T: serde::de::DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<T, NetworkError> {
    let mut length = [0; 4];
    reader
        .read_exact(&mut length)
        .await
        .map_err(|error| NetworkError::Protocol(error.to_string()))?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_CONTROL_BYTES {
        return Err(NetworkError::Protocol(
            "Iroh control frame has an invalid encoded length".to_owned(),
        ));
    }
    let mut encoded = vec![0; length];
    reader
        .read_exact(&mut encoded)
        .await
        .map_err(|error| NetworkError::Protocol(error.to_string()))?;
    serde_json::from_slice(&encoded).map_err(|error| NetworkError::Protocol(error.to_string()))
}

fn unexpected_control(context: &str, message: ControlMessage) -> NetworkError {
    if let ControlMessage::Rejected { message } = message {
        NetworkError::Protocol(format!("{context}: {message}"))
    } else {
        NetworkError::Protocol(format!("{context}: received {}", control_name(&message)))
    }
}

const fn control_name(message: &ControlMessage) -> &'static str {
    match message {
        ControlMessage::Hello { .. } => "hello",
        ControlMessage::Challenge { .. } => "challenge",
        ControlMessage::ChallengeResponse { .. } => "challenge_response",
        ControlMessage::Ready => "ready",
        ControlMessage::Grant { .. } => "grant",
        ControlMessage::Granted => "granted",
        ControlMessage::Revoke { .. } => "revoke",
        ControlMessage::Revoked => "revoked",
        ControlMessage::Dial { .. } => "dial",
        ControlMessage::Dialed { .. } => "dialed",
        ControlMessage::Rejected { .. } => "rejected",
    }
}

fn random_bytes<const N: usize>() -> Result<[u8; N], NetworkError> {
    let mut bytes = [0; N];
    getrandom::fill(&mut bytes).map_err(|error| NetworkError::Endpoint(error.to_string()))?;
    Ok(bytes)
}

const fn require_loopback(address: SocketAddr) -> Result<(), NetworkError> {
    if address.ip().is_loopback() {
        Ok(())
    } else {
        Err(NetworkError::InvalidLoopback(address))
    }
}

fn identity_paths(path: &Path) -> (PathBuf, PathBuf) {
    let path = path.to_path_buf();
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    (directory, path)
}

fn ensure_identity_directory(path: &Path) -> Result<(), NetworkError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(NetworkError::InvalidIdentity {
                path: path.to_path_buf(),
                message: "identity parent is not a directory".to_owned(),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(NetworkError::IdentityIo {
                operation: "inspect the identity directory",
                path: path.to_path_buf(),
                source,
            });
        }
    }
    fs::create_dir_all(path).map_err(|source| NetworkError::IdentityIo {
        operation: "create the identity directory",
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = fs::symlink_metadata(path).map_err(|source| NetworkError::IdentityIo {
        operation: "inspect the created identity directory",
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.file_type().is_dir() {
        return Err(NetworkError::InvalidIdentity {
            path: path.to_path_buf(),
            message: "created identity parent is not a directory".to_owned(),
        });
    }
    secure_identity_directory(path)
}

fn read_identity(path: &Path) -> Result<Option<Vec<u8>>, NetworkError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(source) => {
            return Err(NetworkError::IdentityIo {
                operation: "inspect the identity",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    if !metadata.file_type().is_file() {
        return Err(NetworkError::InvalidIdentity {
            path: path.to_path_buf(),
            message: "identity is not a regular file".to_owned(),
        });
    }
    if metadata.len() > MAX_IDENTITY_BYTES {
        return Err(NetworkError::InvalidIdentity {
            path: path.to_path_buf(),
            message: "identity exceeds the maximum encoded size".to_owned(),
        });
    }
    fs::read(path)
        .map(Some)
        .map_err(|source| NetworkError::IdentityIo {
            operation: "read the identity",
            path: path.to_path_buf(),
            source,
        })
}

fn persist_identity(directory: &Path, path: &Path, encoded: &[u8]) -> Result<bool, NetworkError> {
    let mut staged =
        tempfile::NamedTempFile::new_in(directory).map_err(|source| NetworkError::IdentityIo {
            operation: "stage the identity",
            path: path.to_path_buf(),
            source,
        })?;
    staged
        .write_all(encoded)
        .and_then(|()| staged.as_file().sync_all())
        .map_err(|source| NetworkError::IdentityIo {
            operation: "write the identity",
            path: path.to_path_buf(),
            source,
        })?;
    match staged.persist_noclobber(path) {
        Ok(file) => {
            file.sync_all().map_err(|source| NetworkError::IdentityIo {
                operation: "persist the identity",
                path: path.to_path_buf(),
                source,
            })?;
            secure_identity_file(path)?;
            sync_identity_directory(directory)?;
            Ok(true)
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            drop(error.file);
            Ok(false)
        }
        Err(error) => Err(NetworkError::IdentityIo {
            operation: "persist the identity",
            path: path.to_path_buf(),
            source: error.error,
        }),
    }
}

fn secure_identity_directory(path: &Path) -> Result<(), NetworkError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| {
            NetworkError::IdentityIo {
                operation: "secure the identity directory",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn secure_identity_file(path: &Path) -> Result<(), NetworkError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            NetworkError::IdentityIo {
                operation: "secure the identity",
                path: path.to_path_buf(),
                source,
            }
        })?;
    }
    Ok(())
}

fn sync_identity_directory(path: &Path) -> Result<(), NetworkError> {
    #[cfg(unix)]
    {
        fs::File::open(path)
            .and_then(|directory| directory.sync_all())
            .map_err(|source| NetworkError::IdentityIo {
                operation: "sync the identity directory",
                path: path.to_path_buf(),
                source,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    fn authority_path(directory: &Path) -> PathBuf {
        directory.join("authority.json")
    }

    fn node_path(directory: &Path) -> PathBuf {
        directory.join("node.json")
    }

    #[test]
    fn durable_identity_is_restart_stable_and_redacted() {
        let state = tempfile::tempdir().unwrap();
        let path = authority_path(state.path());
        let first = JoinAuthority::load_or_create(&path).unwrap();
        let second = JoinAuthority::load_or_create(&path).unwrap();

        assert_eq!(first.secret_key.to_bytes(), second.secret_key.to_bytes());
        assert_eq!(first.secret_key.public(), second.secret_key.public());
        assert_eq!(first.token, second.token);
        let debug = format!("{first:?}");
        assert!(debug.contains(&first.secret_key.public().to_string()));
        assert!(!debug.contains(&hex::encode(first.secret_key.to_bytes())));
        assert!(!debug.contains(&hex::encode(first.token)));

        let node_path = node_path(state.path());
        let node = NodeIdentity::load_or_create(&node_path).unwrap();
        let reloaded_node = NodeIdentity::load_or_create(&node_path).unwrap();
        assert_eq!(node.endpoint_id(), reloaded_node.endpoint_id());
        assert_ne!(node.endpoint_id(), first.endpoint_id());
        assert!(!format!("{node:?}").contains(&hex::encode(node.secret_key.to_bytes())));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            let directory = state.path();
            let original_directory_mode =
                fs::metadata(directory).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&node_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
            JoinAuthority::load_or_create(&path).unwrap();
            assert_eq!(
                fs::metadata(directory).unwrap().permissions().mode() & 0o777,
                0o755
            );
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::set_permissions(
                directory,
                fs::Permissions::from_mode(original_directory_mode),
            )
            .unwrap();

            let nested_path = state.path().join("new-identities/authority.json");
            JoinAuthority::load_or_create(&nested_path).unwrap();
            assert_eq!(
                fs::metadata(nested_path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn concurrent_identity_creation_converges_on_one_authority() {
        const CREATORS: usize = 8;

        let state = tempfile::tempdir().unwrap();
        let state_path = Arc::new(state.path().to_path_buf());
        let barrier = Arc::new(Barrier::new(CREATORS));
        let creators = (0..CREATORS)
            .map(|_| {
                let state_path = state_path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    JoinAuthority::load_or_create(authority_path(state_path.as_path())).unwrap()
                })
            })
            .collect::<Vec<_>>();
        let identities = creators
            .into_iter()
            .map(|creator| creator.join().unwrap())
            .collect::<Vec<_>>();

        for identity in &identities[1..] {
            assert_eq!(
                identity.secret_key.to_bytes(),
                identities[0].secret_key.to_bytes()
            );
            assert_eq!(identity.token, identities[0].token);
        }
    }

    #[test]
    fn malformed_or_unsupported_identity_is_never_replaced() {
        let state = tempfile::tempdir().unwrap();
        let path = authority_path(state.path());
        fs::write(&path, b"not-json").unwrap();
        assert!(matches!(
            JoinAuthority::load_or_create(&path),
            Err(NetworkError::InvalidIdentity { .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"not-json");

        fs::write(
            &path,
            serde_json::to_vec(&WireIdentity {
                version: IDENTITY_VERSION + 1,
                secret_key: [7; 32],
                token: [9; TOKEN_BYTES],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(matches!(
            JoinAuthority::load_or_create(&path),
            Err(NetworkError::InvalidIdentity { message, .. })
                if message == format!("unsupported identity version {}", IDENTITY_VERSION + 1)
        ));
    }

    #[test]
    fn unsafe_identity_paths_are_rejected() {
        let state = tempfile::tempdir().unwrap();
        let path = authority_path(state.path());
        fs::create_dir_all(&path).unwrap();

        assert!(matches!(
            JoinAuthority::load_or_create(&path),
            Err(NetworkError::InvalidIdentity { message, .. })
                if message == "identity is not a regular file"
        ));

        fs::remove_dir(&path).unwrap();
        fs::write(&path, vec![0; MAX_IDENTITY_BYTES as usize + 1]).unwrap();
        assert!(matches!(
            JoinAuthority::load_or_create(&path),
            Err(NetworkError::InvalidIdentity { message, .. })
                if message == "identity exceeds the maximum encoded size"
        ));

        let parent = state.path().join("not-a-directory");
        fs::write(&parent, b"caller data").unwrap();
        assert!(matches!(
            NodeIdentity::load_or_create(parent.join("node.json")),
            Err(NetworkError::InvalidIdentity { message, .. })
                if message == "identity parent is not a directory"
        ));
        assert_eq!(fs::read(parent).unwrap(), b"caller data");
    }

    #[tokio::test]
    async fn restarted_endpoint_keeps_identity_token_and_connectivity() {
        let _test_permit = TEST_ENDPOINT_PERMIT.acquire().await.unwrap();
        let state = tempfile::tempdir().unwrap();
        let path = authority_path(state.path());
        let identity = JoinAuthority::load_or_create(&path).unwrap();
        let origin = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin_address = origin.local_addr().unwrap();

        let first_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let (first_server, first_ticket) =
            Hub::spawn_with_token(first_endpoint, false, identity.token)
                .await
                .unwrap();
        TcpBridge::publish(&first_server, origin_address)
            .await
            .unwrap();
        first_server.shutdown().await.unwrap();

        let reloaded = JoinAuthority::load_or_create(&path).unwrap();
        let second_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(reloaded.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let (second_server, second_ticket) =
            Hub::spawn_with_token(second_endpoint, false, reloaded.token)
                .await
                .unwrap();
        TcpBridge::publish(&second_server, origin_address)
            .await
            .unwrap();
        assert_eq!(first_ticket.address.id, second_ticket.address.id);
        assert_eq!(first_ticket.token, second_ticket.token);

        let origin_task = tokio::spawn(async move {
            let (mut stream, _) = origin.accept().await.unwrap();
            let mut request = [0; 4];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            stream.write_all(b"pong").await.unwrap();
        });
        let client = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let dialer = HubDialer {
            endpoint: client.clone(),
            ticket: Arc::new(second_ticket),
            grants: Arc::new(Mutex::new(Vec::new())),
            connection: Arc::new(Mutex::new(None)),
        };
        let (mut send, mut recv) = dialer.open_bi().await.unwrap();
        write_stream_prefix(&mut send, &dialer.ticket.token, STREAM_FORWARD)
            .await
            .unwrap();
        send.write_all(b"ping").await.unwrap();
        send.finish().unwrap();
        assert_eq!(recv.read_to_end(4).await.unwrap(), b"pong");
        origin_task.await.unwrap();
        client.close().await;
        second_server.shutdown().await.unwrap();
    }

    #[test]
    fn tickets_round_trip_without_exposing_the_capability_in_debug() {
        let secret = iroh::SecretKey::from_bytes(&[7; 32]);
        let ticket =
            JoinTicket::from_parts(EndpointAddr::new(secret.public()), [9; TOKEN_BYTES]).unwrap();
        let encoded = ticket.to_string();
        let decoded = JoinTicket::from_str(&encoded).unwrap();

        assert_eq!(decoded.address, ticket.address);
        assert_eq!(decoded.token, ticket.token);
        assert!(!format!("{ticket:?}").contains(&encoded));
        for malformed in [
            "",
            "https://example.com",
            TICKET_PREFIX,
            "nanocodex-net:not-base64!",
            "nanocodex-net:e30",
        ] {
            assert!(
                JoinTicket::from_str(malformed).is_err(),
                "malformed ticket unexpectedly parsed: {malformed}"
            );
        }
        let oversized = format!("{TICKET_PREFIX}{}", "a".repeat(MAX_TICKET_BYTES + 1));
        assert!(JoinTicket::from_str(&oversized).is_err());

        let unsupported = URL_SAFE_NO_PAD.encode(
            serde_json::to_vec(&WireTicket {
                version: TICKET_VERSION + 1,
                address: ticket.address.clone(),
                token: ticket.token,
            })
            .unwrap(),
        );
        assert!(JoinTicket::from_str(&format!("{TICKET_PREFIX}{unsupported}")).is_err());
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
        let (server, ticket) = Hub::bind_with_endpoint(server_endpoint, false)
            .await
            .unwrap();
        TcpBridge::publish(&server, origin_address).await.unwrap();
        let client_endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let dialer = HubDialer {
            endpoint: client_endpoint,
            ticket: Arc::new(ticket),
            grants: Arc::new(Mutex::new(Vec::new())),
            connection: Arc::new(Mutex::new(None)),
        };

        let mut connection_ids = Vec::new();
        for _ in 0..2 {
            let (mut send, mut recv) = dialer.open_bi().await.unwrap();
            connection_ids.push(
                dialer
                    .connection
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .connection
                    .stable_id(),
            );
            write_stream_prefix(&mut send, &dialer.ticket.token, STREAM_FORWARD)
                .await
                .unwrap();
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
    async fn hub_arranges_direct_identity_bound_sessions_between_registered_nodes() {
        const TEST_TIMEOUT: Duration = Duration::from_secs(5);

        let _test_permit = TEST_ENDPOINT_PERMIT.acquire().await.unwrap();
        let server_endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let (hub, ticket) = Hub::bind_with_endpoint(server_endpoint, false)
            .await
            .unwrap();

        let first_identity = NodeIdentity::generate().unwrap();
        let first_id = first_identity.endpoint_id();
        let first_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(first_identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let first = Node::join_with_endpoint(ticket.clone(), first_endpoint)
            .await
            .unwrap();

        let second_identity = NodeIdentity::generate().unwrap();
        let second_id = second_identity.endpoint_id();
        let second_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(second_identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let second = Node::join_with_endpoint(ticket, second_endpoint)
            .await
            .unwrap();

        tokio::time::timeout(TEST_TIMEOUT, async {
            while hub.connected_nodes().await.len() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("nodes did not register with the hub");
        let mut expected = vec![first_id, second_id];
        expected.sort_unstable();
        assert_eq!(hub.connected_nodes().await, expected);

        tokio::time::timeout(TEST_TIMEOUT, hub.prove_direct_path(first_id, second_id))
            .await
            .expect("first bilateral session timed out")
            .unwrap();
        tokio::time::timeout(TEST_TIMEOUT, hub.prove_direct_path(second_id, first_id))
            .await
            .expect("reverse bilateral session timed out")
            .unwrap();
        assert!(matches!(
            hub.prove_direct_path(first_id, first_id).await,
            Err(NetworkError::SameNode)
        ));

        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
        tokio::time::timeout(TEST_TIMEOUT, hub.shutdown())
            .await
            .expect("Iroh hub shutdown timed out")
            .unwrap();
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
        let (server, ticket) = Hub::bind_with_endpoint(server_endpoint, false)
            .await
            .unwrap();
        TcpBridge::publish(&server, origin_address).await.unwrap();

        let invalid_client = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .bind()
            .await
            .unwrap();
        let invalid_connection = invalid_client
            .connect(ticket.address.clone(), HUB_ALPN)
            .await
            .unwrap();
        let (mut invalid_send, mut invalid_recv) = invalid_connection.open_bi().await.unwrap();
        let mut invalid_token = ticket.token;
        invalid_token[0] ^= 1;
        write_stream_prefix(&mut invalid_send, &invalid_token, STREAM_CONTROL)
            .await
            .unwrap();
        invalid_send.finish().unwrap();
        let _ = tokio::time::timeout(TEST_TIMEOUT, invalid_recv.read_to_end(1))
            .await
            .expect("invalid capability stream did not close");
        assert!(
            tokio::time::timeout(Duration::from_millis(250), origin.accept())
                .await
                .is_err(),
            "an invalid capability reached the hub origin"
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
        let node = Node::join_with_endpoint(ticket, client_endpoint)
            .await
            .unwrap();
        let connector = tokio::spawn(async move { TcpBridge::connect(&node, downstream).await });

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
        .expect("Iroh loopback forwarding timed out");

        tokio::time::timeout(TEST_TIMEOUT, origin_task)
            .await
            .expect("origin task timed out")
            .unwrap();
        connector.abort();
        let _ = connector.await;
        tokio::time::timeout(TEST_TIMEOUT, server.shutdown())
            .await
            .expect("Iroh server shutdown timed out")
            .unwrap();
    }
}
