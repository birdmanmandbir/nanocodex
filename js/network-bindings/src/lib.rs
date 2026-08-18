#![cfg(all(target_arch = "wasm32", target_os = "unknown"))]

use std::{collections::BTreeMap, future::Future, pin::Pin, str::FromStr, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use iroh::{Endpoint, RelayMode, endpoint::presets};
use js_sys::Promise;
use nanocodex_network::{
    AdvertisementLease, CapabilityValue, CatalogIngest, Hub, JoinAuthority, JoinTicket, Node,
    NodeAdvertisement, NodeIdentity, PeerChange, PeerStream, PeerWatcher, ProtocolId,
    ProtocolListener, Query, SessionAttestor, SessionAuthorization, SessionAuthorizer,
    SessionCredential, SessionCredentials, SessionDecision, SignedAdvertisement,
};
use serde::Deserialize;
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::oneshot,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{JsFuture, spawn_local};

const MAX_STREAM_CHUNK_BYTES: usize = 1024 * 1024;
const BROWSER_RELAY_TIMEOUT: Duration = Duration::from_secs(30);

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = ["globalThis", "nanocodexNetworkHost"], js_name = authorize)]
    fn host_authorize(policy_id: u32, request: &str) -> Result<Promise, JsValue>;

    #[wasm_bindgen(catch, js_namespace = ["globalThis", "nanocodexNetworkHost"], js_name = attest)]
    fn host_attest(attestor_id: u32, request: &str) -> Result<Promise, JsValue>;
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HubConfig {
    #[serde(default)]
    authority: Option<String>,
    #[serde(default)]
    relay_url: Option<String>,
    #[serde(default)]
    session_authorizer_id: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NodeConfig {
    ticket: String,
    #[serde(default)]
    identity: Option<String>,
    #[serde(default)]
    relay_url: Option<String>,
    #[serde(default)]
    incoming_authorizer_id: Option<u32>,
    #[serde(default)]
    peer_verifier_id: Option<u32>,
    #[serde(default)]
    peer_attestor_id: Option<u32>,
    #[serde(default)]
    peer_attestation: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CredentialsConfig {
    #[serde(default)]
    authority: Option<String>,
    #[serde(default)]
    peer: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AdvertisementConfig {
    revision: u64,
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    attributes: BTreeMap<String, Value>,
    #[serde(default)]
    lease_millis: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct QueryConfig {
    #[serde(default)]
    services: Vec<String>,
    #[serde(default)]
    equals: BTreeMap<String, Value>,
    #[serde(default)]
    minimums: BTreeMap<String, u64>,
    #[serde(default)]
    contains: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug)]
struct JavaScriptAuthorizer {
    id: u32,
}

impl SessionAuthorizer for JavaScriptAuthorizer {
    fn authorize<'a>(
        &'a self,
        request: SessionAuthorization,
    ) -> Pin<Box<dyn Future<Output = SessionDecision> + Send + 'a>> {
        let encoded = encode_authorization(&request);
        let promise = host_authorize(self.id, &encoded);
        let (result_tx, result_rx) = oneshot::channel();
        match promise {
            Ok(promise) => spawn_local(async move {
                let decision = match JsFuture::from(promise).await {
                    Ok(value) if value.as_bool() == Some(true) => SessionDecision::Allow,
                    _ => SessionDecision::Deny,
                };
                let _ = result_tx.send(decision);
            }),
            Err(_) => {
                let _ = result_tx.send(SessionDecision::Deny);
            }
        }
        Box::pin(async move { result_rx.await.unwrap_or(SessionDecision::Deny) })
    }
}

#[derive(Clone, Copy, Debug)]
struct JavaScriptAttestor {
    id: u32,
}

impl SessionAttestor for JavaScriptAttestor {
    fn attest<'a>(
        &'a self,
        request: SessionAuthorization,
    ) -> Pin<Box<dyn Future<Output = Option<SessionCredential>> + Send + 'a>> {
        let encoded = encode_authorization(&request);
        let promise = host_attest(self.id, &encoded);
        let (result_tx, result_rx) = oneshot::channel();
        match promise {
            Ok(promise) => spawn_local(async move {
                let credential = match JsFuture::from(promise).await {
                    Ok(value) => value
                        .as_string()
                        .and_then(|encoded| decode_credential(&encoded).ok()),
                    Err(_) => None,
                };
                let _ = result_tx.send(credential);
            }),
            Err(_) => {
                let _ = result_tx.send(None);
            }
        }
        Box::pin(async move { result_rx.await.unwrap_or(None) })
    }
}

