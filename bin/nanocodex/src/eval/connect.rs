use std::{net::Ipv4Addr, str::FromStr as _};

use clap::Args;
use eyre::{Result, WrapErr as _};
use nanocodex_eval::coordinator::{IrohCoordinatorConnector, IrohCoordinatorTicket};
use tokio::net::TcpListener;

#[derive(Args)]
pub(super) struct Connect {
    /// Opaque capability printed by `eval coordinator --iroh`.
    ticket: String,

    /// Worker-local listen port. Use zero to allocate an available port.
    #[arg(long, default_value_t = 8789)]
    port: u16,
}

impl Connect {
    pub(super) async fn run(self) -> Result<()> {
        let ticket = IrohCoordinatorTicket::from_str(&self.ticket)?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, self.port))
            .await
            .wrap_err("failed to bind the worker-local evaluation coordinator bridge")?;
        let address = listener.local_addr()?;
        println!("http://{address}");
        IrohCoordinatorConnector::serve(ticket, listener)
            .await
            .wrap_err("iroh evaluation coordinator bridge stopped")
    }
}
