use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use iroh::{EndpointId, SecretKey, Signature};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, watch};

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
    /// A node began matching the query.
    Joined(SignedAdvertisement),
    /// A matching node published a newer revision.
    Updated(SignedAdvertisement),
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
            | Self::Expired(record)
            | Self::Unmatched(record) => record,
        }
    }
}

/// Receiver for a filtered, live local cluster view.
///
/// Pending changes are coalesced by durable peer identity. A slow consumer
/// therefore observes each peer's latest query-relative state without an
/// update-frequency-dependent queue or a fixed fleet-size ceiling.
pub struct PeerWatcher {
    queue: std::sync::Arc<WatcherQueue>,
    changed: watch::Receiver<u64>,
}

/// One identity-keyed local view merging signed records from any discovery source.
///
/// Iroh gossip feeds this catalog automatically. Applications may also ingest the
/// same signed record shape from a DHT, control plane, retained cache, or another
/// application-owned source. Signature, expiry, revision, and equivocation checks
/// are identical regardless of provenance.
#[derive(Clone)]
pub struct PeerCatalog {
    own_id: EndpointId,
    view: Arc<ClusterView>,
}

/// Result of merging one authenticated record into a [`PeerCatalog`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogIngest {
    /// The record added a peer, advanced its revision, or renewed its lease.
    Applied,
    /// The exact record or a newer renewal was already present.
    Replay,
    /// A newer content revision was already present.
    Stale,
}

impl PeerCatalog {
    pub(crate) const fn new(own_id: EndpointId, view: Arc<ClusterView>) -> Self {
        Self { own_id, view }
    }

    /// Merges one transport-independent signed advertisement from an external source.
    ///
    /// Invalid signatures, expired records, and same-revision equivocation are
    /// rejected. An accepted record updates every watcher sharing this catalog.
    pub async fn ingest(&self, record: SignedAdvertisement) -> Result<CatalogIngest, NetworkError> {
        record.verify(record.node_id())?;
        self.view.ingest(record).await
    }

    /// Opens a filtered watcher over the merged authenticated view.
    pub async fn watch(&self, query: Query) -> PeerWatcher {
        self.view.watch(self.own_id, query).await
    }

    /// Returns a deterministic snapshot of peers currently matching `query`.
    pub async fn snapshot(&self, query: &Query) -> Vec<SignedAdvertisement> {
        let mut records = self
            .view
            .active_records()
            .await
            .into_iter()
            .filter(|record| record.node_id() != self.own_id && query.matches(record))
            .collect::<Vec<_>>();
        records.sort_unstable_by_key(SignedAdvertisement::node_id);
        records
    }
}

impl std::fmt::Debug for PeerCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PeerCatalog")
            .field("own_id", &self.own_id)
            .finish_non_exhaustive()
    }
}

