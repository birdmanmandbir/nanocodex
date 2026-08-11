use std::{collections::HashSet, future, path::Path, time::Duration};

use futures_util::{StreamExt as _, TryStreamExt as _, stream};
use nanocodex_network::{
    Hub, JoinAuthority, JoinTicket, Node, NodeAdvertisement, NodeIdentity, PeerChange, ProtocolId,
    Query,
};
use serde::Serialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const WORKER_PROTOCOL: &str = "nanocodex.worker/1";
const PROBE_DIAL_CONCURRENCY: usize = 32;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args().skip(1);
    match arguments.next().as_deref() {
        Some("hub") => {
            let authority = required(&mut arguments, "authority path")?;
            require_end(arguments)?;
            run_hub(Path::new(&authority)).await
        }
        Some("serve") => {
            let ticket = required(&mut arguments, "join ticket")?.parse()?;
            let identity = required(&mut arguments, "identity path")?;
            let architecture = arguments.next().unwrap_or_else(|| "unknown".to_owned());
            require_end(arguments)?;
            serve(ticket, Path::new(&identity), architecture).await
        }
        Some("dial") => {
            let ticket = required(&mut arguments, "join ticket")?.parse()?;
            let identity = required(&mut arguments, "identity path")?;
            let architecture = arguments.next().unwrap_or_else(|| "unknown".to_owned());
            require_end(arguments)?;
            dial(ticket, Path::new(&identity), architecture).await
        }
        Some("probe") => {
            let ticket = required(&mut arguments, "join ticket")?.parse()?;
            let identity = required(&mut arguments, "identity path")?;
            let architecture = required(&mut arguments, "CPU architecture")?;
            let workers = required(&mut arguments, "expected worker count")?.parse()?;
            require_end(arguments)?;
            probe(ticket, Path::new(&identity), architecture, workers).await
        }
        _ => Err(usage().into()),
    }
}

#[derive(Serialize)]
struct ProbeResult {
    workers: usize,
    discovery_millis: u128,
    dial_millis: u128,
    dial_p50_millis: u128,
    dial_p95_millis: u128,
    dial_max_millis: u128,
}

async fn run_hub(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let authority = JoinAuthority::load_or_create(path)?;
    let (_hub, ticket) = Hub::bind(&authority).await?;
    println!("{ticket}");
    future::pending().await
}

async fn serve(
    ticket: JoinTicket,
    identity_path: &Path,
    architecture: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity = NodeIdentity::load_or_create(identity_path)?;
    let node = Node::join(ticket, &identity).await?;
    let protocol = ProtocolId::new(WORKER_PROTOCOL)?;
    let mut listener = node.listen(protocol.clone()).await?;
    let _lease = node
        .advertise(
            NodeAdvertisement::new(1)
                .with_service(protocol)
                .with_attribute("cpu.arch", architecture)
                .with_attribute("worker.free_slots", 1_u64)
                .lease_for(Duration::from_secs(10)),
        )
        .await?;
    println!("{}", node.endpoint_id());
    while let Some(mut stream) = listener.accept().await {
        let mut request = [0; 4];
        stream.read_exact(&mut request).await?;
        if &request != b"ping" {
            return Err("received an unexpected direct-stream payload".into());
        }
        stream.write_all(b"pong").await?;
        stream.shutdown().await?;
    }
    Ok(())
}

async fn dial(
    ticket: JoinTicket,
    identity_path: &Path,
    architecture: String,
) -> Result<(), Box<dyn std::error::Error>> {
    let identity = NodeIdentity::load_or_create(identity_path)?;
    let node = Node::join(ticket, &identity).await?;
    let protocol = ProtocolId::new(WORKER_PROTOCOL)?;
    let query = Query::service(protocol.clone())
        .attribute_eq("cpu.arch", architecture)?
        .attribute_at_least("worker.free_slots", 1)?;
    let mut workers = node.watch(query).await;
    let provider = loop {
        match workers.next().await {
            Some(PeerChange::Joined(record) | PeerChange::Updated(record)) => {
                break record.node_id();
            }
            Some(PeerChange::Expired(_) | PeerChange::Unmatched(_)) => {}
            None => return Err("cluster-view watcher closed".into()),
        }
    };
    let mut stream = node.connect(provider, &protocol).await?;
    stream.write_all(b"ping").await?;
    let mut response = [0; 4];
    stream.read_exact(&mut response).await?;
    if &response != b"pong" {
        return Err("received an unexpected direct-stream response".into());
    }
    println!("provider={provider} response=pong");
    node.shutdown().await?;
    Ok(())
}

