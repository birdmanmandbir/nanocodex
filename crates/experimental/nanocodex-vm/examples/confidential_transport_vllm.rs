use std::{
    io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::{
    host::{
        AttestationChallenge, AttestedCommand, AttestedCommandProof, AttestedCommandRequest,
        CommandProofExpectation, CpuAttestationProfile, GuestAttestationParameters,
        NvidiaAttestationProfile, verify_collected_command_proof,
    },
    tools::VmToolSession,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

mod attestation_support;

use attestation_support::load_inference_manifest;

struct Options {
    transport: String,
    transport_arguments: Vec<String>,
    local_guest: PathBuf,
    manifest: PathBuf,
}

#[derive(Serialize)]
struct Output {
    schema_version: u32,
    status: &'static str,
    warning: &'static str,
    workload_manifest_sha256: String,
    expected_executable_sha256: String,
    expected_argv: Vec<String>,
    proof: AttestedCommandProof,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let (manifest, workload_digest) = load_inference_manifest(&options.manifest)?;
    let guest_bytes = std::fs::read(&options.local_guest)?;
    let executable_digest: [u8; 32] = Sha256::digest(&guest_bytes).into();
    if hex::encode(executable_digest) != manifest.guest_executable_sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local guest executable does not match the relying-party manifest",
        )
        .into());
    }
    let argv = manifest.argv();

    let mut transport = Command::new(&options.transport);
    transport.args(&options.transport_arguments);
    let session = VmToolSession::spawn(&mut transport)?;
    tokio::time::timeout(Duration::from_secs(30), session.ready())
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "guest readiness timed out"))??;

    let mut nonce = [0_u8; 32];
    getrandom::fill(&mut nonce)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let challenge = AttestationChallenge::new(
        nonce,
        manifest.policy_id.clone(),
        now.checked_add(300)
            .ok_or_else(|| io::Error::other("attestation expiry overflow"))?,
    )?;
    let command = argv[1..].iter().try_fold(
        AttestedCommand::new(&argv[0])?.timeout_millis(180_000)?,
        |command, argument| command.arg(argument),
    )?;
    let parameters = GuestAttestationParameters::new(
        challenge.clone(),
        workload_digest,
        CpuAttestationProfile::IntelTdx,
        Some(NvidiaAttestationProfile::H100Single),
    );
    let request = AttestedCommandRequest::new(parameters, command);
    let proof = tokio::time::timeout(Duration::from_secs(240), session.prove_command(request))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "inference proof timed out"))??;
    let expectation =
        CommandProofExpectation::new(challenge, workload_digest, executable_digest, argv.clone());
    let collected = verify_collected_command_proof(&proof, &expectation)?;
    let response: serde_json::Value = serde_json::from_slice(collected.stdout())?;
    if response.get("status").and_then(serde_json::Value::as_str)
        != Some("vllm_inference_completed")
    {
        return Err(io::Error::other("authenticated inference output was unexpected").into());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            schema_version: 1,
            status: "attested_vllm_inference_collected",
            warning: "appraise the embedded TDX and NVIDIA evidence and compare this manifest with independent relying-party policy before trusting the receipt",
            workload_manifest_sha256: hex::encode(workload_digest),
            expected_executable_sha256: hex::encode(executable_digest),
            expected_argv: argv,
            proof,
        })?
    );
    session.shutdown().await?;
    Ok(())
}

impl Options {
    fn parse() -> Result<Self, io::Error> {
        let mut transport = None;
        let mut transport_arguments = Vec::new();
        let mut local_guest = None;
        let mut manifest = None;
        let mut arguments = std::env::args().skip(1);
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--transport" => transport = Some(value(&mut arguments, "--transport")?),
                "--transport-arg" => {
                    transport_arguments.push(value(&mut arguments, "--transport-arg")?)
                }
                "--local-guest" => {
                    local_guest = Some(PathBuf::from(value(&mut arguments, "--local-guest")?))
                }
                "--manifest" => {
                    manifest = Some(PathBuf::from(value(&mut arguments, "--manifest")?))
                }
                "--help" | "-h" => {
                    println!(
                        "usage: confidential_transport_vllm --transport PROGRAM [--transport-arg ARG]... --local-guest PATH --manifest PATH"
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
            transport: transport.ok_or_else(|| missing("--transport"))?,
            transport_arguments,
            local_guest: local_guest.ok_or_else(|| missing("--local-guest"))?,
            manifest: manifest.ok_or_else(|| missing("--manifest"))?,
        })
    }
}

fn value(arguments: &mut impl Iterator<Item = String>, option: &str) -> Result<String, io::Error> {
    arguments
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}")))
}

fn missing(option: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("missing {option}"))
}
