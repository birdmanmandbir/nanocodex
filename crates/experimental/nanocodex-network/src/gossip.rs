use std::{future, sync::Arc, time::Duration};

use futures_util::StreamExt as _;
use iroh::EndpointId;
use iroh_gossip::{
    Gossip, TopicId,
    api::{Event, GossipReceiver, GossipSender},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_util::task::AbortOnDropHandle;

use crate::{
    NetworkError, SignedAdvertisement,
    discovery::{ClusterView, IngestOutcome},
};

const MESSAGE_VERSION: u8 = 1;
const TOPIC_DOMAIN: &[u8] = b"nanocodex-network-discovery-topic\0";
const MAX_GOSSIP_MESSAGE_BYTES: usize = 16 * 1024;
const COMMAND_CAPACITY: usize = 128;
const REAP_INTERVAL: Duration = Duration::from_millis(50);
const RESUBSCRIBE_DELAY: Duration = Duration::from_millis(100);
const FULL_SNAPSHOT_COALESCE: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub(crate) struct GossipPublisher {
    commands: mpsc::Sender<Command>,
}

pub(crate) struct GossipDiscovery {
    publisher: GossipPublisher,
    _task: AbortOnDropHandle<()>,
}

#[derive(Clone, Copy)]
pub(crate) enum SnapshotPolicy {
    All,
    Own(EndpointId),
}

enum Command {
    Publish {
        record: SignedAdvertisement,
        accepted: oneshot::Sender<Result<(), String>>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum DiscoveryMessage {
    Advertisement {
        version: u8,
        record: SignedAdvertisement,
    },
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum DiscoveryMessageRef<'a> {
    Advertisement {
        version: u8,
        record: &'a SignedAdvertisement,
    },
}

impl GossipDiscovery {
    pub(crate) async fn spawn(
        gossip: Gossip,
        topic: TopicId,
        bootstrap: Vec<EndpointId>,
        view: Arc<ClusterView>,
        snapshot_policy: SnapshotPolicy,
    ) -> Result<Self, NetworkError> {
        let subscription = gossip
            .subscribe(topic, bootstrap.clone())
            .await
            .map_err(gossip_error)?;
        let (commands, receiver) = mpsc::channel(COMMAND_CAPACITY);
        let publisher = GossipPublisher { commands };
        let task = AbortOnDropHandle::new(tokio::spawn(run(
            gossip,
            topic,
            bootstrap,
            view,
            snapshot_policy,
            receiver,
            Some(subscription.split()),
        )));
        Ok(Self {
            publisher,
            _task: task,
        })
    }

    pub(crate) fn publisher(&self) -> GossipPublisher {
        self.publisher.clone()
    }
}

impl GossipPublisher {
    pub(crate) async fn publish(&self, record: SignedAdvertisement) -> Result<(), NetworkError> {
        let (accepted, response) = oneshot::channel();
        self.commands
            .send(Command::Publish { record, accepted })
            .await
            .map_err(|_| NetworkError::Protocol("gossip discovery task stopped".to_owned()))?;
        response
            .await
            .map_err(|_| NetworkError::Protocol("gossip discovery task stopped".to_owned()))?
            .map_err(NetworkError::Protocol)
    }
}

async fn run(
    gossip: Gossip,
    topic: TopicId,
    bootstrap: Vec<EndpointId>,
    view: Arc<ClusterView>,
    snapshot_policy: SnapshotPolicy,
    mut commands: mpsc::Receiver<Command>,
    mut initial: Option<(GossipSender, GossipReceiver)>,
) {
    loop {
        let subscription = match initial.take() {
            Some(subscription) => subscription,
            None => match gossip.subscribe(topic, bootstrap.clone()).await {
                Ok(subscription) => subscription.split(),
                Err(error) => {
                    tracing::warn!(%error, "could not rejoin capability gossip topic");
                    tokio::time::sleep(RESUBSCRIBE_DELAY).await;
                    continue;
                }
            },
        };
        let (sender, mut receiver) = subscription;
        let mut reaper = tokio::time::interval(REAP_INTERVAL);
        reaper.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut full_snapshot_at = None;
        let resubscribe = loop {
            tokio::select! {
                command = commands.recv() => {
                    let Some(command) = command else {
                        return;
                    };
                    handle_command(command, &sender, &view).await;
                }
                event = receiver.next() => {
                    match event {
                        Some(Ok(Event::Received(message))) => {
                            if let Err(error) = receive(&message.content, &view).await {
                                tracing::debug!(%error, delivered_from = %message.delivered_from, "rejected capability gossip message");
                            }
                        }
                        Some(Ok(Event::NeighborUp(peer))) => {
                            match snapshot_policy {
                                SnapshotPolicy::All => {
                                    full_snapshot_at.get_or_insert_with(|| {
                                        tokio::time::Instant::now() + FULL_SNAPSHOT_COALESCE
                                    });
                                }
                                SnapshotPolicy::Own(_) => {
                                    if let Err(error) = broadcast_snapshot(&sender, &view, snapshot_policy).await {
                                        tracing::debug!(%peer, %error, "could not send capability anti-entropy snapshot");
                                    }
                                }
                            }
                        }
                        Some(Ok(Event::NeighborDown(peer))) => {
                            tracing::debug!(%peer, "capability gossip neighbor disconnected");
                        }
                        Some(Ok(Event::Lagged)) => {
                            tracing::warn!("capability gossip receiver lagged; rejoining topic");
                            break true;
                        }
                        Some(Err(error)) => {
                            tracing::warn!(%error, "capability gossip receiver failed; rejoining topic");
                            break true;
                        }
                        None => break true,
                    }
                }
                _ = reaper.tick() => {
                    view.expire().await;
                }
                _ = wait_until(full_snapshot_at), if full_snapshot_at.is_some() => {
                    full_snapshot_at = None;
                    if let Err(error) = broadcast_snapshot(&sender, &view, SnapshotPolicy::All).await {
                        tracing::debug!(%error, "could not send coalesced capability anti-entropy snapshot");
                    }
                }
            }
        };
        if resubscribe {
            tokio::time::sleep(RESUBSCRIBE_DELAY).await;
        }
    }
}

async fn wait_until(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => future::pending().await,
    }
}

async fn handle_command(command: Command, sender: &GossipSender, view: &ClusterView) {
    match command {
        Command::Publish { record, accepted } => {
            let result = async {
                record.verify(record.node_id())?;
                let message = encode(&record)?;
                match view.ingest(record).await? {
                    IngestOutcome::Broadcast => {}
                    IngestOutcome::Replay => return Ok(()),
                    IngestOutcome::Stale => {
                        return Err(NetworkError::InvalidAdvertisement(
                            "advertisement revision is older than the local active revision"
                                .to_owned(),
                        ));
                    }
                }
                sender
                    .broadcast(message.into())
                    .await
                    .map_err(gossip_error)?;
                Ok(())
            }
            .await;
            let _ = accepted.send(result.map_err(|error: NetworkError| error.to_string()));
        }
    }
}

async fn receive(encoded: &[u8], view: &ClusterView) -> Result<(), NetworkError> {
    if encoded.is_empty() || encoded.len() > MAX_GOSSIP_MESSAGE_BYTES {
        return Err(NetworkError::InvalidAdvertisement(
            "gossip advertisement has an invalid encoded length".to_owned(),
        ));
    }
    let DiscoveryMessage::Advertisement { version, record } = serde_json::from_slice(encoded)
        .map_err(|error| NetworkError::InvalidAdvertisement(error.to_string()))?;
    if version != MESSAGE_VERSION {
        return Err(NetworkError::InvalidAdvertisement(format!(
            "unsupported gossip message version {version}"
        )));
    }
    record.verify(record.node_id())?;
    let _ = view.ingest(record).await?;
    Ok(())
}

async fn broadcast_snapshot(
    sender: &GossipSender,
    view: &ClusterView,
    policy: SnapshotPolicy,
) -> Result<(), NetworkError> {
    for record in view.active_records().await {
        if matches!(policy, SnapshotPolicy::Own(owner) if record.node_id() != owner) {
            continue;
        }
        sender
            .broadcast_neighbors(encode(&record)?.into())
            .await
            .map_err(gossip_error)?;
    }
    Ok(())
}

fn encode(record: &SignedAdvertisement) -> Result<Vec<u8>, NetworkError> {
    let encoded = serde_json::to_vec(&DiscoveryMessageRef::Advertisement {
        version: MESSAGE_VERSION,
        record,
    })
    .map_err(|error| NetworkError::InvalidAdvertisement(error.to_string()))?;
    if encoded.len() > MAX_GOSSIP_MESSAGE_BYTES {
        return Err(NetworkError::InvalidAdvertisement(format!(
            "gossip advertisement exceeds {MAX_GOSSIP_MESSAGE_BYTES} encoded bytes"
        )));
    }
    Ok(encoded)
}

pub(crate) fn topic_id(hub: EndpointId, token: &[u8; 32]) -> TopicId {
    let mut hasher = Sha256::new();
    hasher.update(TOPIC_DOMAIN);
    hasher.update(hub.as_bytes());
    hasher.update(token);
    TopicId::from_bytes(hasher.finalize().into())
}

fn gossip_error(error: impl std::fmt::Display) -> NetworkError {
    NetworkError::Protocol(format!("gossip discovery failed: {error}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::{CapabilityValue, NodeAdvertisement, ProtocolId};

    #[test]
    fn topic_is_cluster_specific_and_stable() {
        let first = iroh::SecretKey::from_bytes(&[1; 32]).public();
        let second = iroh::SecretKey::from_bytes(&[2; 32]).public();
        let token = [3; 32];

        assert_eq!(topic_id(first, &token), topic_id(first, &token));
        assert_ne!(topic_id(first, &token), topic_id(second, &token));
        assert_ne!(topic_id(first, &token), topic_id(first, &[4; 32]));
    }

    #[test]
    fn encoded_gossip_advertisements_are_bounded() {
        let identity = iroh::SecretKey::from_bytes(&[5; 32]);
        let artifacts = (0..128)
            .map(|index| format!("sha256:{index:04}-{}", "a".repeat(256)))
            .collect::<BTreeSet<_>>();
        let record = SignedAdvertisement::sign(
            NodeAdvertisement::new(1)
                .with_service(ProtocolId::new("nanocodex.artifacts/1").unwrap())
                .with_attribute("artifacts", CapabilityValue::StringSet(artifacts)),
            &identity,
        )
        .unwrap();

        assert!(matches!(
            encode(&record),
            Err(NetworkError::InvalidAdvertisement(message))
                if message.contains("exceeds")
        ));
    }

    #[tokio::test]
    async fn gossip_boundary_rejects_malformed_and_unsupported_messages() {
        let view = ClusterView::default();
        for malformed in [b"".as_slice(), b"not-json", b"{}"] {
            assert!(receive(malformed, &view).await.is_err());
        }

        let identity = iroh::SecretKey::from_bytes(&[6; 32]);
        let record = SignedAdvertisement::sign(
            NodeAdvertisement::new(1).with_service(ProtocolId::new("nanocodex.worker/1").unwrap()),
            &identity,
        )
        .unwrap();
        let mut unsupported = serde_json::to_value(DiscoveryMessageRef::Advertisement {
            version: MESSAGE_VERSION,
            record: &record,
        })
        .unwrap();
        unsupported["version"] = serde_json::json!(MESSAGE_VERSION + 1);
        assert!(
            receive(&serde_json::to_vec(&unsupported).unwrap(), &view)
                .await
                .is_err()
        );

        let mut unknown_field = unsupported;
        unknown_field["version"] = serde_json::json!(MESSAGE_VERSION);
        unknown_field["unexpected"] = serde_json::json!(true);
        assert!(
            receive(&serde_json::to_vec(&unknown_field).unwrap(), &view)
                .await
                .is_err()
        );
    }
}
