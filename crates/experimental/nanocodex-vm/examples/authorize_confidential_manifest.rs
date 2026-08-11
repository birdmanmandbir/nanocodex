use std::{io, path::PathBuf};

use ed25519_dalek::SigningKey;
use nanocodex_vm::host::{DeploymentAuthorizationAction, DeploymentHistoryEntry, ManifestSha256};
use zeroize::Zeroizing;

mod attestation_support;

use attestation_support::{load_inference_manifest, read_bounded, value};

struct Options {
    manifest: PathBuf,
    signing_key: PathBuf,
    sequence: u64,
    previous_head: ManifestSha256,
    release_id: String,
    action: DeploymentAuthorizationAction,
    effective_unix_seconds: u64,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let (_, manifest_sha256) = load_inference_manifest(&options.manifest)?;
    let key_text = Zeroizing::new(String::from_utf8(read_bounded(&options.signing_key, 65)?)?);
    let decoded_key = Zeroizing::new(hex::decode(key_text.trim()).map_err(io::Error::other)?);
    if decoded_key.len() != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "authorization signing key decoded to {} bytes; expected 32",
                decoded_key.len()
            ),
        )
        .into());
    }
    let mut key_bytes = Zeroizing::new([0_u8; 32]);
    key_bytes.copy_from_slice(&decoded_key);
    let signing_key = SigningKey::from_bytes(&key_bytes);
    let entry = DeploymentHistoryEntry::sign(
        options.sequence,
        options.previous_head,
        ManifestSha256::from_bytes(manifest_sha256),
        options.release_id,
        options.action,
        options.effective_unix_seconds,
        &signing_key,
    )?;

    println!("{}", serde_json::to_string(&entry)?);
    eprintln!(
        "authorization_public_key={}\nnew_history_head={}",
        hex::encode(signing_key.verifying_key().to_bytes()),
        entry.entry_sha256()?.to_hex(),
    );
    Ok(())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut manifest = None;
        let mut signing_key = None;
        let mut sequence = None;
        let mut previous_head = None;
        let mut release_id = None;
        let mut action = None;
        let mut effective_unix_seconds = None;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--manifest" => manifest = Some(value(&mut arguments, "--manifest")?.into()),
                "--signing-key" => {
                    signing_key = Some(value(&mut arguments, "--signing-key")?.into())
                }
                "--sequence" => {
                    sequence = Some(parse_u64(
                        &value(&mut arguments, "--sequence")?,
                        "sequence",
                    )?)
                }
                "--previous-head" => {
                    previous_head = Some(
                        ManifestSha256::from_hex(&value(&mut arguments, "--previous-head")?)
                            .map_err(io::Error::other)?,
                    )
                }
                "--release-id" => release_id = Some(value(&mut arguments, "--release-id")?),
                "--action" => {
                    action = Some(match value(&mut arguments, "--action")?.as_str() {
                        "authorize" => DeploymentAuthorizationAction::Authorize,
                        "withdraw" => DeploymentAuthorizationAction::Withdraw,
                        other => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!(
                                    "invalid --action value {other:?}; expected authorize or withdraw"
                                ),
                            ));
                        }
                    })
                }
                "--effective" => {
                    effective_unix_seconds = Some(parse_u64(
                        &value(&mut arguments, "--effective")?,
                        "effective Unix time",
                    )?)
                }
                "--help" | "-h" => {
                    println!(
                        "usage: authorize_confidential_manifest --manifest PATH --signing-key HEX_KEY_FILE --sequence N --previous-head 64_HEX --release-id ID --action authorize|withdraw --effective UNIX_SECONDS"
                    );
                    std::process::exit(0);
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unknown argument {other:?}"),
                    ));
                }
            }
        }
        Ok(Self {
            manifest: manifest.ok_or_else(|| missing("--manifest"))?,
            signing_key: signing_key.ok_or_else(|| missing("--signing-key"))?,
            sequence: sequence.ok_or_else(|| missing("--sequence"))?,
            previous_head: previous_head.ok_or_else(|| missing("--previous-head"))?,
            release_id: release_id.ok_or_else(|| missing("--release-id"))?,
            action: action.ok_or_else(|| missing("--action"))?,
            effective_unix_seconds: effective_unix_seconds.ok_or_else(|| missing("--effective"))?,
        })
    }
}

fn parse_u64(value: &str, label: &str) -> Result<u64, io::Error> {
    value.parse().map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid {label}: {error}"),
        )
    })
}

fn missing(option: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}"))
}
