#[cfg(target_os = "linux")]
use std::{
    ffi::{OsStr, OsString},
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs as _},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[cfg(target_os = "linux")]
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use serde_json::{Value, json};
#[cfg(target_os = "linux")]
use sha2::{Digest as _, Sha256};
#[cfg(target_os = "linux")]
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[cfg(target_os = "linux")]
const MAX_ATTESTATION_REQUEST_BYTES: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const MAX_INFERENCE_HTTP_BYTES: usize = 1024 * 1024;
#[cfg(target_os = "linux")]
const MAX_INFERENCE_PROMPT_BYTES: usize = 64 * 1024;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let first = arguments.next();
    if first.as_deref() == Some(OsStr::new("--proof-message")) {
        let message = arguments.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "--proof-message requires one UTF-8 message",
            )
        })?;
        if arguments.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--proof-message accepts exactly one message",
            )
            .into());
        }
        println!(
            "{}",
            message.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "proof message must be UTF-8")
            })?
        );
        return Ok(());
    }
    if first.as_deref() == Some(OsStr::new("--proof-stdin")) {
        if arguments.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--proof-stdin accepts no arguments",
            )
            .into());
        }
        let mut input = Vec::new();
        io::stdin()
            .take((MAX_INFERENCE_PROMPT_BYTES + 1) as u64)
            .read_to_end(&mut input)?;
        if input.is_empty() || input.len() > MAX_INFERENCE_PROMPT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "proof input must contain 1 to 65536 bytes",
            )
            .into());
        }
        io::stdout().lock().write_all(&input)?;
        return Ok(());
    }
    if first.as_deref() == Some(OsStr::new("--proof-vllm-inference")) {
        let options = VllmProofOptions::parse(arguments)?;
        let output = prove_vllm_inference(&options)?;
        serde_json::to_writer(io::stdout().lock(), &output)?;
        println!();
        return Ok(());
    }
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
struct VllmProofOptions {
    container: String,
    image_id: String,
    image_reference: String,
    model: String,
    model_revision: String,
}

#[cfg(target_os = "linux")]
impl VllmProofOptions {
    fn parse(mut arguments: impl Iterator<Item = OsString>) -> Result<Self, io::Error> {
        let container = proof_argument(&mut arguments, "CONTAINER")?;
        let image_id = proof_argument(&mut arguments, "IMAGE_ID")?;
        let image_reference = proof_argument(&mut arguments, "IMAGE_REFERENCE")?;
        let model = proof_argument(&mut arguments, "MODEL")?;
        let model_revision = proof_argument(&mut arguments, "MODEL_REVISION")?;
        if arguments.next().is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--proof-vllm-inference accepts exactly five arguments and reads the prompt from stdin",
            ));
        }
        if container.is_empty()
            || !container
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !image_id.starts_with("sha256:")
            || !image_reference.contains("@sha256:")
            || model.is_empty()
            || model_revision.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid container, image, model, or revision identity",
            ));
        }
        Ok(Self {
            container,
            image_id,
            image_reference,
            model,
            model_revision,
        })
    }
}