impl PeerWatcher {
    /// Waits for the next matching cluster-view change.
    pub async fn next(&mut self) -> Option<PeerChange> {
        loop {
            if let Some(change) = self.queue.pop() {
                return Some(change);
            }
            if self.changed.changed().await.is_err() {
                return self.queue.pop();
            }
        }
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
}

struct WatcherState {
    query: Query,
    own_id: EndpointId,
    queue: std::sync::Arc<WatcherQueue>,
    changed: watch::Sender<u64>,
}

#[derive(Default)]
struct WatcherQueue {
    state: StdMutex<WatcherQueueState>,
}

#[derive(Default)]
struct WatcherQueueState {
    delivered: HashMap<EndpointId, SignedAdvertisement>,
    pending: HashMap<EndpointId, PeerChange>,
    order: VecDeque<EndpointId>,
    queued: HashSet<EndpointId>,
    stale_order_entries: usize,
}

enum WatcherTarget<'a> {
    Matching(&'a SignedAdvertisement),
    Unmatched(&'a SignedAdvertisement),
    Expired(&'a SignedAdvertisement),
}

impl WatcherQueue {
    fn update(&self, target: WatcherTarget<'_>) -> bool {
        let peer = match target {
            WatcherTarget::Matching(record)
            | WatcherTarget::Unmatched(record)
            | WatcherTarget::Expired(record) => record.node_id(),
        };
        let mut state = self.lock();
        let change = match (state.delivered.get(&peer), target) {
            (None, WatcherTarget::Matching(record)) => Some(PeerChange::Joined(record.clone())),
            (Some(previous), WatcherTarget::Matching(record))
                if !record.same_revision_content(previous) =>
            {
                Some(PeerChange::Updated(record.clone()))
            }
            (Some(_), WatcherTarget::Unmatched(record)) => {
                Some(PeerChange::Unmatched(record.clone()))
            }
            (Some(_), WatcherTarget::Expired(record)) => Some(PeerChange::Expired(record.clone())),
            _ => None,
        };

        match change {
            Some(change) => {
                let replaced = state.pending.insert(peer, change).is_some();
                if state.queued.insert(peer) {
                    state.order.push_back(peer);
                } else if !replaced {
                    state.stale_order_entries = state.stale_order_entries.saturating_sub(1);
                }
            }
            None => {
                if state.pending.remove(&peer).is_some() {
                    state.stale_order_entries += 1;
                }
            }
        }
        state.compact_order_if_needed();
        true
    }

    fn pop(&self) -> Option<PeerChange> {
        let mut state = self.lock();
        while let Some(peer) = state.order.pop_front() {
            state.queued.remove(&peer);
            let Some(change) = state.pending.remove(&peer) else {
                state.stale_order_entries = state.stale_order_entries.saturating_sub(1);
                continue;
            };
            match &change {
                PeerChange::Joined(record) | PeerChange::Updated(record) => {
                    state.delivered.insert(peer, record.clone());
                }
                PeerChange::Expired(_) | PeerChange::Unmatched(_) => {
                    state.delivered.remove(&peer);
                }
            }
            return Some(change);
        }
        None
    }

    fn lock(&self) -> StdMutexGuard<'_, WatcherQueueState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl WatcherQueueState {
    fn compact_order_if_needed(&mut self) {
        if self.stale_order_entries < 64
            || self.stale_order_entries.saturating_mul(2) < self.order.len()
        {
            return;
        }
        let pending = self.pending.keys().copied().collect::<HashSet<_>>();
        self.order.retain(|peer| pending.contains(peer));
        self.queued = pending;
        self.stale_order_entries = 0;
    }
}

impl ClusterView {
    pub(crate) async fn watch(&self, own_id: EndpointId, query: Query) -> PeerWatcher {
        let queue = std::sync::Arc::new(WatcherQueue::default());
        let (changed, receiver) = watch::channel(0_u64);
        let mut state = self.state.lock().await;
        for (peer, presence) in &state.peers {
            if *peer != own_id && query.matches(&presence.record) {
                queue.update(WatcherTarget::Matching(&presence.record));
            }
        }
        state.watchers.push(WatcherState {
            query,
            own_id,
            queue: queue.clone(),
            changed,
        });
        PeerWatcher {
            queue,
            changed: receiver,
        }
    }

    pub(crate) async fn ingest(
        &self,
        record: SignedAdvertisement,
    ) -> Result<CatalogIngest, NetworkError> {
        let mut state = self.state.lock().await;
        let kind = match state.peers.get_mut(&record.node_id()) {
            None => PresenceKind::Joined,
            Some(previous)
                if record.advertisement().revision()
                    < previous.record.advertisement().revision() =>
            {
                return Ok(CatalogIngest::Stale);
            }
            Some(previous)
                if record.advertisement().revision()
                    == previous.record.advertisement().revision() =>
            {
                if !record.same_revision_content(&previous.record) {
                    return Err(NetworkError::InvalidAdvertisement(
                        "one revision cannot describe different capability content".to_owned(),
                    ));
                }
                if record.expires_at_unix_millis() > previous.record.expires_at_unix_millis() {
                    previous.record = record;
                    return Ok(CatalogIngest::Applied);
                }
                return Ok(CatalogIngest::Replay);
            }
            Some(_) => PresenceKind::Updated,
        };
        state.peers.insert(
            record.node_id(),
            PeerPresence {
                record: record.clone(),
            },
        );
        state.notify_watchers(PresenceChange { kind, record });
        Ok(CatalogIngest::Applied)
    }

    pub(crate) async fn expire(&self) {
        let now = match unix_millis() {
            Ok(now) => now,
            Err(error) => {
                tracing::warn!(%error, "could not expire capability advertisements");
                return;
            }
        };
        let mut state = self.state.lock().await;
        let expired = state
            .peers
            .values()
            .filter(|presence| presence.record.is_expired_at(now))
            .map(|presence| presence.record.clone())
            .collect::<Vec<_>>();
        for record in expired {
            state.peers.remove(&record.node_id());
            state.notify_watchers(PresenceChange {
                kind: PresenceKind::Expired,
                record,
            });
        }
    }

    pub(crate) async fn active_records(&self) -> Vec<SignedAdvertisement> {
        self.state
            .lock()
            .await
            .peers
            .values()
            .map(|presence| presence.record.clone())
            .collect()
    }
}

impl ViewState {
    fn notify_watchers(&mut self, change: PresenceChange) {
        let peer = change.record.node_id();
        self.watchers.retain_mut(|watcher| {
            if watcher.changed.receiver_count() == 0 {
                return false;
            }
            if peer == watcher.own_id {
                return true;
            }
            let target = match change.kind {
                PresenceKind::Joined | PresenceKind::Updated
                    if watcher.query.matches(&change.record) =>
                {
                    WatcherTarget::Matching(&change.record)
                }
                PresenceKind::Joined | PresenceKind::Updated => {
                    WatcherTarget::Unmatched(&change.record)
                }
                PresenceKind::Expired => WatcherTarget::Expired(&change.record),
            };
            watcher.queue.update(target);
            watcher
                .changed
                .send_modify(|revision| *revision = revision.wrapping_add(1));
            true
        });
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[tokio::test]
    async fn catalog_merges_authenticated_records_from_independent_sources() {
        let observer = SecretKey::from_bytes(&[0x31; 32]).public();
        let provider = SecretKey::from_bytes(&[0x32; 32]);
        let protocol = ProtocolId::new("nanocodex.egress.http/1").unwrap();
        let catalog = PeerCatalog::new(observer, Arc::new(ClusterView::default()));
        let mut watcher = catalog.watch(Query::service(protocol.clone())).await;
        let record = SignedAdvertisement::sign(
            NodeAdvertisement::new(1)
                .with_service(protocol.clone())
                .with_attribute("gateway.internet", true),
            &provider,
        )
        .unwrap();

        assert_eq!(
            catalog.ingest(record.clone()).await.unwrap(),
            CatalogIngest::Applied
        );
        assert!(matches!(
            watcher.next().await,
            Some(PeerChange::Joined(joined)) if joined == record
        ));
        assert_eq!(
            catalog.ingest(record.clone()).await.unwrap(),
            CatalogIngest::Replay
        );
        assert_eq!(
            catalog.snapshot(&Query::service(protocol)).await,
            vec![record.clone()]
        );

        let mut encoded = serde_json::to_value(&record).unwrap();
        encoded["advertisement"]["revision"] = serde_json::json!(2);
        let forged: SignedAdvertisement = serde_json::from_value(encoded).unwrap();
        assert!(catalog.ingest(forged).await.is_err());
    }

    #[tokio::test]
    async fn initial_watch_snapshot_does_not_drop_large_fleets() {
        const PEERS: usize = 320;

        let view = ClusterView::default();
        let protocol = ProtocolId::new("nanocodex.worker/1").unwrap();
        for index in 0..PEERS {
            let mut key = [0_u8; 32];
            key[..8].copy_from_slice(&(index as u64 + 1).to_le_bytes());
            let identity = SecretKey::from_bytes(&key);
            let record = SignedAdvertisement::sign(
                NodeAdvertisement::new(1).with_service(protocol.clone()),
                &identity,
            )
            .unwrap();
            assert_eq!(view.ingest(record).await.unwrap(), CatalogIngest::Applied);
        }

        let observer = SecretKey::from_bytes(&[0xff; 32]).public();
        let mut watcher = view.watch(observer, Query::service(protocol)).await;
        let mut joined = HashSet::with_capacity(PEERS);
        tokio::time::timeout(Duration::from_secs(2), async {
            while joined.len() < PEERS {
                let PeerChange::Joined(record) = watcher.next().await.unwrap() else {
                    panic!("initial snapshot must contain only joins");
                };
                joined.insert(record.node_id());
            }
        })
        .await
        .expect("initial snapshot silently dropped peers");
    }

    #[tokio::test]
    async fn slow_watchers_coalesce_each_identity_to_its_latest_state() {
        const LAST_REVISION: u64 = 1_024;

        let view = ClusterView::default();
        let protocol = ProtocolId::new("nanocodex.worker/1").unwrap();
        let identity = SecretKey::from_bytes(&[0x41; 32]);
        let observer = SecretKey::from_bytes(&[0x42; 32]).public();
        let first = SignedAdvertisement::sign(
            NodeAdvertisement::new(1).with_service(protocol.clone()),
            &identity,
        )
        .unwrap();
        view.ingest(first).await.unwrap();
        let mut watcher = view.watch(observer, Query::service(protocol.clone())).await;

        for revision in 2..=LAST_REVISION {
            let record = SignedAdvertisement::sign(
                NodeAdvertisement::new(revision)
                    .with_service(protocol.clone())
                    .with_attribute("worker.free_slots", revision),
                &identity,
            )
            .unwrap();
            view.ingest(record).await.unwrap();
        }
        assert!(matches!(
            watcher.next().await,
            Some(PeerChange::Joined(record))
                if record.advertisement().revision() == LAST_REVISION
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), watcher.next())
                .await
                .is_err(),
            "coalesced revisions must not leave stale events"
        );

        let unmatched =
            SignedAdvertisement::sign(NodeAdvertisement::new(LAST_REVISION + 1), &identity)
                .unwrap();
        view.ingest(unmatched).await.unwrap();
        let matching_again = SignedAdvertisement::sign(
            NodeAdvertisement::new(LAST_REVISION + 2).with_service(protocol),
            &identity,
        )
        .unwrap();
        view.ingest(matching_again).await.unwrap();
        assert!(matches!(
            watcher.next().await,
            Some(PeerChange::Updated(record))
                if record.advertisement().revision() == LAST_REVISION + 2
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), watcher.next())
                .await
                .is_err(),
            "a transient unmatch must collapse into the latest matching state"
        );
    }
}
