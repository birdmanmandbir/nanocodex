//! Durable Iroh identities, admission, and bilateral peer connectivity.

mod discovery;

pub use discovery::{
    CapabilityValue, NodeAdvertisement, PeerChange, PeerWatcher, Query, SignedAdvertisement,
};

use std::{
    collections::HashMap,
    fmt, fs,
    io::Write as _,
    net::SocketAddr,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll},
    time::{Duration, Instant},
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
    io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore, mpsc},
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
const MAX_PENDING_SESSIONS: usize = 32;
const MAX_PROTOCOL_BYTES: usize = 128;
const TOKEN_BYTES: usize = 32;
const NONCE_BYTES: usize = 32;
const ONLINE_TIMEOUT: Duration = Duration::from_secs(60);
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
const GRANT_LIFETIME: Duration = Duration::from_secs(30);
const MAX_CONCURRENT_STREAMS: usize = 32;
const STREAM_CONTROL: u8 = 1;
const STREAM_SESSION_REQUEST: u8 = 2;
const STREAM_ADVERTISEMENT_REQUEST: u8 = 3;
const ADVERTISEMENT_REAP_INTERVAL: Duration = Duration::from_millis(50);
const TCP_BRIDGE_PROTOCOL: &str = "nanocodex/tcp-bridge/1";
const TCP_TICKET_PREFIX: &str = "nanocodex-tcp:";
const TCP_TICKET_VERSION: u8 = 1;

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

/// Stable application protocol name routed between authenticated peers.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ProtocolId(String);

/// Opaque capability for joining a network and reaching one published TCP bridge.
#[derive(Clone)]
pub struct TcpBridgeTicket {
    network: JoinTicket,
    provider: iroh::EndpointId,
    encoded: String,
}

/// Running Iroh rendezvous and admission endpoint.
pub struct Hub {
    router: Router,
    nodes: Arc<NodeRegistry>,
    _expiry_task: AbortOnDropHandle<()>,
}

/// One durable node joined to a network.
pub struct Node {
    router: Router,
    dialer: HubDialer,
    listeners: Arc<Mutex<HashMap<ProtocolId, mpsc::Sender<PeerStream>>>>,
    cluster_view: Arc<discovery::ClusterView>,
}

/// A live, automatically renewed capability-advertisement lease.
pub struct AdvertisementLease {
    node_id: iroh::EndpointId,
    revision: u64,
    _renewal_task: AbortOnDropHandle<()>,
}

/// Receiver for authenticated streams addressed to one application protocol.
pub struct ProtocolListener {
    protocol: ProtocolId,
    incoming: mpsc::Receiver<PeerStream>,
}

/// One mutually authenticated, protocol-bound peer stream.
pub struct PeerStream {
    peer: iroh::EndpointId,
    protocol: ProtocolId,
    _connection: iroh::endpoint::Connection,
    send: iroh::endpoint::SendStream,
    recv: iroh::endpoint::RecvStream,
}

/// Optional bounded adapter between loopback TCP and network streams.
pub struct TcpBridge;

#[derive(Clone)]
struct HubDialer {
    endpoint: Endpoint,
    ticket: Arc<JoinTicket>,
    grants: Arc<Mutex<Vec<PendingSessionGrant>>>,
    listeners: Arc<Mutex<HashMap<ProtocolId, mpsc::Sender<PeerStream>>>>,
    cluster_view: Arc<discovery::ClusterView>,
    connection: Arc<Mutex<Option<ActiveHubConnection>>>,
}

struct ActiveHubConnection {
    connection: iroh::endpoint::Connection,
    _control_task: AbortOnDropHandle<()>,
}

