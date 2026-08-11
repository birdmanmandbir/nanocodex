use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use iroh::{EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, mpsc};

use crate::{NetworkError, ProtocolId};

const ADVERTISEMENT_VERSION: u8 = 1;
const SIGNATURE_DOMAIN: &[u8] = b"nanocodex-network-advertisement\0";
const DEFAULT_LEASE: Duration = Duration::from_secs(30);
const MIN_LEASE: Duration = Duration::from_millis(100);
const MAX_LEASE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_SERVICES: usize = 32;
const MAX_ATTRIBUTES: usize = 64;
const MAX_ATTRIBUTE_KEY_BYTES: usize = 128;
const MAX_ATTRIBUTE_STRING_BYTES: usize = 1024;
const MAX_ATTRIBUTE_SET_ITEMS: usize = 128;
const WATCH_QUEUE_CAPACITY: usize = 256;

/// One typed fact advertised by a node.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum CapabilityValue {
    /// One exact text value, such as a CPU architecture or TEE kind.
    String(String),
    /// One unsigned quantity, such as free slots or RAM bytes.
    Unsigned(u64),
    /// One boolean capability.
    Boolean(bool),
    /// An unordered set, such as cached artifact content hashes.
    StringSet(BTreeSet<String>),
}

impl From<String> for CapabilityValue {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for CapabilityValue {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl From<u64> for CapabilityValue {
    fn from(value: u64) -> Self {
        Self::Unsigned(value)
    }
}

impl From<bool> for CapabilityValue {
    fn from(value: bool) -> Self {
        Self::Boolean(value)
    }
}

impl<const N: usize> From<[&str; N]> for CapabilityValue {
    fn from(values: [&str; N]) -> Self {
        Self::StringSet(values.into_iter().map(ToOwned::to_owned).collect())
    }
}

/// Application-defined services and capability facts published by one node.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeAdvertisement {
    revision: u64,
    services: BTreeSet<ProtocolId>,
    attributes: BTreeMap<String, CapabilityValue>,
    #[serde(skip, default = "default_lease")]
    lease_duration: Duration,
}

impl NodeAdvertisement {
    /// Starts one advertisement revision with the default 30-second lease.
    #[must_use]
    pub const fn new(revision: u64) -> Self {
        Self {
            revision,
            services: BTreeSet::new(),
            attributes: BTreeMap::new(),
            lease_duration: DEFAULT_LEASE,
        }
    }

    /// Adds one directly dialable application protocol.
    #[must_use]
    pub fn with_service(mut self, service: ProtocolId) -> Self {
        self.services.insert(service);
        self
    }

    /// Adds or replaces one application-defined capability fact.
    #[must_use]
    pub fn with_attribute(
        mut self,
        key: impl Into<String>,
        value: impl Into<CapabilityValue>,
    ) -> Self {
        self.attributes.insert(key.into(), value.into());
        self
    }

    /// Selects how long the record remains present without renewal.
    #[must_use]
    pub const fn lease_for(mut self, duration: Duration) -> Self {
        self.lease_duration = duration;
        self
    }

    /// Returns the caller-owned monotonic content revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Returns the advertised application protocols.
    pub fn services(&self) -> impl Iterator<Item = &ProtocolId> {
        self.services.iter()
    }

    /// Returns the application-defined capability facts.
    #[must_use]
    pub const fn attributes(&self) -> &BTreeMap<String, CapabilityValue> {
        &self.attributes
    }

    pub(crate) const fn lease_duration(&self) -> Duration {
        self.lease_duration
    }

