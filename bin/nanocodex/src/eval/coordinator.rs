use std::{net::Ipv4Addr, path::PathBuf};

use clap::Args;
use eyre::{Result, WrapErr as _};
use nanocodex_eval::{
    Evaluation,
    coordinator::{CoordinatorServer, IrohCoordinatorIdentity, IrohCoordinatorServer},
};
use tokio::net::TcpListener;

use super::profile::default_state_dir;

#[derive(Args)]
pub(super) struct Coordinator {
    /// Named benchmark stored in SQLite.
    profile: String,

    /// Runtime harness helper configuration. SQLite owns desired work.
    #[arg(long, default_value = "nanocodex.toml")]
    config: PathBuf,

    /// Durable SQLite ledger and retained coordinator artifacts.
    #[arg(long, value_name = "DIRECTORY")]
    state_dir: Option<PathBuf>,

    /// Listen port. Use zero to allocate an available port.
    #[arg(long, default_value_t = 8789)]
    port: u16,

    /// Publish the loopback coordinator through an authenticated iroh endpoint.
    #[arg(long)]
    iroh: bool,
}

impl Coordinator {
    pub(super) async fn run(self) -> Result<()> {
        let state = self.state_dir.map_or_else(default_state_dir, Ok)?;
        let evaluation = Evaluation::open(&self.config, Some(&self.profile), state.clone())?;
        let identity = self
            .iroh
            .then(|| IrohCoordinatorIdentity::load_or_create(&state))
            .transpose()
            .wrap_err("failed to load the durable iroh coordinator identity")?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, self.port))
            .await
            .wrap_err("failed to bind the evaluation coordinator")?;
        let address = listener.local_addr()?;
        let iroh = if let Some(identity) = identity.as_ref() {
            let (server, ticket) = IrohCoordinatorServer::bind(address, identity)
                .await
                .wrap_err("failed to publish the evaluation coordinator over iroh")?;
            eprintln!("local coordinator: http://{address}");
            println!("{ticket}");
            Some(server)
        } else {
            println!("http://{address}");
            None
        };
        let result = CoordinatorServer::new(evaluation)
            .serve(listener)
            .await
            .wrap_err("evaluation coordinator stopped");
        if let Some(iroh) = iroh {
            iroh.shutdown()
                .await
                .wrap_err("failed to stop the iroh coordinator endpoint")?;
        }
        result
    }
}
