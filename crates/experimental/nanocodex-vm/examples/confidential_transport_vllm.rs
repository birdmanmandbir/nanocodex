use std::{
    io,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use nanocodex_vm::{
    host::{
        AttestationChallenge, AttestedCommand, AttestedCommandProof, CommandProofExpectation,
        CpuAttestationProfile, GuestAttestationParameters, ManifestSha256, NativeVerifierSet,
        NvidiaAttestationProfile, NvidiaNvattestVerifier, TdxVerificationPolicy, TdxVerifier,
        VerifiedDeploymentHistory, verify_attestation, verify_released_secret_proof,
    },
    tools::VmToolSession,
};
use serde::Serialize;
use sha2::{Digest as _, Sha256};
use tokio::process::Command;

mod attestation_support;

use attestation_support::{load_inference_manifest, parse_hex, read_bounded};

const MAX_COLLATERAL_BYTES: usize = 16 * 1024 * 1024;
const MAX_DEPLOYMENT_HISTORY_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_ROOT_BYTES: usize = 64 * 1024;

struct Options {
    transport: String,
    transport_arguments: Vec<String>,
    local_guest: PathBuf,
    manifest: PathBuf,
    prompt: PathBuf,
    collateral: PathBuf,
    intel_root: Option<PathBuf>,
    nvidia_policy: PathBuf,
    nvattest: PathBuf,
    authorization_history: PathBuf,
    authorization_public_key: [u8; 32],
    authorization_head: ManifestSha256,
}

#[derive(Serialize)]
struct Output {
    schema_version: u32,
    status: &'static str,
    workload_manifest_sha256: String,
    expected_executable_sha256: String,
    expected_argv: Vec<String>,
    authorized_release: String,
    authorization_head_sha256: String,
    proof: AttestedCommandProof,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = Options::parse()?;
    let (manifest, workload_digest) = load_inference_manifest(&options.manifest)?;
    let authorization_history = VerifiedDeploymentHistory::from_jsonl(
        &read_bounded(&options.authorization_history, MAX_DEPLOYMENT_HISTORY_BYTES)?,
        options.authorization_public_key,
        options.authorization_head,
    )?;
    let authorization = authorization_history.require_authorized(
        ManifestSha256::from_bytes(workload_digest),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
    )?;
    let authorized_release = authorization.release_id().to_owned();
    let prompt = read_bounded(&options.prompt, MAX_PROMPT_BYTES)?;
    if prompt.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "prompt cannot be empty").into());
    }
    let guest_bytes = std::fs::read(&options.local_guest)?;
    let executable_digest: [u8; 32] = Sha256::digest(&guest_bytes).into();
    if executable_digest != manifest.guest_executable_sha256() {
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
    )
    .measure_workload_in_tdx_rtmr3();
    let attestation = tokio::time::timeout(Duration::from_secs(180), session.attest(parameters))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "attestation timed out"))??;
    attestation.verify_key_proof()?;
    let collateral = read_bounded(&options.collateral, MAX_COLLATERAL_BYTES)?;
    let (mr_td, rt_mrs) = manifest.tdx_reference()?;
    let mut policy = TdxVerificationPolicy::new(&manifest.policy_id, &collateral, mr_td, rt_mrs)?
        .with_workload_rtmr3(rt_mrs[3]);
    if let Some(intel_root) = &options.intel_root {
        policy = policy.with_intel_root(read_bounded(intel_root, MAX_ROOT_BYTES)?);
    }
    let nvidia = NvidiaNvattestVerifier::program(&options.nvattest, &options.nvidia_policy);
    let verifiers = NativeVerifierSet::new(TdxVerifier::new(policy), nvidia);
    let verified = verify_attestation(
        attestation.into_bundle(),
        &challenge,
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        &verifiers,
    )
    .await?;
    let confidential_command = verified.seal_confidential_command(
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
        command,
        executable_digest,
        &prompt,
    )?;
    let proof = tokio::time::timeout(
        Duration::from_secs(240),
        session.prove_confidential_command(confidential_command),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "inference proof timed out"))??;
    let expectation =
        CommandProofExpectation::new(challenge, workload_digest, executable_digest, argv.clone())
            .stdin_sha256(Sha256::digest(&prompt).into());
    let result = verify_released_secret_proof(&proof, &verified, &expectation)?;
    let response: serde_json::Value = serde_json::from_slice(result.stdout())?;
    if response.get("status").and_then(serde_json::Value::as_str)
        != Some("vllm_inference_completed")
    {
        return Err(io::Error::other("authenticated inference output was unexpected").into());
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&Output {
            schema_version: 1,
            status: "closed_chain_vllm_inference_verified",
            workload_manifest_sha256: hex::encode(workload_digest),
            expected_executable_sha256: hex::encode(executable_digest),
            expected_argv: argv,
            authorized_release,
            authorization_head_sha256: authorization_history.head_sha256().to_hex(),
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
        let mut prompt = None;
        let mut collateral = None;
        let mut intel_root = None;
        let mut nvidia_policy = None;
        let mut nvattest = PathBuf::from("nvattest");
        let mut authorization_history = None;
        let mut authorization_public_key = None;
        let mut authorization_head = None;
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
                "--prompt" => prompt = Some(PathBuf::from(value(&mut arguments, "--prompt")?)),
                "--collateral" => {
                    collateral = Some(PathBuf::from(value(&mut arguments, "--collateral")?))
                }
                "--intel-root" => {
                    intel_root = Some(PathBuf::from(value(&mut arguments, "--intel-root")?))
                }
                "--nvidia-policy" => {
                    nvidia_policy = Some(PathBuf::from(value(&mut arguments, "--nvidia-policy")?))
                }
                "--nvattest" => nvattest = PathBuf::from(value(&mut arguments, "--nvattest")?),
                "--authorization-history" => {
                    authorization_history = Some(PathBuf::from(value(
                        &mut arguments,
                        "--authorization-history",
                    )?))
                }
                "--authorization-key" => {
                    authorization_public_key = Some(parse_hex(
                        &value(&mut arguments, "--authorization-key")?,
                        "authorization public key",
                    )?)
                }
                "--authorization-head" => {
                    authorization_head = Some(
                        ManifestSha256::from_hex(&value(&mut arguments, "--authorization-head")?)
                            .map_err(io::Error::other)?,
                    )
                }
                "--help" | "-h" => {
                    println!(
                        "usage: confidential_transport_vllm --transport PROGRAM [--transport-arg ARG]... --local-guest PATH --manifest PATH --prompt PATH|- --collateral PATH [--intel-root DER_PATH] --nvidia-policy REGO_PATH [--nvattest PATH] --authorization-history JSONL --authorization-key 64_HEX --authorization-head 64_HEX"
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
            prompt: prompt.ok_or_else(|| missing("--prompt"))?,
            collateral: collateral.ok_or_else(|| missing("--collateral"))?,
            intel_root,
            nvidia_policy: nvidia_policy.ok_or_else(|| missing("--nvidia-policy"))?,
            nvattest,
            authorization_history: authorization_history
                .ok_or_else(|| missing("--authorization-history"))?,
            authorization_public_key: authorization_public_key
                .ok_or_else(|| missing("--authorization-key"))?,
            authorization_head: authorization_head
                .ok_or_else(|| missing("--authorization-head"))?,
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