/// Browser/WASM owner of an Iroh rendezvous hub.
#[wasm_bindgen(js_name = NetworkHub)]
pub struct WasmNetworkHub {
    inner: Option<Hub>,
    authority: JoinAuthority,
    ticket: JoinTicket,
}

#[wasm_bindgen(js_class = NetworkHub)]
impl WasmNetworkHub {
    /// Binds a relay-capable browser hub from JSON configuration.
    #[wasm_bindgen(js_name = bind)]
    pub async fn bind(config_json: &str) -> Result<Self, JsValue> {
        let config = parse_json::<HubConfig>(config_json, "network hub configuration")?;
        let authority = match config.authority {
            Some(encoded) => decode_authority(&encoded)?,
            None => JoinAuthority::generate().map_err(js_error)?,
        };
        let endpoint = bind_authority_endpoint(&authority, config.relay_url.as_deref()).await?;
        let mut builder = Hub::builder(&authority, endpoint).map_err(js_error)?;
        if let Some(id) = config.session_authorizer_id {
            builder = builder.session_authorizer(JavaScriptAuthorizer { id });
        }
        let (inner, ticket) = builder.spawn().await.map_err(js_error)?;
        Ok(Self {
            inner: Some(inner),
            authority,
            ticket,
        })
    }

    /// Returns the public endpoint identity.
    #[wasm_bindgen(getter, js_name = endpointId)]
    pub fn endpoint_id(&self) -> String {
        self.authority.endpoint_id().to_string()
    }

    /// Returns the shareable network admission ticket.
    #[wasm_bindgen(getter)]
    pub fn ticket(&self) -> String {
        self.ticket.to_string()
    }

    /// Exports secret authority material for caller-owned secure persistence.
    #[wasm_bindgen(js_name = exportAuthority)]
    pub fn export_authority(&self) -> String {
        encode_authority(&self.authority)
    }

    /// Opens a query-relative watcher over the hub's merged signed catalog.
    pub async fn watch(&self, query_json: &str) -> Result<WasmNetworkWatcher, JsValue> {
        let query = parse_query(query_json)?;
        Ok(WasmNetworkWatcher {
            inner: self.hub()?.catalog().watch(query).await,
        })
    }

    /// Returns a deterministic snapshot of matching signed advertisements.
    pub async fn snapshot(&self, query_json: &str) -> Result<String, JsValue> {
        let query = parse_query(query_json)?;
        let records = self.hub()?.catalog().snapshot(&query).await;
        serde_json::to_string(&records).map_err(js_error)
    }

    /// Merges a signed advertisement obtained from a DHT, control plane, or cache.
    #[wasm_bindgen(js_name = ingestAdvertisement)]
    pub async fn ingest_advertisement(&self, record_json: &str) -> Result<String, JsValue> {
        let record = parse_json::<SignedAdvertisement>(record_json, "signed advertisement")?;
        let result = self
            .hub()?
            .catalog()
            .ingest(record)
            .await
            .map_err(js_error)?;
        Ok(encode_catalog_ingest(result))
    }

    /// Gracefully stops the browser hub.
    pub async fn shutdown(&mut self) -> Result<(), JsValue> {
        let hub = self
            .inner
            .take()
            .ok_or_else(|| js_error("network hub is already shut down"))?;
        hub.shutdown().await.map_err(js_error)
    }
}

impl WasmNetworkHub {
    fn hub(&self) -> Result<&Hub, JsValue> {
        self.inner
            .as_ref()
            .ok_or_else(|| js_error("network hub is shut down"))
    }
}

/// Browser/WASM owner of one Iroh mesh node.
#[wasm_bindgen(js_name = NetworkNode)]
pub struct WasmNetworkNode {
    inner: Option<Node>,
    identity: NodeIdentity,
}