#[derive(Default)]
struct NodeRegistry {
    peers: Mutex<HashMap<iroh::EndpointId, Arc<RegisteredNode>>>,
    advertisements: Mutex<HashMap<iroh::EndpointId, SignedAdvertisement>>,
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

#[derive(Clone)]
struct PendingSessionGrant {
    token: [u8; TOKEN_BYTES],
    requester: iroh::EndpointId,
    protocol: ProtocolId,
    expires_at: Instant,
}

/// Identity, admission, Iroh endpoint, or forwarding failure.
#[derive(Debug, thiserror::Error)]
pub enum NetworkError {
    /// The supplied ticket is malformed or uses an unsupported version.
    #[error("invalid network join ticket: {0}")]
    InvalidTicket(String),
    /// An application protocol name is empty, too long, or contains unsupported bytes.
    #[error("invalid network protocol: {0}")]
    InvalidProtocol(String),
    /// A capability advertisement is malformed, stale, oversized, or has an invalid signature.
    #[error("invalid network advertisement: {0}")]
    InvalidAdvertisement(String),
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
    /// A peer session requires two distinct node identities.
    #[error("an Iroh peer session requires two distinct nodes")]
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
struct WireTcpBridgeTicket {
    version: u8,
    network: String,
    provider: iroh::EndpointId,
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
        protocol: String,
    },
    Granted,
    AdvertisementChanged {
        kind: AdvertisementChangeKind,
        record: SignedAdvertisement,
    },
    AdvertisementSnapshotReset,
    Rejected {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AdvertisementChangeKind {
    Joined,
    Updated,
    Disconnected,
    Expired,
}

#[derive(Serialize, Deserialize)]
struct AdvertisementPublish {
    record: SignedAdvertisement,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum AdvertisementPublishResponse {
    Accepted,
    Rejected { message: String },
}

#[derive(Serialize, Deserialize)]
struct SessionRequest {
    provider: iroh::EndpointId,
    protocol: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum SessionGrant {
    Granted {
        address: EndpointAddr,
        token: [u8; TOKEN_BYTES],
    },
    Rejected {
        message: String,
    },
}

#[derive(Serialize, Deserialize)]
struct SessionOpen {
    token: [u8; TOKEN_BYTES],
    protocol: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum SessionOpenResponse {
    Accepted,
    Rejected { message: String },
}

#[derive(Clone)]
struct HubProtocol {
    token: [u8; TOKEN_BYTES],
    streams: Arc<Semaphore>,
    nodes: Arc<NodeRegistry>,
}

#[derive(Clone)]
struct NodeProtocol {
    grants: Arc<Mutex<Vec<PendingSessionGrant>>>,
    listeners: Arc<Mutex<HashMap<ProtocolId, mpsc::Sender<PeerStream>>>>,
    streams: Arc<Semaphore>,
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

impl ProtocolId {
    /// Creates a bounded protocol identifier suitable for stable wire routing.
    pub fn new(protocol: impl Into<String>) -> Result<Self, NetworkError> {
        let protocol = protocol.into();
        if protocol.is_empty() {
            return Err(NetworkError::InvalidProtocol(
                "protocol name must not be empty".to_owned(),
            ));
        }
        if protocol.len() > MAX_PROTOCOL_BYTES {
            return Err(NetworkError::InvalidProtocol(format!(
                "protocol name exceeds {MAX_PROTOCOL_BYTES} bytes"
            )));
        }
        if !protocol
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
        {
            return Err(NetworkError::InvalidProtocol(
                "protocol name must use only ASCII letters, digits, '/', '.', '_', or '-'"
                    .to_owned(),
            ));
        }
        Ok(Self(protocol))
    }

    /// Returns the wire-stable protocol name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProtocolId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ProtocolId {
    type Err = NetworkError;

    fn from_str(protocol: &str) -> Result<Self, Self::Err> {
        Self::new(protocol)
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

impl TcpBridgeTicket {
    /// Binds one TCP bridge provider identity to a network admission ticket.
    pub fn new(network: JoinTicket, provider: iroh::EndpointId) -> Result<Self, NetworkError> {
        let payload = serde_json::to_vec(&WireTcpBridgeTicket {
            version: TCP_TICKET_VERSION,
            network: network.to_string(),
            provider,
        })
        .map_err(|error| NetworkError::InvalidTicket(error.to_string()))?;
        Ok(Self {
            network,
            provider,
            encoded: URL_SAFE_NO_PAD.encode(payload),
        })
    }

    /// Returns the topology admission capability carried by this bridge ticket.
    #[must_use]
    pub fn join_ticket(&self) -> JoinTicket {
        self.network.clone()
    }

    /// Returns the durable identity providing the TCP service.
    #[must_use]
    pub const fn provider_id(&self) -> iroh::EndpointId {
        self.provider
    }
}

impl fmt::Debug for TcpBridgeTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TcpBridgeTicket")
            .field("provider_id", &self.provider)
            .field("network", &"<redacted>")
            .finish()
    }
}

impl fmt::Display for TcpBridgeTicket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{TCP_TICKET_PREFIX}{}", self.encoded)
    }
}

impl FromStr for TcpBridgeTicket {
    type Err = NetworkError;

    fn from_str(ticket: &str) -> Result<Self, Self::Err> {
        let encoded = ticket.strip_prefix(TCP_TICKET_PREFIX).ok_or_else(|| {
            NetworkError::InvalidTicket(format!(
                "expected a TCP bridge ticket beginning with {TCP_TICKET_PREFIX}"
            ))
        })?;
        if encoded.is_empty() || encoded.len() > MAX_TICKET_BYTES * 2 {
            return Err(NetworkError::InvalidTicket(
                "TCP bridge ticket payload has an invalid length".to_owned(),
            ));
        }
        let payload = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|error| NetworkError::InvalidTicket(error.to_string()))?;
        let WireTcpBridgeTicket {
            version,
            network,
            provider,
        } = serde_json::from_slice(&payload)
            .map_err(|error| NetworkError::InvalidTicket(error.to_string()))?;
        if version != TCP_TICKET_VERSION {
            return Err(NetworkError::InvalidTicket(format!(
                "unsupported TCP bridge ticket version {version}"
            )));
        }
        Self::new(JoinTicket::from_str(&network)?, provider)
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
        let protocol = HubProtocol {
            streams: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
            token,
            nodes: nodes.clone(),
        };
        let token = protocol.token;
        let router = Router::builder(endpoint).accept(HUB_ALPN, protocol).spawn();
        let expiry_nodes = Arc::downgrade(&nodes);
        let expiry_task = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut interval = tokio::time::interval(ADVERTISEMENT_REAP_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let Some(nodes) = expiry_nodes.upgrade() else {
                    break;
                };
                nodes.expire_advertisements().await;
            }
        }));
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
                _expiry_task: expiry_task,
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

    /// Gracefully closes the Iroh endpoint and its active sessions.
    pub async fn shutdown(self) -> Result<(), NetworkError> {
        self.router
            .shutdown()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))
    }
}

impl NodeRegistry {
    async fn publish(
        &self,
        authenticated_node: iroh::EndpointId,
        record: SignedAdvertisement,
    ) -> Result<(), NetworkError> {
        if !self.peers.lock().await.contains_key(&authenticated_node) {
            return Err(NetworkError::InvalidAdvertisement(
                "publishing node is no longer connected".to_owned(),
            ));
        }
        record.verify(authenticated_node)?;
        let change = {
            let mut advertisements = self.advertisements.lock().await;
            match advertisements.get(&authenticated_node) {
                None => {
                    advertisements.insert(authenticated_node, record.clone());
                    Some(AdvertisementChangeKind::Joined)
                }
                Some(previous)
                    if record.advertisement().revision() < previous.advertisement().revision() =>
                {
                    return Err(NetworkError::InvalidAdvertisement(format!(
                        "revision {} is older than active revision {}",
                        record.advertisement().revision(),
                        previous.advertisement().revision()
                    )));
                }
                Some(previous)
                    if record.advertisement().revision() == previous.advertisement().revision() =>
                {
                    if !record.same_revision_content(previous) {
                        return Err(NetworkError::InvalidAdvertisement(
                            "one revision cannot describe different capability content".to_owned(),
                        ));
                    }
                    if record.expires_at_unix_millis() > previous.expires_at_unix_millis() {
                        advertisements.insert(authenticated_node, record.clone());
                    }
                    None
                }
                Some(_) => {
                    advertisements.insert(authenticated_node, record.clone());
                    Some(AdvertisementChangeKind::Updated)
                }
            }
        };
        if let Some(kind) = change {
            self.broadcast_change(kind, record).await;
        }
        Ok(())
    }

    async fn send_snapshot(&self, node: &RegisteredNode) -> Result<(), NetworkError> {
        let connected = self
            .peers
            .lock()
            .await
            .keys()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        let now = discovery::unix_millis()?;
        let records = self
            .advertisements
            .lock()
            .await
            .values()
            .filter(|record| connected.contains(&record.node_id()) && !record.is_expired_at(now))
            .cloned()
            .collect::<Vec<_>>();
        let mut control = node.control.lock().await;
        write_frame(
            &mut control.send,
            &ControlMessage::AdvertisementSnapshotReset,
        )
        .await?;
        for record in records {
            write_frame(
                &mut control.send,
                &ControlMessage::AdvertisementChanged {
                    kind: AdvertisementChangeKind::Joined,
                    record,
                },
            )
            .await?;
        }
        Ok(())
    }

