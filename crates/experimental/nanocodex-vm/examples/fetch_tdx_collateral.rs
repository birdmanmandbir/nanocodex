use std::{
    fs,
    io::{self, Read as _},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use dcap_qvl::{
    collateral::{CollateralClient, INTEL_PCS_URL},
    configs::RustCryptoConfig,
    verify::QuoteVerifier,
};

const MAX_QUOTE_BYTES: usize = 1024 * 1024;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let quote = read_bounded(Options::parse()?.quote)?;
    let client =
        CollateralClient::with_default_http(INTEL_PCS_URL)?.with_config::<RustCryptoConfig>();
    let collateral = client.fetch(&quote).await?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    QuoteVerifier::new_prod()
        .with_config::<RustCryptoConfig>()
        .verify(&quote, &collateral, now)?;
    serde_json::to_writer_pretty(io::stdout().lock(), &collateral)?;
    println!();
    Ok(())
}

fn read_bounded(path: PathBuf) -> Result<Vec<u8>, io::Error> {
    let mut quote = Vec::new();
    fs::File::open(path)?
        .take((MAX_QUOTE_BYTES + 1) as u64)
        .read_to_end(&mut quote)?;
    if quote.is_empty() || quote.len() > MAX_QUOTE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "TDX quote must contain between 1 byte and 1 MiB",
        ));
    }
    Ok(quote)
}

struct Options {
    quote: PathBuf,
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut arguments = std::env::args_os().skip(1);
        let quote = arguments.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: fetch_tdx_collateral RAW_QUOTE",
            )
        })?;
        if arguments.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: fetch_tdx_collateral RAW_QUOTE",
            ));
        }
        Ok(Self {
            quote: PathBuf::from(quote),
        })
    }
}