#[wasm_bindgen(js_class = NetworkNode)]
impl WasmNetworkNode {
    /// Joins a browser node from JSON configuration.
    #[wasm_bindgen(js_name = join)]
    pub async fn join(config_json: &str) -> Result<Self, JsValue> {
        let config = parse_json::<NodeConfig>(config_json, "network node configuration")?;
        if config.peer_attestor_id.is_some() && config.peer_attestation.is_some() {
            return Err(js_error(
                "peerAttestor and peerAttestation cannot both be configured",
            ));
        }
        let ticket = JoinTicket::from_str(&config.ticket).map_err(js_error)?;
        let identity = match config.identity {
            Some(encoded) => NodeIdentity::from_secret_bytes(decode_exact(&encoded)?),
            None => NodeIdentity::generate().map_err(js_error)?,
        };
        let endpoint = bind_node_endpoint(&identity, config.relay_url.as_deref()).await?;
        let mut builder = Node::builder(ticket, &identity, endpoint).map_err(js_error)?;
        if let Some(id) = config.incoming_authorizer_id {
            builder = builder.incoming_session_authorizer(JavaScriptAuthorizer { id });
        }
        if let Some(id) = config.peer_verifier_id {
            builder = builder.peer_verifier(JavaScriptAuthorizer { id });
        }
        if let Some(id) = config.peer_attestor_id {
            builder = builder.peer_attestor(JavaScriptAttestor { id });
        } else if let Some(encoded) = config.peer_attestation {
            builder = builder.peer_attestation(decode_credential(&encoded)?);
        }
        let inner = builder.spawn().await.map_err(js_error)?;
        Ok(Self {
            inner: Some(inner),
            identity,
        })
    }

    /// Returns the durable endpoint identity.
    #[wasm_bindgen(getter, js_name = endpointId)]
    pub fn endpoint_id(&self) -> String {
        self.identity.endpoint_id().to_string()
    }

    /// Exports secret identity bytes for caller-owned secure persistence.
    #[wasm_bindgen(js_name = exportIdentity)]
    pub fn export_identity(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.identity.secret_bytes())
    }

    /// Publishes and renews a signed capability advertisement.
    pub async fn advertise(
        &self,
        advertisement_json: &str,
    ) -> Result<WasmNetworkAdvertisementLease, JsValue> {
        let advertisement = parse_advertisement(advertisement_json)?;
        let lease = self
            .node()?
            .advertise(advertisement)
            .await
            .map_err(js_error)?;
        let watcher = lease.subscribe();
        Ok(WasmNetworkAdvertisementLease { lease, watcher })
    }

    /// Opens a query-relative watcher over the merged signed catalog.
    pub async fn watch(&self, query_json: &str) -> Result<WasmNetworkWatcher, JsValue> {
        let query = parse_query(query_json)?;
        Ok(WasmNetworkWatcher {
            inner: self.node()?.watch(query).await,
        })
    }

    /// Returns a deterministic snapshot of matching signed advertisements.
    pub async fn snapshot(&self, query_json: &str) -> Result<String, JsValue> {
        let query = parse_query(query_json)?;
        let records = self.node()?.catalog().snapshot(&query).await;
        serde_json::to_string(&records).map_err(js_error)
    }

    /// Merges a signed advertisement obtained from a DHT, control plane, or cache.
    #[wasm_bindgen(js_name = ingestAdvertisement)]
    pub async fn ingest_advertisement(&self, record_json: &str) -> Result<String, JsValue> {
        let record = parse_json::<SignedAdvertisement>(record_json, "signed advertisement")?;
        let result = self
            .node()?
            .catalog()
            .ingest(record)
            .await
            .map_err(js_error)?;
        Ok(encode_catalog_ingest(result))
    }

    /// Registers one application protocol for incoming bilaterally authorized streams.
    pub async fn listen(&self, protocol: &str) -> Result<WasmNetworkListener, JsValue> {
        let protocol = ProtocolId::new(protocol).map_err(js_error)?;
        let inner = self.node()?.listen(protocol).await.map_err(js_error)?;
        Ok(WasmNetworkListener { inner })
    }

    /// Opens one bilaterally authorized byte stream.
    pub async fn connect(
        &self,
        peer_id: &str,
        protocol: &str,
        credentials_json: &str,
    ) -> Result<WasmNetworkPeerStream, JsValue> {
        let peer = peer_id
            .parse()
            .map_err(|error| js_error(format!("invalid peer endpoint identity: {error}")))?;
        let protocol = ProtocolId::new(protocol).map_err(js_error)?;
        let config = parse_json::<CredentialsConfig>(credentials_json, "session credentials")?;
        let authority = config
            .authority
            .map(|value| decode_credential(&value))
            .transpose()?
            .unwrap_or_else(SessionCredential::none);
        let peer_credential = config
            .peer
            .map(|value| decode_credential(&value))
            .transpose()?
            .unwrap_or_else(SessionCredential::none);
        let inner = self
            .node()?
            .connect_with_credentials(
                peer,
                &protocol,
                SessionCredentials::new(authority, peer_credential),
            )
            .await
            .map_err(js_error)?;
        Ok(WasmNetworkPeerStream { inner: Some(inner) })
    }

    /// Gracefully stops the browser node.
    pub async fn shutdown(&mut self) -> Result<(), JsValue> {
        let node = self
            .inner
            .take()
            .ok_or_else(|| js_error("network node is already shut down"))?;
        node.shutdown().await.map_err(js_error)
    }
}