#[cfg(target_os = "linux")]
fn proof_argument(
    arguments: &mut impl Iterator<Item = OsString>,
    name: &str,
) -> Result<String, io::Error> {
    arguments
        .next()
        .and_then(|argument| argument.into_string().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("--proof-vllm-inference requires UTF-8 {name}"),
            )
        })
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DockerInspection {
    id: String,
    image: String,
    config: DockerConfig,
    state: DockerState,
    host_config: DockerHostConfig,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DockerConfig {
    image: String,
    cmd: Vec<String>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DockerState {
    running: bool,
    pid: u32,
    started_at: String,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DockerHostConfig {
    device_requests: Vec<DockerDeviceRequest>,
}

#[cfg(target_os = "linux")]
#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
struct DockerDeviceRequest {
    driver: String,
    count: i64,
    capabilities: Vec<Vec<String>>,
}

#[cfg(target_os = "linux")]
#[derive(Serialize)]
struct VllmInferenceProof {
    schema_version: u32,
    status: &'static str,
    server: &'static str,
    container: DockerInspection,
    request: Value,
    response: Value,
}

#[cfg(target_os = "linux")]
fn prove_vllm_inference(
    options: &VllmProofOptions,
) -> Result<VllmInferenceProof, Box<dyn std::error::Error>> {
    let mut prompt = Vec::new();
    io::stdin()
        .take((MAX_INFERENCE_PROMPT_BYTES + 1) as u64)
        .read_to_end(&mut prompt)?;
    if prompt.is_empty() || prompt.len() > MAX_INFERENCE_PROMPT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inference prompt must contain 1 to 65536 bytes",
        )
        .into());
    }
    let prompt = String::from_utf8(prompt)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "prompt must be UTF-8"))?;
    let before = inspect_vllm_container(&options.container)?;
    validate_vllm_container(options, &before)?;
    let request = json!({
        "model": options.model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
        "seed": 7,
        "max_tokens": 32,
    });
    let request_bytes = serde_json::to_vec(&request)?;
    let mut addresses = ("127.0.0.1", 8000).to_socket_addrs()?;
    let address = addresses
        .next()
        .ok_or_else(|| io::Error::other("vLLM address did not resolve"))?;
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(10))?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let response_bytes = http_exchange(
        &mut stream,
        "127.0.0.1:8000",
        "POST",
        "/v1/chat/completions",
        &request_bytes,
    )?;
    let response: Value = serde_json::from_slice(&response_bytes)?;
    if response.get("model").and_then(Value::as_str) != Some(&options.model)
        || response
            .get("choices")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    {
        return Err(io::Error::other("vLLM returned an unexpected model response").into());
    }
    let after = inspect_vllm_container(&options.container)?;
    validate_vllm_container(options, &after)?;
    if before != after {
        return Err(io::Error::other("vLLM container changed during inference").into());
    }
    Ok(VllmInferenceProof {
        schema_version: 1,
        status: "vllm_inference_completed",
        server: "vLLM OpenAI-compatible server",
        container: after,
        request,
        response,
    })
}

#[cfg(target_os = "linux")]
fn inspect_vllm_container(name: &str) -> Result<DockerInspection, Box<dyn std::error::Error>> {
    let mut stream = UnixStream::connect("/var/run/docker.sock")?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let body = http_exchange(
        &mut stream,
        "docker",
        "GET",
        &format!("/containers/{name}/json"),
        &[],
    )?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(target_os = "linux")]
fn validate_vllm_container(
    options: &VllmProofOptions,
    value: &DockerInspection,
) -> Result<(), io::Error> {
    let has_model = argument_pair(&value.config.cmd, "--model", &options.model);
    let has_revision = argument_pair(&value.config.cmd, "--revision", &options.model_revision);
    let has_served_name = argument_pair(&value.config.cmd, "--served-model-name", &options.model);
    let has_gpu = value.host_config.device_requests.iter().any(|request| {
        (request.driver.is_empty() || request.driver == "nvidia")
            && request.count != 0
            && request
                .capabilities
                .iter()
                .flatten()
                .any(|capability| capability == "gpu")
    });
    if !value.state.running
        || value.state.pid == 0
        || value.image != options.image_id
        || value.config.image != options.image_reference
        || !has_model
        || !has_revision
        || !has_served_name
        || !has_gpu
    {
        return Err(io::Error::other(
            "running vLLM container does not match the expected image, model, revision, or GPU policy",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn argument_pair(arguments: &[String], option: &str, value: &str) -> bool {
    arguments
        .windows(2)
        .any(|pair| pair[0] == option && pair[1] == value)
}

#[cfg(target_os = "linux")]
fn http_exchange(
    stream: &mut (impl Read + Write),
    host: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<Vec<u8>, io::Error> {
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut response = Vec::new();
    stream
        .take((MAX_INFERENCE_HTTP_BYTES + 1) as u64)
        .read_to_end(&mut response)?;
    if response.len() > MAX_INFERENCE_HTTP_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "HTTP response exceeds 1 MiB",
        ));
    }
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP response"))?;
    let headers = std::str::from_utf8(&response[..header_end])
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !headers.starts_with("HTTP/1.1 200 ") && !headers.starts_with("HTTP/1.0 200 ") {
        return Err(io::Error::other(format!(
            "HTTP request failed: {}",
            headers.lines().next().unwrap_or("missing status")
        )));
    }
    let chunked = headers.lines().any(|line| {
        line.split_once(':').is_some_and(|(name, value)| {
            name.eq_ignore_ascii_case("transfer-encoding")
                && value.trim().eq_ignore_ascii_case("chunked")
        })
    });
    let body = &response[header_end..];
    if chunked {
        decode_chunked(body)
    } else {
        Ok(body.to_vec())
    }
}

#[cfg(target_os = "linux")]
fn decode_chunked(mut encoded: &[u8]) -> Result<Vec<u8>, io::Error> {
    let mut decoded = Vec::new();
    loop {
        let line_end = encoded
            .windows(2)
            .position(|window| window == b"\r\n")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP chunk"))?;
        let size_text = std::str::from_utf8(&encoded[..line_end])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if size_text.is_empty() || size_text.contains(';') {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid HTTP chunk size",
            ));
        }
        let size = usize::from_str_radix(size_text, 16)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        encoded = &encoded[line_end + 2..];
        if size == 0 {
            if encoded != b"\r\n" {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "HTTP chunk trailers are not accepted",
                ));
            }
            return Ok(decoded);
        }
        if size > MAX_INFERENCE_HTTP_BYTES.saturating_sub(decoded.len())
            || encoded.len() < size + 2
            || &encoded[size..size + 2] != b"\r\n"
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid or oversized HTTP chunk",
            ));
        }
        decoded.extend_from_slice(&encoded[..size]);
        encoded = &encoded[size + 2..];
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum ExampleNvidia {
    Auto,
    Off,
    H100,
    Single,
    Hgx8,
}

