use std::{io::Write as _, net::SocketAddr, path::PathBuf, str::FromStr as _};

use clap::{Args, Subcommand};
use eyre::{Result, WrapErr as _, eyre};
use nanocodex_network::{Hub, JoinAuthority, JoinTicket, Node, NodeIdentity, TcpBridge};
use tokio::net::TcpListener;

#[derive(Args)]
pub(crate) struct Network {
    #[command(subcommand)]
    command: NetworkCommand,
}

#[derive(Subcommand)]
enum NetworkCommand {
    /// Publish one loopback TCP service into a Nanocodex network.
    Publish(Publish),
    /// Join a Nanocodex network and expose its published service on loopback.
    Connect(Connect),
}

#[derive(Args)]
struct Publish {
    /// Fixed loopback TCP service made available to joined nodes.
    #[arg(long, value_name = "HOST:PORT")]
    target: SocketAddr,

    /// Durable network authority state.
    #[arg(long, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,
}

#[derive(Args)]
struct Connect {
    /// Opaque capability printed by `nanocodex network publish`.
    ticket: String,

    /// Node-local listen port. Use zero to allocate an available port.
    #[arg(long, default_value_t = 8789)]
    port: u16,

    /// Durable node identity state.
    #[arg(long, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,
}

impl Network {
    pub(crate) async fn run(self) -> Result<()> {
        match self.command {
            NetworkCommand::Publish(command) => command.run().await,
            NetworkCommand::Connect(command) => command.run().await,
        }
    }
}

impl Publish {
    async fn run(self) -> Result<()> {
        let state = self.state_dir.map_or_else(default_state_dir, Ok)?;
        let authority = JoinAuthority::load_or_create(state.join("authority.json"))
            .wrap_err("failed to load the durable network authority")?;
        let (hub, ticket) = Hub::bind(&authority)
            .await
            .wrap_err("failed to start the Iroh network hub")?;
        TcpBridge::publish(&hub, self.target)
            .await
            .wrap_err("failed to publish the loopback TCP service")?;
        eprintln!("network hub identity: {}", authority.endpoint_id());
        eprintln!("published TCP target: {}", self.target);
        println!("{ticket}");
        std::io::stdout()
            .flush()
            .wrap_err("failed to flush the network join ticket")?;
        tokio::signal::ctrl_c()
            .await
            .wrap_err("failed to listen for network hub shutdown")?;
        hub.shutdown()
            .await
            .wrap_err("failed to stop the Iroh network hub")
    }
}

impl Connect {
    async fn run(self) -> Result<()> {
        let ticket = JoinTicket::from_str(&self.ticket)?;
        let state = self.state_dir.map_or_else(default_state_dir, Ok)?;
        let identity = NodeIdentity::load_or_create(state.join("node.json"))
            .wrap_err("failed to load the durable network node identity")?;
        let node = Node::join(ticket, &identity)
            .await
            .wrap_err("failed to join the Iroh network")?;
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, self.port))
            .await
            .wrap_err("failed to bind the node-local TCP bridge")?;
        let address = listener.local_addr()?;
        eprintln!("network node identity: {}", node.endpoint_id());
        println!("tcp://{address}");
        std::io::stdout()
            .flush()
            .wrap_err("failed to flush the node-local TCP address")?;
        let bridge = tokio::select! {
            result = TcpBridge::connect(&node, listener) => result,
            signal = tokio::signal::ctrl_c() => {
                signal.wrap_err("failed to listen for network node shutdown")?;
                Ok(())
            }
        };
        let shutdown = node
            .shutdown()
            .await
            .wrap_err("failed to stop the Iroh network node");
        bridge.wrap_err("network TCP bridge stopped")?;
        shutdown
    }
}

fn default_state_dir() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("NANOCODEX_HOME").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(home).join("network"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| eyre!("HOME is not set; pass --state-dir for durable network state"))?;
    Ok(PathBuf::from(home).join(".nanocodex/network"))
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use crate::Cli;

    #[test]
    fn network_surface_is_independent_from_eval() {
        for arguments in [
            vec![
                "nanocodex",
                "network",
                "publish",
                "--target",
                "127.0.0.1:8789",
            ],
            vec![
                "nanocodex",
                "network",
                "connect",
                "nanocodex-net:ticket",
                "--port",
                "0",
            ],
        ] {
            Cli::try_parse_from(arguments).expect("supported network command must parse");
        }
    }
}
