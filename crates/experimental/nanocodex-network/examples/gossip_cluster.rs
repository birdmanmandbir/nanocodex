use std::{future, path::Path, time::Duration};

use nanocodex_network::{
    Hub, JoinAuthority, JoinTicket, Node, NodeAdvertisement, NodeIdentity, PeerChange, ProtocolId,
    Query,
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const WORKER_PROTOCOL: &str = "nanocodex.worker/1";

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
        _ => Err(usage().into()),
    }
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
    "usage: gossip_cluster hub AUTHORITY_PATH | serve JOIN_TICKET IDENTITY_PATH [CPU_ARCH] | dial JOIN_TICKET IDENTITY_PATH [CPU_ARCH]"
}