    pub(crate) fn validate(&self) -> Result<(), NetworkError> {
        if self.revision == 0 {
            return Err(invalid_advertisement("revision must be greater than zero"));
        }
        if self.services.len() > MAX_SERVICES {
            return Err(invalid_advertisement(format!(
                "advertisement has more than {MAX_SERVICES} services"
            )));
        }
        if self.attributes.len() > MAX_ATTRIBUTES {
            return Err(invalid_advertisement(format!(
                "advertisement has more than {MAX_ATTRIBUTES} attributes"
            )));
        }
        if !(MIN_LEASE..=MAX_LEASE).contains(&self.lease_duration) {
            return Err(invalid_advertisement(format!(
                "lease must be between {} milliseconds and {} seconds",
                MIN_LEASE.as_millis(),
                MAX_LEASE.as_secs()
            )));
        }
        for service in &self.services {
            ProtocolId::new(service.as_str())?;
        }
        for (key, value) in &self.attributes {
            validate_attribute_key(key)?;
            validate_value(value)?;
        }
        Ok(())
    }

    fn same_content(&self, other: &Self) -> bool {
        self.revision == other.revision
            && self.services == other.services
            && self.attributes == other.attributes
    }
}

/// A transport-independent capability record signed by its Iroh node identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SignedAdvertisement {
    version: u8,
    node_id: EndpointId,
    issued_at_unix_millis: u64,
    expires_at_unix_millis: u64,
    advertisement: NodeAdvertisement,
    signature: Signature,
}

#[derive(Serialize)]
struct UnsignedAdvertisement<'a> {
    version: u8,
    node_id: EndpointId,
    issued_at_unix_millis: u64,
    expires_at_unix_millis: u64,
    advertisement: &'a NodeAdvertisement,
}

impl SignedAdvertisement {
    pub(crate) fn sign(
        advertisement: NodeAdvertisement,
        secret_key: &SecretKey,
    ) -> Result<Self, NetworkError> {
        advertisement.validate()?;
        let issued_at_unix_millis = unix_millis()?;
        let lease_millis = u64::try_from(advertisement.lease_duration().as_millis())
            .map_err(|_| invalid_advertisement("lease duration does not fit in u64"))?;
        let expires_at_unix_millis = issued_at_unix_millis
            .checked_add(lease_millis)
            .ok_or_else(|| invalid_advertisement("lease expiry overflowed"))?;
        let node_id = secret_key.public();
        let message = signing_message(
            node_id,
            issued_at_unix_millis,
            expires_at_unix_millis,
            &advertisement,
        )?;
        Ok(Self {
            version: ADVERTISEMENT_VERSION,
            node_id,
            issued_at_unix_millis,
            expires_at_unix_millis,
            advertisement,
            signature: secret_key.sign(&message),
        })
    }

    /// Returns the durable node identity which signed this record.
    #[must_use]
    pub const fn node_id(&self) -> EndpointId {
        self.node_id
    }

    /// Returns the signed application-owned advertisement body.
    #[must_use]
    pub const fn advertisement(&self) -> &NodeAdvertisement {
        &self.advertisement
    }

    /// Returns the absolute lease expiry as Unix milliseconds.
    #[must_use]
    pub const fn expires_at_unix_millis(&self) -> u64 {
        self.expires_at_unix_millis
    }

    pub(crate) fn verify(&self, expected_node: EndpointId) -> Result<(), NetworkError> {
        if self.version != ADVERTISEMENT_VERSION {
            return Err(invalid_advertisement(format!(
                "unsupported advertisement version {}",
                self.version
            )));
        }
        if self.node_id != expected_node {
            return Err(invalid_advertisement(
                "signed node identity does not match the authenticated Iroh endpoint",
            ));
        }
        self.advertisement.validate()?;
        let now = unix_millis()?;
        if self.issued_at_unix_millis > now.saturating_add(5_000) {
            return Err(invalid_advertisement(
                "advertisement issuance is too far in the future",
            ));
        }
        if self.expires_at_unix_millis <= now {
            return Err(invalid_advertisement("advertisement lease has expired"));
        }
        let lifetime = self
            .expires_at_unix_millis
            .checked_sub(self.issued_at_unix_millis)
            .ok_or_else(|| invalid_advertisement("advertisement expiry precedes issuance"))?;
        if lifetime < u64::try_from(MIN_LEASE.as_millis()).unwrap_or(u64::MAX)
            || lifetime > u64::try_from(MAX_LEASE.as_millis()).unwrap_or(0)
        {
            return Err(invalid_advertisement(
                "advertisement lease lifetime is out of bounds",
            ));
        }
        let message = signing_message(
            self.node_id,
            self.issued_at_unix_millis,
            self.expires_at_unix_millis,
            &self.advertisement,
        )?;
        self.node_id
            .verify(&message, &self.signature)
            .map_err(|_| invalid_advertisement("advertisement signature is invalid"))
    }

