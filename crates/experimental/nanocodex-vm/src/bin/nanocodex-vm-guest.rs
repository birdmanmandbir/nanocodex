#[cfg(target_os = "linux")]
use std::{
    ffi::{OsStr, OsString},
    io::{self, Read as _},
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use serde::Serialize;
#[cfg(target_os = "linux")]
use sha2::{Digest as _, Sha256};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[cfg(target_os = "linux")]
const MAX_ATTESTATION_REQUEST_BYTES: usize = 64 * 1024;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    if first.as_deref() == Some(OsStr::new("--attest-example")) {
        let options = ExampleOptions::parse(arguments)?;
        let output = collect_example(options).await?;
        let mut response = serde_json::to_vec_pretty(&output)?;
        response.push(b'\n');
        tokio::io::stdout().write_all(&response).await?;
        tokio::io::stdout().flush().await?;
        return Ok(());
    }
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

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum ExampleNvidia {
    Auto,
    Off,
    Single,
    Hgx8,
}

#[cfg(target_os = "linux")]
struct ExampleOptions {
    nonce: Option<[u8; 32]>,
    policy_id: String,
    manifest_digest: Option<[u8; 32]>,
    nvidia: ExampleNvidia,
}

#[cfg(target_os = "linux")]
impl ExampleOptions {
    fn parse(arguments: impl Iterator<Item = OsString>) -> Result<Self, io::Error> {
        let mut options = Self {
            nonce: None,
            policy_id: "nanocodex-attestation-example-v1".to_owned(),
            manifest_digest: None,
            nvidia: ExampleNvidia::Auto,
        };
        let mut arguments = arguments.peekable();
        while let Some(argument) = arguments.next() {
            let argument = argument.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "arguments must be UTF-8")
            })?;
            match argument {
                "--nonce-hex" => {
                    options.nonce = Some(parse_hex_32(next_value(&mut arguments, argument)?)?);
                }
                "--policy-id" => {
                    options.policy_id = next_value(&mut arguments, argument)?;
                }
                "--manifest-sha256" => {
                    options.manifest_digest =
                        Some(parse_hex_32(next_value(&mut arguments, argument)?)?);
                }
                "--nvidia" => {
                    options.nvidia = match next_value(&mut arguments, argument)?.as_str() {
                        "auto" => ExampleNvidia::Auto,
                        "off" => ExampleNvidia::Off,
                        "b200-single" => ExampleNvidia::Single,
                        "b200-hgx8" => ExampleNvidia::Hgx8,
                        value => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!(
                                    "invalid --nvidia value {value:?}; expected auto, off, b200-single, or b200-hgx8"
                                ),
                            ));
                        }
                    };
                }
                "--help" | "-h" => {
                    eprintln!(
                        "usage: nanocodex-vm-guest --attest-example [--nonce-hex 64_HEX] [--policy-id ID] [--manifest-sha256 64_HEX] [--nvidia auto|off|b200-single|b200-hgx8]"
                    );
                    std::process::exit(0);
                }
                value => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown --attest-example argument {value:?}"),
                    ));
                }
            }
        }
        Ok(options)
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExampleInputOrigin {
    GeneratedInsideGuest,
    RelyingParty,
    CurrentExecutableSha256,
    CallerProvided,
}

#[cfg(target_os = "linux")]
#[derive(Serialize)]
struct ExampleOutput {
    schema_version: u32,
    status: &'static str,
    nonce_origin: ExampleInputOrigin,
    manifest_origin: ExampleInputOrigin,
    key_proof_verified: bool,
    warning: &'static str,
    attestation: nanocodex_vm::guest::GuestAttestation,
}

#[cfg(target_os = "linux")]
async fn collect_example(
    options: ExampleOptions,
) -> Result<ExampleOutput, Box<dyn std::error::Error>> {
    use nanocodex_vm::guest::{
        AttestationChallenge, GuestAttestationIdentity, GuestAttestationParameters,
        NvidiaAttestationProfile, detect_cpu_attestation_profile,
        detect_nvidia_attestation_profile,
    };

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let (nonce, nonce_origin) = match options.nonce {
        Some(nonce) => (nonce, ExampleInputOrigin::RelyingParty),
        None => {
            let mut nonce = [0_u8; 32];
            getrandom::fill(&mut nonce)?;
            (nonce, ExampleInputOrigin::GeneratedInsideGuest)
        }
    };
    let (manifest_digest, manifest_origin) = match options.manifest_digest {
        Some(digest) => (digest, ExampleInputOrigin::CallerProvided),
        None => (
            current_executable_digest()?,
            ExampleInputOrigin::CurrentExecutableSha256,
        ),
    };
    let cpu_profile = detect_cpu_attestation_profile().await?;
    let nvidia_profile = match options.nvidia {
        ExampleNvidia::Auto => detect_nvidia_attestation_profile().await?,
        ExampleNvidia::Off => None,
        ExampleNvidia::Single => Some(NvidiaAttestationProfile::B200Single),
        ExampleNvidia::Hgx8 => Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink),
    };
    let challenge = AttestationChallenge::new(
        nonce,
        options.policy_id,
        now.checked_add(300)
            .ok_or_else(|| io::Error::other("attestation expiry overflow"))?,
    )?;
    let parameters =
        GuestAttestationParameters::new(challenge, manifest_digest, cpu_profile, nvidia_profile);
    let identity = GuestAttestationIdentity::generate()?;
    let attestation = identity.collect(parameters).await?;
    attestation.verify_key_proof()?;
    let warning = if matches!(nonce_origin, ExampleInputOrigin::GeneratedInsideGuest) {
        "native evidence collected with a guest-generated demonstration challenge; supply --nonce-hex from a relying party for remote freshness"
    } else {
        "native evidence and guest-key possession collected; vendor signature and measurement appraisal must still be performed by the relying party"
    };
    Ok(ExampleOutput {
        schema_version: 1,
        status: "native_evidence_collected",
        nonce_origin,
        manifest_origin,
        key_proof_verified: true,
        warning,
        attestation,
    })
}

#[cfg(target_os = "linux")]
fn current_executable_digest() -> Result<[u8; 32], io::Error> {
    let executable = std::env::current_exe()?;
    let mut file = std::fs::File::open(executable)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

#[cfg(target_os = "linux")]
fn next_value(
    arguments: &mut std::iter::Peekable<impl Iterator<Item = OsString>>,
    option: &str,
) -> Result<String, io::Error> {
    arguments
        .next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{option} requires a UTF-8 value"),
            )
        })
}

#[cfg(target_os = "linux")]
fn parse_hex_32(value: String) -> Result<[u8; 32], io::Error> {
    let bytes = hex::decode(&value).map_err(|source| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("expected 64 hexadecimal characters: {source}"),
        )
    })?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("expected 32 decoded bytes; received {}", bytes.len()),
        )
    })
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("nanocodex-vm-guest must be built for a Linux guest target");
}
