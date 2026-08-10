use std::{net::Ipv4Addr, path::PathBuf, str::FromStr as _};

use clap::Args;
use eyre::{Result, WrapErr as _};
use nanocodex_network::{JoinTicket, NodeConnector, NodeIdentity};
use tokio::net::TcpListener;

use super::profile::default_state_dir;

#[derive(Args)]
pub(super) struct Connect {
    /// Opaque capability printed by `eval coordinator --iroh`.
    ticket: String,

    /// Worker-local listen port. Use zero to allocate an available port.
    #[arg(long, default_value_t = 8789)]
    port: u16,

    /// Durable Iroh node identity and local connector state.
    #[arg(long, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,
}

impl Connect {
    pub(super) async fn run(self) -> Result<()> {
        let ticket = JoinTicket::from_str(&self.ticket)?;
        let state = self.state_dir.map_or_else(default_state_dir, Ok)?;
        let identity = NodeIdentity::load_or_create(state.join("iroh/node.json"))
            .wrap_err("failed to load the durable iroh node identity")?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, self.port))
            .await
            .wrap_err("failed to bind the node-local evaluation coordinator bridge")?;
        let address = listener.local_addr()?;
        eprintln!("iroh node identity: {}", identity.endpoint_id());
        println!("http://{address}");
        NodeConnector::serve(ticket, &identity, listener)
            .await
            .wrap_err("iroh evaluation coordinator bridge stopped")
    }
}
