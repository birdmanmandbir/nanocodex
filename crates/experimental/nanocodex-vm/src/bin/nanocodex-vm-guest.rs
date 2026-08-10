#[cfg(target_os = "linux")]
use std::{ffi::OsStr, io, path::PathBuf};

#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[cfg(target_os = "linux")]
const MAX_ATTESTATION_REQUEST_BYTES: usize = 64 * 1024;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    if first.as_deref() == Some(OsStr::new("--attest")) {
        if arguments.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--attest accepts a single JSON request on stdin and no arguments",
            )
            .into());
        }
        let mut request = Vec::new();
        tokio::io::stdin()
            .take((MAX_ATTESTATION_REQUEST_BYTES + 1) as u64)
            .read_to_end(&mut request)
            .await?;
        if request.len() > MAX_ATTESTATION_REQUEST_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "attestation request exceeds 64 KiB",
            )
            .into());
        }
        let request = serde_json::from_slice(&request)?;
        let bundle = nanocodex_vm::guest::collect_attestation(request).await?;
        let mut response = serde_json::to_vec(&bundle)?;
        response.push(b'\n');
        tokio::io::stdout().write_all(&response).await?;
        tokio::io::stdout().flush().await?;
        return Ok(());
    }
    if first.as_deref() == Some(OsStr::new("--overlay-root")) {
        let workspace = arguments.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "--overlay-root requires a guest workspace",
            )
        })?;
        let resolver = arguments.next().unwrap_or_default();
        if arguments.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--overlay-root accepts only WORKSPACE and optional RESOLVER",
            )
            .into());
        }
        let resolver = resolver.to_string_lossy();
        return nanocodex_vm::tools::serve_overlay_guest(
            PathBuf::from(workspace),
            (!resolver.is_empty()).then_some(resolver.as_ref()),
        )
        .await
        .map_err(Into::into);
    }

    let workspace = first.map_or_else(|| PathBuf::from("/workspace"), PathBuf::from);
    if arguments.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "guest runtime accepts only one workspace argument",
        )
        .into());
    }
    nanocodex_vm::tools::serve_guest(workspace)
        .await
        .map_err(Into::into)
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nanocodex-vm-guest must be built for a Linux guest target");
}