impl WasmNetworkNode {
    fn node(&self) -> Result<&Node, JsValue> {
        self.inner
            .as_ref()
            .ok_or_else(|| js_error("network node is shut down"))
    }
}

/// Browser handle keeping one signed advertisement renewal alive.
#[wasm_bindgen(js_name = NetworkAdvertisementLease)]
pub struct WasmNetworkAdvertisementLease {
    lease: AdvertisementLease,
    watcher: nanocodex_network::AdvertisementWatcher,
}

#[wasm_bindgen(js_class = NetworkAdvertisementLease)]
impl WasmNetworkAdvertisementLease {
    /// Returns the most recently signed record as JSON.
    pub fn latest(&self) -> Result<String, JsValue> {
        serde_json::to_string(&self.lease.latest()).map_err(js_error)
    }

    /// Returns the initial record and each coalesced renewal as JSON.
    pub async fn next(&mut self) -> Result<Option<String>, JsValue> {
        self.watcher
            .next()
            .await
            .map(|record| serde_json::to_string(&record).map_err(js_error))
            .transpose()
    }
}

/// Browser watcher over query-relative signed peer changes.
#[wasm_bindgen(js_name = NetworkWatcher)]
pub struct WasmNetworkWatcher {
    inner: PeerWatcher,
}

#[wasm_bindgen(js_class = NetworkWatcher)]
impl WasmNetworkWatcher {
    /// Waits for the next coalesced peer change and returns JSON.
    pub async fn next(&mut self) -> Result<Option<String>, JsValue> {
        self.inner
            .next()
            .await
            .map(|change| encode_peer_change(&change))
            .transpose()
    }
}

/// Browser listener for one application protocol.
#[wasm_bindgen(js_name = NetworkListener)]
pub struct WasmNetworkListener {
    inner: ProtocolListener,
}

#[wasm_bindgen(js_class = NetworkListener)]
impl WasmNetworkListener {
    /// Waits for the next fully authorized peer stream.
    pub async fn accept(&mut self) -> Option<WasmNetworkPeerStream> {
        self.inner
            .accept()
            .await
            .map(|inner| WasmNetworkPeerStream { inner: Some(inner) })
    }
}

/// Bounded browser byte-stream view over an authenticated Iroh peer session.
#[wasm_bindgen(js_name = NetworkPeerStream)]
pub struct WasmNetworkPeerStream {
    inner: Option<PeerStream>,
}

#[wasm_bindgen(js_class = NetworkPeerStream)]
impl WasmNetworkPeerStream {
    /// Returns the authenticated remote endpoint identity.
    #[wasm_bindgen(getter, js_name = peerId)]
    pub fn peer_id(&self) -> Result<String, JsValue> {
        Ok(self.stream()?.peer_id().to_string())
    }

    /// Returns the application protocol bound into the session grant.
    #[wasm_bindgen(getter)]
    pub fn protocol(&self) -> Result<String, JsValue> {
        Ok(self.stream()?.protocol().to_string())
    }