    async fn node_reconnected(&self, node_id: iroh::EndpointId) {
        let record = self.advertisements.lock().await.get(&node_id).cloned();
        if let Some(record) = record {
            self.broadcast_change(AdvertisementChangeKind::Joined, record)
                .await;
        }
    }

    async fn node_disconnected(&self, node_id: iroh::EndpointId) {
        let record = self.advertisements.lock().await.get(&node_id).cloned();
        if let Some(record) = record {
            self.broadcast_change(AdvertisementChangeKind::Disconnected, record)
                .await;
        }
    }

    async fn expire_advertisements(&self) {
        let now = match discovery::unix_millis() {
            Ok(now) => now,
            Err(error) => {
                tracing::warn!(%error, "could not reap network advertisements");
                return;
            }
        };
        let expired = {
            let mut advertisements = self.advertisements.lock().await;
            let expired = advertisements
                .values()
                .filter(|record| record.is_expired_at(now))
                .cloned()
                .collect::<Vec<_>>();
            for record in &expired {
                advertisements.remove(&record.node_id());
            }
            expired
        };
        for record in expired {
            self.broadcast_change(AdvertisementChangeKind::Expired, record)
                .await;
        }
    }

    async fn broadcast_change(&self, kind: AdvertisementChangeKind, record: SignedAdvertisement) {
        let peers = self
            .peers
            .lock()
            .await
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for peer in peers {
            let mut control = peer.control.lock().await;
            let result = tokio::time::timeout(
                AUTH_TIMEOUT,
                write_frame(
                    &mut control.send,
                    &ControlMessage::AdvertisementChanged {
                        kind,
                        record: record.clone(),
                    },
                ),
            )
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    tracing::debug!(%error, "could not publish cluster-view change");
                }
                Err(_) => {
                    tracing::debug!("cluster-view change timed out");
                }
            }
        }
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
        let listeners = Arc::new(Mutex::new(HashMap::new()));
        let cluster_view = Arc::new(discovery::ClusterView::default());
        let router = Router::builder(endpoint.clone())
            .accept(
                NODE_ALPN,
                NodeProtocol {
                    grants: grants.clone(),
                    listeners: listeners.clone(),
                    streams: Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS)),
                },
            )
            .spawn();
        let dialer = HubDialer {
            endpoint,
            ticket: Arc::new(ticket),
            grants,
            listeners: listeners.clone(),
            cluster_view: cluster_view.clone(),
            connection: Arc::new(Mutex::new(None)),
        };
        dialer.ensure_connected().await?;
        Ok(Self {
            router,
            dialer,
            listeners,
            cluster_view,
        })
    }

    /// Returns the node's durable authenticated endpoint identity.
    #[must_use]
    pub fn endpoint_id(&self) -> iroh::EndpointId {
        self.router.endpoint().id()
    }

    /// Registers one application protocol on this node.
    ///
    /// At most one live listener may own a protocol name at a time. Dropping
    /// the listener closes its queue and permits a replacement listener.
    pub async fn listen(&self, protocol: ProtocolId) -> Result<ProtocolListener, NetworkError> {
        let mut listeners = self.listeners.lock().await;
        if listeners
            .get(&protocol)
            .is_some_and(|listener| !listener.is_closed())
        {
            return Err(NetworkError::Protocol(format!(
                "protocol {protocol} already has a listener"
            )));
        }
        let (incoming, receiver) = mpsc::channel(MAX_PENDING_SESSIONS);
        listeners.insert(protocol.clone(), incoming);
        Ok(ProtocolListener {
            protocol,
            incoming: receiver,
        })
    }

    /// Opens a direct authenticated stream to `peer` for `protocol`.
    ///
    /// The hub authorizes one short-lived, identity- and protocol-bound grant.
    /// Application bytes then flow directly between the two nodes.
    pub async fn connect(
        &self,
        peer: iroh::EndpointId,
        protocol: &ProtocolId,
    ) -> Result<PeerStream, NetworkError> {
        if peer == self.endpoint_id() {
            return Err(NetworkError::SameNode);
        }
        self.dialer.connect_peer(peer, protocol).await
    }

    /// Publishes and automatically renews one signed capability advertisement.
    ///
    /// Dropping the returned lease stops renewal. The record remains visible
    /// until its signed expiry, so observers can distinguish disconnects from
    /// lease expiration.
    pub async fn advertise(
        &self,
        advertisement: NodeAdvertisement,
    ) -> Result<AdvertisementLease, NetworkError> {
        let record = SignedAdvertisement::sign(advertisement, self.router.endpoint().secret_key())?;
        self.dialer.publish_advertisement(record.clone()).await?;
        let node_id = record.node_id();
        let revision = record.advertisement().revision();
        let renewal_delay = record.advertisement().lease_duration() / 2;
        let dialer = self.dialer.clone();
        let advertisement = record.advertisement().clone();
        let secret_key = self.router.endpoint().secret_key().clone();
        let renewal_task = AbortOnDropHandle::new(tokio::spawn(async move {
            let mut interval = tokio::time::interval(renewal_delay);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval.tick().await;
            loop {
                interval.tick().await;
                let record = match SignedAdvertisement::sign(advertisement.clone(), &secret_key) {
                    Ok(record) => record,
                    Err(error) => {
                        tracing::warn!(%error, "could not renew network advertisement");
                        break;
                    }
                };
                if let Err(error) = dialer.publish_advertisement(record).await {
                    tracing::warn!(%error, "network advertisement renewal stopped");
                    break;
                }
            }
        }));
        Ok(AdvertisementLease {
            node_id,
            revision,
            _renewal_task: renewal_task,
        })
    }

    /// Opens a filtered stream over this node's local authenticated cluster view.
    pub async fn watch(&self, query: Query) -> PeerWatcher {
        self.cluster_view.watch(self.endpoint_id(), query).await
    }

    /// Gracefully leaves the network and closes direct sessions.
    pub async fn shutdown(self) -> Result<(), NetworkError> {
        self.router
            .shutdown()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))
    }
}

impl AdvertisementLease {
    /// Returns the durable identity which owns this lease.
    #[must_use]
    pub const fn node_id(&self) -> iroh::EndpointId {
        self.node_id
    }

    /// Returns the caller-owned advertisement revision being renewed.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
}

impl fmt::Debug for AdvertisementLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AdvertisementLease")
            .field("node_id", &self.node_id)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl ProtocolListener {
    /// Returns the protocol routed to this listener.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }

    /// Waits for the next authenticated peer stream.
    pub async fn accept(&mut self) -> Option<PeerStream> {
        self.incoming.recv().await
    }
}