    pub(crate) fn same_revision_content(&self, other: &Self) -> bool {
        self.node_id == other.node_id && self.advertisement.same_content(&other.advertisement)
    }

    pub(crate) const fn is_expired_at(&self, now_unix_millis: u64) -> bool {
        self.expires_at_unix_millis <= now_unix_millis
    }
}

/// Local filtering over authenticated advertisements; it never selects or schedules work.
#[derive(Clone, Debug, Default)]
pub struct Query {
    services: BTreeSet<ProtocolId>,
    equals: BTreeMap<String, CapabilityValue>,
    minimums: BTreeMap<String, u64>,
    contains: BTreeMap<String, String>,
}

impl Query {
    /// Matches nodes advertising one service protocol.
    #[must_use]
    pub fn service(service: ProtocolId) -> Self {
        Self::default().requiring_service(service)
    }

    /// Requires another service protocol.
    #[must_use]
    pub fn requiring_service(mut self, service: ProtocolId) -> Self {
        self.services.insert(service);
        self
    }

    /// Requires an attribute to equal one exact typed value.
    pub fn attribute_eq(
        mut self,
        key: impl Into<String>,
        value: impl Into<CapabilityValue>,
    ) -> Result<Self, NetworkError> {
        let key = key.into();
        validate_attribute_key(&key)?;
        let value = value.into();
        validate_value(&value)?;
        self.equals.insert(key, value);
        Ok(self)
    }

    /// Requires an unsigned attribute to be at least `minimum`.
    pub fn attribute_at_least(
        mut self,
        key: impl Into<String>,
        minimum: u64,
    ) -> Result<Self, NetworkError> {
        let key = key.into();
        validate_attribute_key(&key)?;
        self.minimums.insert(key, minimum);
        Ok(self)
    }

    /// Requires a string-set attribute to contain one exact item.
    pub fn attribute_contains(
        mut self,
        key: impl Into<String>,
        item: impl Into<String>,
    ) -> Result<Self, NetworkError> {
        let key = key.into();
        validate_attribute_key(&key)?;
        let item = item.into();
        validate_string(&item)?;
        self.contains.insert(key, item);
        Ok(self)
    }

    fn matches(&self, record: &SignedAdvertisement) -> bool {
        let advertisement = record.advertisement();
        self.services.is_subset(&advertisement.services)
            && self
                .equals
                .iter()
                .all(|(key, expected)| advertisement.attributes.get(key) == Some(expected))
            && self.minimums.iter().all(|(key, minimum)| {
                matches!(
                    advertisement.attributes.get(key),
                    Some(CapabilityValue::Unsigned(actual)) if actual >= minimum
                )
            })
            && self.contains.iter().all(|(key, item)| {
                matches!(
                    advertisement.attributes.get(key),
                    Some(CapabilityValue::StringSet(items)) if items.contains(item)
                )
            })
    }
}

/// One query-relative change in the local cluster view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PeerChange {
    /// A connected node began matching the query.
    Joined(SignedAdvertisement),
    /// A matching connected node published a newer revision.
    Updated(SignedAdvertisement),
    /// A matching node's control connection closed before its lease expired.
    Disconnected(SignedAdvertisement),
    /// A matching node's advertisement lease expired.
    Expired(SignedAdvertisement),
    /// A newer revision remains online but no longer matches this query.
    Unmatched(SignedAdvertisement),
}