    /// Writes one bounded byte chunk.
    pub async fn write(&mut self, bytes: Vec<u8>) -> Result<(), JsValue> {
        if bytes.len() > MAX_STREAM_CHUNK_BYTES {
            return Err(js_error(format!(
                "network stream write exceeds {MAX_STREAM_CHUNK_BYTES} bytes"
            )));
        }
        self.stream_mut()?.write_all(&bytes).await.map_err(js_error)
    }

    /// Reads at most one bounded byte chunk. An empty result is EOF.
    pub async fn read(&mut self, max_bytes: u32) -> Result<Vec<u8>, JsValue> {
        let max_bytes = usize::try_from(max_bytes).map_err(js_error)?;
        if max_bytes == 0 || max_bytes > MAX_STREAM_CHUNK_BYTES {
            return Err(js_error(format!(
                "network stream read size must be between 1 and {MAX_STREAM_CHUNK_BYTES} bytes"
            )));
        }
        let mut bytes = vec![0; max_bytes];
        let read = self
            .stream_mut()?
            .read(&mut bytes)
            .await
            .map_err(js_error)?;
        bytes.truncate(read);
        Ok(bytes)
    }

    /// Finishes the sending side while retaining the receiving side.
    pub async fn finish(&mut self) -> Result<(), JsValue> {
        self.stream_mut()?.shutdown().await.map_err(js_error)
    }
}

impl WasmNetworkPeerStream {
    fn stream(&self) -> Result<&PeerStream, JsValue> {
        self.inner
            .as_ref()
            .ok_or_else(|| js_error("network peer stream is closed"))
    }

    fn stream_mut(&mut self) -> Result<&mut PeerStream, JsValue> {
        self.inner
            .as_mut()
            .ok_or_else(|| js_error("network peer stream is closed"))
    }
}

async fn bind_authority_endpoint(
    authority: &JoinAuthority,
    relay_url: Option<&str>,
) -> Result<Endpoint, JsValue> {
    let endpoint = match relay_url {
        Some(relay_url) => {
            let relay_url = relay_url
                .parse()
                .map_err(|error| js_error(format!("invalid browser relay URL: {error}")))?;
            authority
                .endpoint_builder(presets::Minimal)
                .relay_mode(RelayMode::custom([relay_url]))
                .bind()
                .await
        }
        None => authority.endpoint_builder(presets::N0).bind().await,
    }
    .map_err(js_error)?;
    await_browser_relay(&endpoint).await?;
    Ok(endpoint)
}

async fn bind_node_endpoint(
    identity: &NodeIdentity,
    relay_url: Option<&str>,
) -> Result<Endpoint, JsValue> {
    let endpoint = match relay_url {
        Some(relay_url) => {
            let relay_url = relay_url
                .parse()
                .map_err(|error| js_error(format!("invalid browser relay URL: {error}")))?;
            identity
                .endpoint_builder(presets::Minimal)
                .relay_mode(RelayMode::custom([relay_url]))
                .bind()
                .await
        }
        None => identity.endpoint_builder(presets::N0).bind().await,
    }
    .map_err(js_error)?;
    await_browser_relay(&endpoint).await?;
    Ok(endpoint)
}

async fn await_browser_relay(endpoint: &Endpoint) -> Result<(), JsValue> {
    tokio::time::timeout(BROWSER_RELAY_TIMEOUT, endpoint.online())
        .await
        .map_err(|_| {
            js_error("browser Iroh endpoint did not connect to a relay within 30 seconds")
        })?;
    Ok(())
}

fn encode_authorization(request: &SessionAuthorization) -> String {
    serde_json::json!({
        "requesterId": request.requester_id().to_string(),
        "providerId": request.provider_id().to_string(),
        "protocol": request.protocol().as_str(),
        "credential": URL_SAFE_NO_PAD.encode(request.credential().as_bytes()),
    })
    .to_string()
}

fn encode_authority(authority: &JoinAuthority) -> String {
    let (secret_key, token) = authority.secret_material();
    let mut material = Vec::with_capacity(secret_key.len() + token.len());
    material.extend_from_slice(&secret_key);
    material.extend_from_slice(&token);
    URL_SAFE_NO_PAD.encode(material)
}