impl fmt::Debug for ProtocolListener {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProtocolListener")
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl PeerStream {
    /// Returns the durable identity authenticated by the peer's Iroh endpoint.
    #[must_use]
    pub const fn peer_id(&self) -> iroh::EndpointId {
        self.peer
    }

    /// Returns the application protocol bound into this session's grant.
    #[must_use]
    pub const fn protocol(&self) -> &ProtocolId {
        &self.protocol
    }
}

impl fmt::Debug for PeerStream {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PeerStream")
            .field("peer_id", &self.peer)
            .field("protocol", &self.protocol)
            .finish_non_exhaustive()
    }
}

impl AsyncRead for PeerStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), context, buffer)
    }
}

impl AsyncWrite for PeerStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        AsyncWrite::poll_write(Pin::new(&mut self.send), context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), context)
    }
}

impl TcpBridge {
    /// Registers the fixed TCP bridge protocol before advertising a provider.
    pub async fn listen(node: &Node) -> Result<ProtocolListener, NetworkError> {
        let protocol = ProtocolId::new(TCP_BRIDGE_PROTOCOL)?;
        node.listen(protocol).await
    }

    /// Serves one fixed loopback TCP target over authenticated peer sessions.
    pub async fn serve(
        mut listener: ProtocolListener,
        target: SocketAddr,
    ) -> Result<(), NetworkError> {
        require_loopback(target)?;
        if listener.protocol().as_str() != TCP_BRIDGE_PROTOCOL {
            return Err(NetworkError::Protocol(
                "TCP bridge received a listener for the wrong protocol".to_owned(),
            ));
        }
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                incoming = listener.accept(), if connections.len() < MAX_CONCURRENT_STREAMS => {
                    let Some(mut session) = incoming else {
                        return Ok(());
                    };
                    connections.spawn(async move {
                        let mut origin = TcpStream::connect(target).await?;
                        tokio::io::copy_bidirectional(&mut origin, &mut session).await?;
                        Ok::<(), NetworkError>(())
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Ok(Err(error))) = completed {
                        tracing::warn!(%error, "network TCP bridge upstream closed");
                    } else if let Some(Err(error)) = completed {
                        tracing::warn!(%error, "network TCP bridge upstream task failed");
                    }
                }
            }
        }
    }

    /// Forwards a node-local loopback listener to one peer's published service.
    pub async fn connect(
        node: &Node,
        provider: iroh::EndpointId,
        listener: TcpListener,
    ) -> Result<(), NetworkError> {
        require_loopback(listener.local_addr()?)?;
        let protocol = ProtocolId::new(TCP_BRIDGE_PROTOCOL)?;
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept(), if connections.len() < MAX_CONCURRENT_STREAMS => {
                    let (stream, peer) = accepted?;
                    let node = node.dialer.clone();
                    let protocol = protocol.clone();
                    connections.spawn(async move {
                        if let Err(error) = forward_downstream(node, provider, protocol, stream).await {
                            tracing::warn!(%peer, %error, "network TCP bridge downstream closed");
                        }
                    });
                }
                completed = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%error, "network TCP bridge downstream task failed");
                    }
                }
            }
        }
    }
}