impl PeerChange {
    /// Returns the signed record associated with this change.
    #[must_use]
    pub const fn advertisement(&self) -> &SignedAdvertisement {
        match self {
            Self::Joined(record)
            | Self::Updated(record)
            | Self::Disconnected(record)
            | Self::Expired(record)
            | Self::Unmatched(record) => record,
        }
    }
}

/// Receiver for a filtered, live local cluster view.
pub struct PeerWatcher {
    incoming: mpsc::Receiver<PeerChange>,
}

impl PeerWatcher {
    /// Waits for the next matching cluster-view change.
    pub async fn next(&mut self) -> Option<PeerChange> {
        self.incoming.recv().await
    }
}

impl std::fmt::Debug for PeerWatcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PeerWatcher")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PresenceKind {
    Joined,
    Updated,
    Disconnected,
    Expired,
}

#[derive(Clone, Debug)]
pub(crate) struct PresenceChange {
    pub kind: PresenceKind,
    pub record: SignedAdvertisement,
}

#[derive(Default)]
pub(crate) struct ClusterView {
    state: Mutex<ViewState>,
}

#[derive(Default)]
struct ViewState {
    peers: HashMap<EndpointId, PeerPresence>,
    watchers: Vec<WatcherState>,
}

struct PeerPresence {
    record: SignedAdvertisement,
    connected: bool,
}

struct WatcherState {
    query: Query,
    own_id: EndpointId,
    matched: HashSet<EndpointId>,
    sender: mpsc::Sender<PeerChange>,
}

impl ClusterView {
    pub(crate) async fn watch(&self, own_id: EndpointId, query: Query) -> PeerWatcher {
        let (sender, incoming) = mpsc::channel(WATCH_QUEUE_CAPACITY);
        let mut state = self.state.lock().await;
        let mut matched = HashSet::new();
        for (peer, presence) in &state.peers {
            if *peer != own_id && presence.connected && query.matches(&presence.record) {
                matched.insert(*peer);
                let _ = sender.try_send(PeerChange::Joined(presence.record.clone()));
            }
        }
        state.watchers.push(WatcherState {
            query,
            own_id,
            matched,
            sender,
        });
        PeerWatcher { incoming }
    }

    pub(crate) async fn apply(&self, change: PresenceChange) {
        let peer = change.record.node_id();
        let mut state = self.state.lock().await;
        match change.kind {
            PresenceKind::Joined | PresenceKind::Updated => {
                state.peers.insert(
                    peer,
                    PeerPresence {
                        record: change.record.clone(),
                        connected: true,
                    },
                );
            }
            PresenceKind::Disconnected => {
                state.peers.insert(
                    peer,
                    PeerPresence {
                        record: change.record.clone(),
                        connected: false,
                    },
                );
            }
            PresenceKind::Expired => {
                state.peers.remove(&peer);
            }
        }

        state.watchers.retain_mut(|watcher| {
            if watcher.sender.is_closed() {
                return false;
            }
            if peer == watcher.own_id {
                return true;
            }
            let was_matching = watcher.matched.contains(&peer);
            let now_matching = matches!(change.kind, PresenceKind::Joined | PresenceKind::Updated)
                && watcher.query.matches(&change.record);
            let event = match (change.kind, was_matching, now_matching) {
                (PresenceKind::Joined, false, true) => {
                    Some(PeerChange::Joined(change.record.clone()))
                }
                (PresenceKind::Joined, true, true) | (PresenceKind::Updated, true, true) => {
                    Some(PeerChange::Updated(change.record.clone()))
                }
                (PresenceKind::Updated, false, true) => {
                    Some(PeerChange::Joined(change.record.clone()))
                }
                (PresenceKind::Updated, true, false) => {
                    Some(PeerChange::Unmatched(change.record.clone()))
                }
                (PresenceKind::Disconnected, true, _) => {
                    Some(PeerChange::Disconnected(change.record.clone()))
                }
                (PresenceKind::Expired, true, _) => {
                    Some(PeerChange::Expired(change.record.clone()))
                }
                _ => None,
            };
            if now_matching {
                watcher.matched.insert(peer);
            } else {
                watcher.matched.remove(&peer);
            }
            event.is_none_or(|event| watcher.sender.try_send(event).is_ok())
        });
    }