async fn probe(
    ticket: JoinTicket,
    identity_path: &Path,
    architecture: String,
    expected_workers: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    if expected_workers == 0 {
        return Err("expected worker count must be greater than zero".into());
    }
    let identity = NodeIdentity::load_or_create(identity_path)?;
    let node = Node::join(ticket, &identity).await?;
    let protocol = ProtocolId::new(WORKER_PROTOCOL)?;
    let query = Query::service(protocol.clone())
        .attribute_eq("cpu.arch", architecture)?
        .attribute_at_least("worker.free_slots", 1)?;
    let mut changes = node.watch(query).await;
    let discovery_started = std::time::Instant::now();
    let providers = tokio::time::timeout(Duration::from_secs(180), async {
        let mut providers = HashSet::with_capacity(expected_workers);
        while providers.len() < expected_workers {
            match changes.next().await {
                Some(PeerChange::Joined(record) | PeerChange::Updated(record)) => {
                    providers.insert(record.node_id());
                }
                Some(PeerChange::Expired(_) | PeerChange::Unmatched(_)) => {}
                None => return Err("cluster-view watcher closed"),
            }
        }
        Ok::<_, &str>(providers)
    })
    .await
    .map_err(|_| "timed out waiting for the expected worker fleet")??;
    let discovery_millis = discovery_started.elapsed().as_millis();

    let dial_started = std::time::Instant::now();
    let mut dial_latencies = stream::iter(providers.iter().copied())
        .map(|provider| ping(&node, provider, &protocol))
        .buffer_unordered(PROBE_DIAL_CONCURRENCY)
        .try_collect::<Vec<_>>()
        .await?;
    dial_latencies.sort_unstable();
    let result = ProbeResult {
        workers: providers.len(),
        discovery_millis,
        dial_millis: dial_started.elapsed().as_millis(),
        dial_p50_millis: percentile(&dial_latencies, 50),
        dial_p95_millis: percentile(&dial_latencies, 95),
        dial_max_millis: *dial_latencies.last().ok_or("probe completed no dials")?,
    };
    println!("{}", serde_json::to_string(&result)?);
    node.shutdown().await?;
    Ok(())
}

async fn ping(
    node: &Node,
    provider: iroh::EndpointId,
    protocol: &ProtocolId,
) -> Result<u128, Box<dyn std::error::Error>> {
    let started = std::time::Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let mut stream = node.connect(provider, protocol).await?;
        stream.write_all(b"ping").await?;
        let mut response = [0; 4];
        stream.read_exact(&mut response).await?;
        if &response != b"pong" {
            return Err::<(), Box<dyn std::error::Error>>(
                "received an unexpected direct-stream response".into(),
            );
        }
        Ok(())
    })
    .await
    .map_err(|_| format!("timed out dialing provider {provider}"))?;
    outcome.map_err(|error| format!("provider {provider}: {error}"))?;
    Ok(started.elapsed().as_millis())
}

fn percentile(sorted: &[u128], percentile: usize) -> u128 {
    debug_assert!(!sorted.is_empty());
    debug_assert!((1..=100).contains(&percentile));
    let rank = sorted
        .len()
        .saturating_mul(percentile)
        .div_ceil(100)
        .saturating_sub(1);
    sorted[rank]
}

fn required(
    arguments: &mut impl Iterator<Item = String>,
    label: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    arguments
        .next()
        .ok_or_else(|| format!("missing {label}; {}", usage()).into())
}

fn require_end(
    mut arguments: impl Iterator<Item = String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(argument) = arguments.next() {
        return Err(format!("unexpected argument {argument:?}; {}", usage()).into());
    }
    Ok(())
}

const fn usage() -> &'static str {
    "usage: gossip_cluster hub AUTHORITY_PATH | serve JOIN_TICKET IDENTITY_PATH [CPU_ARCH] | dial JOIN_TICKET IDENTITY_PATH [CPU_ARCH] | probe JOIN_TICKET IDENTITY_PATH CPU_ARCH EXPECTED_WORKERS"
}

#[cfg(test)]
mod tests {
    use super::percentile;

    #[test]
    fn percentile_uses_nearest_rank() {
        let values = (1..=100).collect::<Vec<_>>();
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 95);
        assert_eq!(percentile(&[7], 95), 7);
    }
}