fn decode_authority(encoded: &str) -> Result<JoinAuthority, JsValue> {
    let material = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| js_error(format!("invalid browser authority material: {error}")))?;
    if material.len() != 64 {
        return Err(js_error("browser authority material must contain 64 bytes"));
    }
    let mut secret_key = [0; 32];
    let mut token = [0; 32];
    secret_key.copy_from_slice(&material[..32]);
    token.copy_from_slice(&material[32..]);
    Ok(JoinAuthority::from_secret_material(secret_key, token))
}

fn decode_exact<const N: usize>(encoded: &str) -> Result<[u8; N], JsValue> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| js_error(format!("invalid base64url network secret: {error}")))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        js_error(format!(
            "network secret contains {} bytes; expected {N}",
            bytes.len()
        ))
    })
}

fn decode_credential(encoded: &str) -> Result<SessionCredential, JsValue> {
    let bytes = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|error| js_error(format!("invalid base64url session credential: {error}")))?;
    SessionCredential::new(bytes).map_err(js_error)
}

fn parse_advertisement(encoded: &str) -> Result<NodeAdvertisement, JsValue> {
    let config = parse_json::<AdvertisementConfig>(encoded, "network advertisement")?;
    let mut advertisement = NodeAdvertisement::new(config.revision);
    for service in config.services {
        advertisement = advertisement.with_service(ProtocolId::new(service).map_err(js_error)?);
    }
    for (key, value) in config.attributes {
        advertisement = advertisement.with_attribute(key, parse_capability(value)?);
    }
    if let Some(lease_millis) = config.lease_millis {
        advertisement = advertisement.lease_for(Duration::from_millis(lease_millis));
    }
    Ok(advertisement)
}

fn parse_query(encoded: &str) -> Result<Query, JsValue> {
    let config = parse_json::<QueryConfig>(encoded, "network query")?;
    let mut query = Query::default();
    for service in config.services {
        query = query.requiring_service(ProtocolId::new(service).map_err(js_error)?);
    }
    for (key, value) in config.equals {
        query = query
            .attribute_eq(key, parse_capability(value)?)
            .map_err(js_error)?;
    }
    for (key, minimum) in config.minimums {
        query = query.attribute_at_least(key, minimum).map_err(js_error)?;
    }
    for (key, item) in config.contains {
        query = query.attribute_contains(key, item).map_err(js_error)?;
    }
    Ok(query)
}

fn parse_capability(value: Value) -> Result<CapabilityValue, JsValue> {
    match value {
        Value::String(value) => Ok(CapabilityValue::String(value)),
        Value::Bool(value) => Ok(CapabilityValue::Boolean(value)),
        Value::Number(value) => value
            .as_u64()
            .map(CapabilityValue::Unsigned)
            .ok_or_else(|| js_error("network capability numbers must be unsigned integers")),
        Value::Array(values) => values
            .into_iter()
            .map(|value| {
                value
                    .as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| js_error("network capability arrays must contain only strings"))
            })
            .collect::<Result<_, _>>()
            .map(CapabilityValue::StringSet),
        _ => Err(js_error(
            "network capabilities must be strings, booleans, unsigned integers, or string arrays",
        )),
    }
}

fn encode_peer_change(change: &PeerChange) -> Result<String, JsValue> {
    let kind = match change {
        PeerChange::Joined(_) => "joined",
        PeerChange::Updated(_) => "updated",
        PeerChange::Expired(_) => "expired",
        PeerChange::Unmatched(_) => "unmatched",
    };
    serde_json::to_string(&serde_json::json!({
        "type": kind,
        "record": change.advertisement(),
    }))
    .map_err(js_error)
}

fn encode_catalog_ingest(result: CatalogIngest) -> String {
    match result {
        CatalogIngest::Applied => "applied",
        CatalogIngest::Replay => "replay",
        CatalogIngest::Stale => "stale",
    }
    .to_owned()
}

fn parse_json<T: for<'de> Deserialize<'de>>(encoded: &str, name: &str) -> Result<T, JsValue> {
    serde_json::from_str(encoded).map_err(|error| js_error(format!("invalid {name}: {error}")))
}

fn js_error(error: impl ToString) -> JsValue {
    js_sys::Error::new(&error.to_string()).into()
}