async fn forward_downstream(
    node: HubDialer,
    provider: iroh::EndpointId,
    protocol: ProtocolId,
    mut stream: TcpStream,
) -> Result<(), NetworkError> {
    let mut session = node.connect_peer(provider, &protocol).await?;
    tokio::io::copy_bidirectional(&mut stream, &mut session).await?;
    Ok(())
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
        let grants = self.grants.clone();
        let listeners = self.listeners.clone();
        let cluster_view = self.cluster_view.clone();
        let control_task = AbortOnDropHandle::new(tokio::spawn(async move {
            let result =
                node_control_loop(grants, listeners, cluster_view.clone(), send, recv).await;
            cluster_view.disconnect_all().await;
            if let Err(error) = result {
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

    async fn request_session(
        &self,
        provider: iroh::EndpointId,
        protocol: &ProtocolId,
    ) -> Result<(EndpointAddr, [u8; TOKEN_BYTES]), NetworkError> {
        let (mut send, mut recv) = self.open_bi().await?;
        write_stream_prefix(&mut send, &self.ticket.token, STREAM_SESSION_REQUEST).await?;
        write_frame(
            &mut send,
            &SessionRequest {
                provider,
                protocol: protocol.to_string(),
            },
        )
        .await?;
        send.finish()
            .map_err(|error| NetworkError::Protocol(error.to_string()))?;
        match tokio::time::timeout(AUTH_TIMEOUT, read_frame(&mut recv))
            .await
            .map_err(|_| NetworkError::Protocol("hub session grant timed out".to_owned()))??
        {
            SessionGrant::Granted { address, token } => Ok((address, token)),
            SessionGrant::Rejected { message } => Err(NetworkError::Protocol(format!(
                "hub rejected protocol {protocol} to {provider}: {message}"
            ))),
        }
    }

    async fn publish_advertisement(&self, record: SignedAdvertisement) -> Result<(), NetworkError> {
        let (mut send, mut recv) = self.open_bi().await?;
        write_stream_prefix(&mut send, &self.ticket.token, STREAM_ADVERTISEMENT_REQUEST).await?;
        write_frame(&mut send, &AdvertisementPublish { record }).await?;
        send.finish()
            .map_err(|error| NetworkError::Protocol(error.to_string()))?;
        match tokio::time::timeout(AUTH_TIMEOUT, read_frame(&mut recv))
            .await
            .map_err(|_| {
                NetworkError::Protocol("hub advertisement response timed out".to_owned())
            })?? {
            AdvertisementPublishResponse::Accepted => Ok(()),
            AdvertisementPublishResponse::Rejected { message } => {
                Err(NetworkError::InvalidAdvertisement(message))
            }
        }
    }

    async fn connect_peer(
        &self,
        peer: iroh::EndpointId,
        protocol: &ProtocolId,
    ) -> Result<PeerStream, NetworkError> {
        let (address, token) = self.request_session(peer, protocol).await?;
        let connection = self
            .endpoint
            .connect(address, NODE_ALPN)
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        let (mut send, mut recv) = connection
            .open_bi()
            .await
            .map_err(|error| NetworkError::Endpoint(error.to_string()))?;
        write_frame(
            &mut send,
            &SessionOpen {
                token,
                protocol: protocol.to_string(),
            },
        )
        .await?;
        match tokio::time::timeout(AUTH_TIMEOUT, read_frame(&mut recv))
            .await
            .map_err(|_| NetworkError::Protocol("peer session acceptance timed out".to_owned()))??
        {
            SessionOpenResponse::Accepted => Ok(PeerStream {
                peer,
                protocol: protocol.clone(),
                _connection: connection,
                send,
                recv,
            }),
            SessionOpenResponse::Rejected { message } => Err(NetworkError::Protocol(format!(
                "peer rejected protocol {protocol}: {message}"
            ))),
        }
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
                        if let Err(error) = protocol.handle_request(remote, send, recv).await {
                            tracing::warn!(%remote, %error, "Iroh hub request failed");
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
        let removed = {
            let mut nodes = self.nodes.peers.lock().await;
            if nodes
                .get(&remote)
                .is_some_and(|node| node.generation == registered.generation)
            {
                nodes.remove(&remote);
                true
            } else {
                false
            }
        };
        if removed {
            self.nodes.node_disconnected(remote).await;
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
        self.nodes.send_snapshot(&registered).await?;
        self.nodes.node_reconnected(remote).await;
        tracing::info!(node_endpoint_id = %remote, "registered durable Iroh node identity");
        Ok(registered)
    }

    async fn handle_request(
        &self,
        requester: iroh::EndpointId,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) -> Result<(), NetworkError> {
        let kind = read_stream_kind(&mut recv, &self.token).await?;
        match kind {
            STREAM_SESSION_REQUEST => {
                let SessionRequest { provider, protocol } = read_frame(&mut recv).await?;
                let response = self
                    .grant_session(requester, provider, ProtocolId::new(protocol)?)
                    .await;
                write_frame(&mut send, &response).await?;
            }
            STREAM_ADVERTISEMENT_REQUEST => {
                let AdvertisementPublish { record } = read_frame(&mut recv).await?;
                let response = match self.nodes.publish(requester, record).await {
                    Ok(()) => AdvertisementPublishResponse::Accepted,
                    Err(error) => AdvertisementPublishResponse::Rejected {
                        message: error.to_string(),
                    },
                };
                write_frame(&mut send, &response).await?;
            }
            _ => {
                return Err(NetworkError::Protocol(
                    "remote endpoint opened an unexpected hub stream kind".to_owned(),
                ));
            }
        }
        send.finish()
            .map_err(|error| NetworkError::Protocol(error.to_string()))?;
        Ok(())
    }

    async fn grant_session(
        &self,
        requester: iroh::EndpointId,
        provider: iroh::EndpointId,
        protocol: ProtocolId,
    ) -> SessionGrant {
        if requester == provider {
            return SessionGrant::Rejected {
                message: "a peer session requires two distinct nodes".to_owned(),
            };
        }
        let provider_peer = {
            let nodes = self.nodes.peers.lock().await;
            if !nodes.contains_key(&requester) {
                return SessionGrant::Rejected {
                    message: "requesting node is no longer registered".to_owned(),
                };
            }
            let Some(provider_peer) = nodes.get(&provider).cloned() else {
                return SessionGrant::Rejected {
                    message: format!("provider {provider} is not connected"),
                };
            };
            provider_peer
        };
        let token = match random_bytes::<TOKEN_BYTES>() {
            Ok(token) => token,
            Err(error) => {
                return SessionGrant::Rejected {
                    message: error.to_string(),
                };
            }
        };
        let request = ControlMessage::Grant {
            token,
            requester,
            protocol: protocol.to_string(),
        };
        if let Err(error) = expect_control(
            &provider_peer,
            &request,
            |message| matches!(message, ControlMessage::Granted),
            "provider rejected the peer session grant",
        )
        .await
        {
            return SessionGrant::Rejected {
                message: error.to_string(),
            };
        }
        SessionGrant::Granted {
            address: provider_peer.address.clone(),
            token,
        }
    }
}

impl ProtocolHandler for NodeProtocol {
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
                    let connection = connection.clone();
                    streams.spawn(async move {
                        let _permit = permit;
                        if let Err(error) = protocol
                            .accept_session(remote, connection, send, recv)
                            .await
                        {
                            tracing::warn!(%remote, %error, "Iroh bilateral peer session failed");
                        }
                    });
                }
                completed = streams.join_next(), if !streams.is_empty() => {
                    if let Some(Err(error)) = completed {
                        tracing::warn!(%remote, %error, "Iroh bilateral peer task failed");
                    }
                }
            }
        }
        while streams.join_next().await.is_some() {}
        Ok(())
    }
}

impl NodeProtocol {
    async fn accept_session(
        &self,
        remote: iroh::EndpointId,
        connection: iroh::endpoint::Connection,
        mut send: iroh::endpoint::SendStream,
        mut recv: iroh::endpoint::RecvStream,
    ) -> Result<(), NetworkError> {
        let SessionOpen { token, protocol } =
            tokio::time::timeout(AUTH_TIMEOUT, read_frame(&mut recv))
                .await
                .map_err(|_| NetworkError::Protocol("peer session hello timed out".to_owned()))??;
        let protocol = ProtocolId::new(protocol)?;
        let mut grants = self.grants.lock().await;
        let now = Instant::now();
        grants.retain(|grant| grant.expires_at > now);
        let Some(index) = grants.iter().position(|grant| {
            grant.requester == remote
                && grant.protocol == protocol
                && constant_time_eq_32(&grant.token, &token)
        }) else {
            write_frame(
                &mut send,
                &SessionOpenResponse::Rejected {
                    message: "no matching identity- and protocol-bound grant".to_owned(),
                },
            )
            .await?;
            send.finish()
                .map_err(|error| NetworkError::Protocol(error.to_string()))?;
            return Ok(());
        };
        grants.swap_remove(index);
        drop(grants);

        let listener = self.listeners.lock().await.get(&protocol).cloned();
        let Some(listener) = listener else {
            write_frame(
                &mut send,
                &SessionOpenResponse::Rejected {
                    message: "protocol listener is no longer available".to_owned(),
                },
            )
            .await?;
            send.finish()
                .map_err(|error| NetworkError::Protocol(error.to_string()))?;
            return Ok(());
        };
        let permit = match listener.try_reserve_owned() {
            Ok(permit) => permit,
            Err(error) => {
                let message = match error {
                    mpsc::error::TrySendError::Closed(_) => "protocol listener is closed",
                    mpsc::error::TrySendError::Full(_) => "protocol listener queue is full",
                };
                write_frame(
                    &mut send,
                    &SessionOpenResponse::Rejected {
                        message: message.to_owned(),
                    },
                )
                .await?;
                send.finish()
                    .map_err(|error| NetworkError::Protocol(error.to_string()))?;
                return Ok(());
            }
        };
        write_frame(&mut send, &SessionOpenResponse::Accepted).await?;
        permit.send(PeerStream {
            peer: remote,
            protocol,
            _connection: connection,
            send,
            recv,
        });
        Ok(())
    }
}

async fn node_control_loop(
    grants: Arc<Mutex<Vec<PendingSessionGrant>>>,
    listeners: Arc<Mutex<HashMap<ProtocolId, mpsc::Sender<PeerStream>>>>,
    cluster_view: Arc<discovery::ClusterView>,
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
) -> Result<(), NetworkError> {
    loop {
        let message = read_idle_control(&mut recv).await?;
        let response = match message {
            ControlMessage::Grant {
                token,
                requester,
                protocol,
            } => {
                let protocol = match ProtocolId::new(protocol) {
                    Ok(protocol) => protocol,
                    Err(error) => {
                        write_frame(
                            &mut send,
                            &ControlMessage::Rejected {
                                message: error.to_string(),
                            },
                        )
                        .await?;
                        continue;
                    }
                };
                let listener_available = listeners
                    .lock()
                    .await
                    .get(&protocol)
                    .is_some_and(|listener| !listener.is_closed());
                if !listener_available {
                    ControlMessage::Rejected {
                        message: format!("protocol {protocol} has no live listener"),
                    }
                } else {
                    let mut grants = grants.lock().await;
                    let now = Instant::now();
                    grants.retain(|grant| grant.expires_at > now);
                    if grants.len() >= MAX_PENDING_GRANTS {
                        ControlMessage::Rejected {
                            message: "node has too many pending bilateral grants".to_owned(),
                        }
                    } else {
                        grants.push(PendingSessionGrant {
                            token,
                            requester,
                            protocol,
                            expires_at: now + GRANT_LIFETIME,
                        });
                        ControlMessage::Granted
                    }
                }
            }
            ControlMessage::AdvertisementChanged { kind, record } => {
                let kind = match kind {
                    AdvertisementChangeKind::Joined => discovery::PresenceKind::Joined,
                    AdvertisementChangeKind::Updated => discovery::PresenceKind::Updated,
                    AdvertisementChangeKind::Disconnected => discovery::PresenceKind::Disconnected,
                    AdvertisementChangeKind::Expired => discovery::PresenceKind::Expired,
                };
                cluster_view
                    .apply(discovery::PresenceChange { kind, record })
                    .await;
                continue;
            }
            ControlMessage::AdvertisementSnapshotReset => {
                cluster_view.reset().await;
                continue;
            }
            other => ControlMessage::Rejected {
                message: format!("unexpected hub control message {}", control_name(&other)),
            },
        };
        write_frame(&mut send, &response).await?;
    }
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

async fn read_idle_control(
    recv: &mut (impl AsyncRead + Unpin),
) -> Result<ControlMessage, NetworkError> {
    read_frame(recv).await
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
    let kind = read_stream_kind(recv, token).await?;
    if kind != expected_kind {
        return Err(NetworkError::Protocol(
            "remote endpoint opened an unexpected stream kind".to_owned(),
        ));
    }
    Ok(())
}

async fn read_stream_kind(
    recv: &mut iroh::endpoint::RecvStream,
    token: &[u8; TOKEN_BYTES],
) -> Result<u8, NetworkError> {
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
    Ok(kind[0])
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
        ControlMessage::AdvertisementChanged { .. } => "advertisement_changed",
        ControlMessage::AdvertisementSnapshotReset => "advertisement_snapshot_reset",
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

    async fn present_session_grant(
        endpoint: &Endpoint,
        address: EndpointAddr,
        token: [u8; TOKEN_BYTES],
        protocol: &ProtocolId,
    ) -> SessionOpenResponse {
        let connection = endpoint.connect(address, NODE_ALPN).await.unwrap();
        let (mut send, mut recv) = connection.open_bi().await.unwrap();
        write_frame(
            &mut send,
            &SessionOpen {
                token,
                protocol: protocol.to_string(),
            },
        )
        .await
        .unwrap();
        read_frame(&mut recv).await.unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn idle_control_channel_does_not_reuse_the_authentication_timeout() {
        let (mut send, mut recv) = tokio::io::duplex(MAX_CONTROL_BYTES);
        let reader = tokio::spawn(async move { read_idle_control(&mut recv).await });

        tokio::time::advance(AUTH_TIMEOUT + Duration::from_secs(1)).await;
        assert!(!reader.is_finished());

        write_frame(&mut send, &ControlMessage::Granted)
            .await
            .unwrap();
        assert!(matches!(
            reader.await.unwrap().unwrap(),
            ControlMessage::Granted
        ));
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
        assert_eq!(first_ticket.address.id, second_ticket.address.id);
        assert_eq!(first_ticket.token, second_ticket.token);

        let provider_identity = NodeIdentity::generate().unwrap();
        let provider_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(provider_identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let provider = Node::join_with_endpoint(second_ticket.clone(), provider_endpoint)
            .await
            .unwrap();
        let protocol = ProtocolId::new("nanocodex/restart-test/1").unwrap();
        let mut listener = provider.listen(protocol.clone()).await.unwrap();

        let requester_identity = NodeIdentity::generate().unwrap();
        let requester_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(requester_identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let requester = Node::join_with_endpoint(second_ticket, requester_endpoint)
            .await
            .unwrap();
        let mut outgoing = requester
            .connect(provider.endpoint_id(), &protocol)
            .await
            .unwrap();
        let mut incoming = listener.accept().await.unwrap();
        outgoing.write_all(b"ping").await.unwrap();
        let mut request = [0; 4];
        incoming.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"ping");

        requester.shutdown().await.unwrap();
        provider.shutdown().await.unwrap();
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

        let bridge = TcpBridgeTicket::new(ticket.clone(), secret.public()).unwrap();
        let bridge_encoded = bridge.to_string();
        let decoded_bridge = TcpBridgeTicket::from_str(&bridge_encoded).unwrap();
        assert_eq!(decoded_bridge.provider_id(), secret.public());
        assert_eq!(decoded_bridge.join_ticket().token, ticket.token);
        assert!(!format!("{bridge:?}").contains(&ticket.to_string()));
        assert!(TcpBridgeTicket::from_str(&ticket.to_string()).is_err());
    }

    #[test]
    fn protocol_names_are_bounded_and_wire_stable() {
        let protocol = ProtocolId::new("nanocodex/tasks.claim_v1").unwrap();
        assert_eq!(protocol.as_str(), "nanocodex/tasks.claim_v1");
        assert_eq!(ProtocolId::from_str(protocol.as_str()).unwrap(), protocol);
        for invalid in ["", "spaces are not stable", "emoji/🦀"] {
            assert!(ProtocolId::new(invalid).is_err());
        }
        assert!(ProtocolId::new("a".repeat(MAX_PROTOCOL_BYTES + 1)).is_err());
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

        let provider_identity = NodeIdentity::generate().unwrap();
        let provider_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(provider_identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let provider = Node::join_with_endpoint(ticket.clone(), provider_endpoint)
            .await
            .unwrap();
        let protocol = ProtocolId::new("nanocodex/multiplex-test/1").unwrap();
        let mut listener = provider.listen(protocol.clone()).await.unwrap();

        let requester_identity = NodeIdentity::generate().unwrap();
        let requester_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(requester_identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let requester = Node::join_with_endpoint(ticket, requester_endpoint)
            .await
            .unwrap();

        let mut connection_ids = Vec::new();
        for _ in 0..2 {
            let mut outgoing = requester
                .connect(provider.endpoint_id(), &protocol)
                .await
                .unwrap();
            let mut incoming = listener.accept().await.unwrap();
            connection_ids.push(
                requester
                    .dialer
                    .connection
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .connection
                    .stable_id(),
            );
            outgoing.write_all(b"ping").await.unwrap();
            let mut request = [0; 4];
            incoming.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
        }

        assert_eq!(connection_ids[0], connection_ids[1]);
        requester.shutdown().await.unwrap();
        provider.shutdown().await.unwrap();
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn nodes_exchange_application_bytes_over_protocol_bound_sessions() {
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

        let protocol = ProtocolId::new("nanocodex/example/1").unwrap();
        let mut second_listener = second.listen(protocol.clone()).await.unwrap();
        assert!(second.listen(protocol.clone()).await.is_err());

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

        let (address, token) = first
            .dialer
            .request_session(second_id, &protocol)
            .await
            .unwrap();
        let wrong_protocol = ProtocolId::new("nanocodex/wrong-protocol/1").unwrap();
        assert!(matches!(
            present_session_grant(
                &first.dialer.endpoint,
                address.clone(),
                token,
                &wrong_protocol,
            )
            .await,
            SessionOpenResponse::Rejected { message }
                if message.contains("identity- and protocol-bound")
        ));
        let attacker = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        assert!(matches!(
            present_session_grant(&attacker, address, token, &protocol).await,
            SessionOpenResponse::Rejected { message }
                if message.contains("identity- and protocol-bound")
        ));
        attacker.close().await;

        let mut outgoing = tokio::time::timeout(TEST_TIMEOUT, first.connect(second_id, &protocol))
            .await
            .expect("outgoing peer session timed out")
            .unwrap();
        let mut incoming = tokio::time::timeout(TEST_TIMEOUT, second_listener.accept())
            .await
            .expect("incoming peer session timed out")
            .unwrap();
        assert_eq!(outgoing.peer_id(), second_id);
        assert_eq!(incoming.peer_id(), first_id);
        assert_eq!(outgoing.protocol(), &protocol);
        assert_eq!(incoming.protocol(), &protocol);
        outgoing.write_all(b"ping").await.unwrap();
        let mut request = [0; 4];
        incoming.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"ping");
        incoming.write_all(b"pong").await.unwrap();
        let mut response = [0; 4];
        outgoing.read_exact(&mut response).await.unwrap();
        assert_eq!(&response, b"pong");

        let reverse_protocol = ProtocolId::new("nanocodex/example-reverse/1").unwrap();
        let mut first_listener = first.listen(reverse_protocol.clone()).await.unwrap();
        let reverse_outgoing = second.connect(first_id, &reverse_protocol).await.unwrap();
        let reverse_incoming = first_listener.accept().await.unwrap();
        assert_eq!(reverse_outgoing.peer_id(), first_id);
        assert_eq!(reverse_incoming.peer_id(), second_id);

        assert!(matches!(
            first.connect(first_id, &protocol).await,
            Err(NetworkError::SameNode)
        ));
        let unavailable = ProtocolId::new("nanocodex/unavailable/1").unwrap();
        assert!(matches!(
            first.connect(second_id, &unavailable).await,
            Err(NetworkError::Protocol(message))
                if message.contains("has no live listener")
        ));

        drop(outgoing);
        drop(incoming);
        drop(reverse_outgoing);
        drop(reverse_incoming);
        drop(first_listener);
        drop(second_listener);
        first.shutdown().await.unwrap();
        second.shutdown().await.unwrap();
        tokio::time::timeout(TEST_TIMEOUT, hub.shutdown())
            .await
            .expect("Iroh hub shutdown timed out")
            .unwrap();
    }

    #[tokio::test]
    async fn signed_capability_leases_drive_a_watchable_cluster_view() {
        const TEST_TIMEOUT: Duration = Duration::from_secs(5);
        const LEASE: Duration = Duration::from_millis(500);

        async fn local_node(ticket: JoinTicket, identity: &NodeIdentity) -> Node {
            let endpoint = Endpoint::builder(presets::Minimal)
                .secret_key(identity.secret_key.clone())
                .relay_mode(iroh::RelayMode::Disabled)
                .clear_ip_transports()
                .bind_addr("127.0.0.1:0")
                .unwrap()
                .bind()
                .await
                .unwrap();
            Node::join_with_endpoint(ticket, endpoint).await.unwrap()
        }

        async fn change(watcher: &mut PeerWatcher) -> PeerChange {
            tokio::time::timeout(TEST_TIMEOUT, watcher.next())
                .await
                .expect("cluster-view change timed out")
                .expect("cluster-view watcher closed")
        }

        fn worker_advertisement(
            revision: u64,
            free_slots: u64,
            cpu_cores: u64,
            protocol: &ProtocolId,
        ) -> NodeAdvertisement {
            NodeAdvertisement::new(revision)
                .with_service(protocol.clone())
                .with_attribute("cpu.arch", "aarch64")
                .with_attribute("cpu.cores", cpu_cores)
                .with_attribute("worker.free_slots", free_slots)
                .with_attribute(
                    "artifacts",
                    CapabilityValue::from(["sha256:model", "sha256:image"]),
                )
                .lease_for(LEASE)
        }

        let _test_permit = TEST_ENDPOINT_PERMIT.acquire().await.unwrap();
        let hub_endpoint = Endpoint::builder(presets::Minimal)
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let (hub, ticket) = Hub::bind_with_endpoint(hub_endpoint, false).await.unwrap();

        let first_observer_identity = NodeIdentity::generate().unwrap();
        let first_observer = local_node(ticket.clone(), &first_observer_identity).await;
        let second_observer_identity = NodeIdentity::generate().unwrap();
        let second_observer = local_node(ticket.clone(), &second_observer_identity).await;
        let worker_identity = NodeIdentity::generate().unwrap();
        let worker_id = worker_identity.endpoint_id();
        let worker = local_node(ticket.clone(), &worker_identity).await;
        let protocol = ProtocolId::new("nanocodex.worker/1").unwrap();
        let mut listener = worker.listen(protocol.clone()).await.unwrap();

        let query = Query::service(protocol.clone())
            .attribute_eq("cpu.arch", "aarch64")
            .unwrap()
            .attribute_at_least("worker.free_slots", 1)
            .unwrap()
            .attribute_contains("artifacts", "sha256:model")
            .unwrap();
        let mut first_view = first_observer.watch(query.clone()).await;
        let mut second_view = second_observer.watch(query).await;

        let first_lease = worker
            .advertise(worker_advertisement(1, 2, 8, &protocol))
            .await
            .unwrap();
        assert_eq!(first_lease.node_id(), worker_id);
        assert_eq!(first_lease.revision(), 1);
        for observed in [
            change(&mut first_view).await,
            change(&mut second_view).await,
        ] {
            assert!(matches!(
                observed,
                PeerChange::Joined(record)
                    if record.node_id() == worker_id
                        && record.advertisement().revision() == 1
            ));
        }
        tokio::time::sleep(LEASE + Duration::from_millis(150)).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), first_view.next())
                .await
                .is_err(),
            "a silent lease renewal must not look like a capability update"
        );

        let second_lease = worker
            .advertise(worker_advertisement(2, 2, 16, &protocol))
            .await
            .unwrap();
        drop(first_lease);
        assert!(matches!(
            change(&mut first_view).await,
            PeerChange::Updated(record)
                if record.advertisement().attributes().get("cpu.cores")
                    == Some(&CapabilityValue::Unsigned(16))
        ));

        let third_lease = worker
            .advertise(worker_advertisement(3, 0, 16, &protocol))
            .await
            .unwrap();
        drop(second_lease);
        assert!(matches!(
            change(&mut first_view).await,
            PeerChange::Unmatched(record) if record.advertisement().revision() == 3
        ));

        let fourth_advertisement = worker_advertisement(4, 1, 16, &protocol);
        let fourth_lease = worker
            .advertise(fourth_advertisement.clone())
            .await
            .unwrap();
        drop(third_lease);
        assert!(matches!(
            change(&mut first_view).await,
            PeerChange::Joined(record) if record.advertisement().revision() == 4
        ));

        assert!(
            worker
                .advertise(worker_advertisement(4, 9, 16, &protocol))
                .await
                .is_err()
        );

        let mut outgoing = first_observer.connect(worker_id, &protocol).await.unwrap();
        let mut incoming = listener.accept().await.unwrap();
        outgoing.write_all(b"work").await.unwrap();
        let mut bytes = [0; 4];
        incoming.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"work");
        drop(outgoing);
        drop(incoming);
        drop(listener);

        worker.shutdown().await.unwrap();
        assert!(matches!(
            change(&mut first_view).await,
            PeerChange::Disconnected(record) if record.node_id() == worker_id
        ));
        drop(fourth_lease);

        let restarted = local_node(ticket, &worker_identity).await;
        assert_eq!(restarted.endpoint_id(), worker_id);
        assert!(matches!(
            change(&mut first_view).await,
            PeerChange::Joined(record)
                if record.node_id() == worker_id && record.advertisement().revision() == 4
        ));
        let restarted_lease = restarted.advertise(fourth_advertisement).await.unwrap();
        drop(restarted_lease);
        assert!(matches!(
            change(&mut first_view).await,
            PeerChange::Expired(record)
                if record.node_id() == worker_id && record.advertisement().revision() == 4
        ));

        restarted.shutdown().await.unwrap();
        first_observer.shutdown().await.unwrap();
        second_observer.shutdown().await.unwrap();
        hub.shutdown().await.unwrap();
    }

    #[test]
    fn advertisement_signatures_bind_identity_expiry_and_capabilities() {
        let identity = iroh::SecretKey::from_bytes(&[0x11; 32]);
        let other = iroh::SecretKey::from_bytes(&[0x22; 32]);
        let protocol = ProtocolId::new("nanocodex.worker/1").unwrap();
        let record = SignedAdvertisement::sign(
            NodeAdvertisement::new(1)
                .with_service(protocol)
                .with_attribute("cpu.arch", "aarch64"),
            &identity,
        )
        .unwrap();

        record.verify(identity.public()).unwrap();
        assert!(record.verify(other.public()).is_err());

        let mut tampered = serde_json::to_value(&record).unwrap();
        tampered["advertisement"]["attributes"]["cpu.arch"]["value"] = serde_json::json!("x86_64");
        let tampered: SignedAdvertisement = serde_json::from_value(tampered).unwrap();
        assert!(tampered.verify(identity.public()).is_err());
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

        let provider_identity = NodeIdentity::generate().unwrap();
        let provider_id = provider_identity.endpoint_id();
        let provider_endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(provider_identity.secret_key.clone())
            .relay_mode(iroh::RelayMode::Disabled)
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let provider = Node::join_with_endpoint(ticket.clone(), provider_endpoint)
            .await
            .unwrap();
        let bridge_listener = TcpBridge::listen(&provider).await.unwrap();
        let bridge_server =
            tokio::spawn(async move { TcpBridge::serve(bridge_listener, origin_address).await });

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
            .clear_ip_transports()
            .bind_addr("127.0.0.1:0")
            .unwrap()
            .bind()
            .await
            .unwrap();
        let node = Node::join_with_endpoint(ticket, client_endpoint)
            .await
            .unwrap();
        let connector =
            tokio::spawn(async move { TcpBridge::connect(&node, provider_id, downstream).await });

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
        bridge_server.abort();
        let _ = bridge_server.await;
        provider.shutdown().await.unwrap();
        tokio::time::timeout(TEST_TIMEOUT, server.shutdown())
            .await
            .expect("Iroh server shutdown timed out")
            .unwrap();
    }
}