    pub(crate) async fn disconnect_all(&self) {
        let records = self
            .state
            .lock()
            .await
            .peers
            .values()
            .filter(|presence| presence.connected)
            .map(|presence| presence.record.clone())
            .collect::<Vec<_>>();
        for record in records {
            self.apply(PresenceChange {
                kind: PresenceKind::Disconnected,
                record,
            })
            .await;
        }
    }

    pub(crate) async fn reset(&self) {
        let records = {
            let mut state = self.state.lock().await;
            let records = state
                .peers
                .values()
                .map(|presence| presence.record.clone())
                .collect::<Vec<_>>();
            state.peers.clear();
            records
        };
        for record in records {
            self.apply(PresenceChange {
                kind: PresenceKind::Expired,
                record,
            })
            .await;
        }
    }
}

pub(crate) fn unix_millis() -> Result<u64, NetworkError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            invalid_advertisement(format!("system clock is before Unix epoch: {error}"))
        })?
        .as_millis();
    u64::try_from(millis).map_err(|_| invalid_advertisement("Unix time does not fit in u64"))
}

fn signing_message(
    node_id: EndpointId,
    issued_at_unix_millis: u64,
    expires_at_unix_millis: u64,
    advertisement: &NodeAdvertisement,
) -> Result<Vec<u8>, NetworkError> {
    let unsigned = UnsignedAdvertisement {
        version: ADVERTISEMENT_VERSION,
        node_id,
        issued_at_unix_millis,
        expires_at_unix_millis,
        advertisement,
    };
    let encoded = serde_json::to_vec(&unsigned).map_err(|error| {
        invalid_advertisement(format!("could not encode advertisement: {error}"))
    })?;
    let mut message = Vec::with_capacity(SIGNATURE_DOMAIN.len() + encoded.len());
    message.extend_from_slice(SIGNATURE_DOMAIN);
    message.extend_from_slice(&encoded);
    Ok(message)
}

fn validate_attribute_key(key: &str) -> Result<(), NetworkError> {
    if key.is_empty() || key.len() > MAX_ATTRIBUTE_KEY_BYTES {
        return Err(invalid_advertisement(format!(
            "attribute key must contain 1 to {MAX_ATTRIBUTE_KEY_BYTES} bytes"
        )));
    }
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(invalid_advertisement(
            "attribute keys must use only ASCII letters, digits, '.', '_', '-', or '/'",
        ));
    }
    Ok(())
}

fn validate_value(value: &CapabilityValue) -> Result<(), NetworkError> {
    match value {
        CapabilityValue::String(value) => validate_string(value),
        CapabilityValue::Unsigned(_) | CapabilityValue::Boolean(_) => Ok(()),
        CapabilityValue::StringSet(values) => {
            if values.len() > MAX_ATTRIBUTE_SET_ITEMS {
                return Err(invalid_advertisement(format!(
                    "attribute set has more than {MAX_ATTRIBUTE_SET_ITEMS} items"
                )));
            }
            values.iter().try_for_each(|value| validate_string(value))
        }
    }
}

fn validate_string(value: &str) -> Result<(), NetworkError> {
    if value.len() > MAX_ATTRIBUTE_STRING_BYTES {
        return Err(invalid_advertisement(format!(
            "attribute string exceeds {MAX_ATTRIBUTE_STRING_BYTES} bytes"
        )));
    }
    Ok(())
}

fn invalid_advertisement(message: impl Into<String>) -> NetworkError {
    NetworkError::InvalidAdvertisement(message.into())
}

const fn default_lease() -> Duration {
    DEFAULT_LEASE
}