#[cfg(target_os = "linux")]
struct ExampleOptions {
    nonce: Option<[u8; 32]>,
    policy_id: String,
    manifest_digest: Option<[u8; 32]>,
    nvidia: ExampleNvidia,
    measure_workload_in_tdx_rtmr3: bool,
}

#[cfg(target_os = "linux")]
impl ExampleOptions {
    fn parse(arguments: impl Iterator<Item = OsString>) -> Result<Self, io::Error> {
        let mut options = Self {
            nonce: None,
            policy_id: "nanocodex-attestation-example-v1".to_owned(),
            manifest_digest: None,
            nvidia: ExampleNvidia::Auto,
            measure_workload_in_tdx_rtmr3: false,
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
                        "h100-single" => ExampleNvidia::H100,
                        "b200-single" => ExampleNvidia::Single,
                        "b200-hgx8" => ExampleNvidia::Hgx8,
                        value => {
                            return Err(io::Error::new(
                                io::ErrorKind::InvalidInput,
                                format!(
                                    "invalid --nvidia value {value:?}; expected auto, off, h100-single, b200-single, or b200-hgx8"
                                ),
                            ));
                        }
                    };
                }
                "--measure-workload-in-tdx-rtmr3" => {
                    options.measure_workload_in_tdx_rtmr3 = true;
                }
                "--help" | "-h" => {
                    eprintln!(
                        "usage: nanocodex-vm-guest --attest-example [--nonce-hex 64_HEX] [--policy-id ID] [--manifest-sha256 64_HEX] [--measure-workload-in-tdx-rtmr3] [--nvidia auto|off|h100-single|b200-single|b200-hgx8]"
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
        ExampleNvidia::H100 => Some(NvidiaAttestationProfile::H100Single),
        ExampleNvidia::Single => Some(NvidiaAttestationProfile::B200Single),
        ExampleNvidia::Hgx8 => Some(NvidiaAttestationProfile::B200Hgx8EncryptedNvlink),
    };
    let challenge = AttestationChallenge::new(
        nonce,
        options.policy_id,
        now.checked_add(300)
            .ok_or_else(|| io::Error::other("attestation expiry overflow"))?,
    )?;
    let mut parameters =
        GuestAttestationParameters::new(challenge, manifest_digest, cpu_profile, nvidia_profile);
    if options.measure_workload_in_tdx_rtmr3 {
        parameters = parameters.measure_workload_in_tdx_rtmr3();
    }
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
