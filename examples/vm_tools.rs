use std::{
    error::Error,
    net::Ipv4Addr,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header::WWW_AUTHENTICATE},
    response::IntoResponse,
    routing::post,
};
use mpp::{
    Base64UrlJson, MppError, PaymentChallenge, PaymentCredential, PaymentPayload,
    client::PaymentProvider, format_www_authenticate,
};
use mpp_egress::{EgressPolicy, MppEgress};
use nanocodex::{Tool, ToolContext, ToolExecution, ToolInput, ToolOutputBody, ToolOutputContent};
use nanocodex_vm::{VmToolSession, mpp_egress_layer};
use nanovm::{BlockDevice, EgressLease, GuestCommand, VmConfig, VmProcessConfig};
use serde::Deserialize;
use serde_json::value::to_raw_value;
use tokio::process::Command;

const GUEST_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";
const RUNTIME_BLOCK_DEVICE: &str = "/dev/vdb";
const RUNTIME_MOUNT: &str = "/run/nanocodex";
type AnyError = Box<dyn Error + Send + Sync>;

#[derive(Deserialize)]
struct CommandOutput {
    output: String,
    exit_code: Option<i32>,
    session_id: Option<i64>,
}

fn main() -> Result<(), AnyError> {
    let mut arguments = std::env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.first().is_some_and(|value| value == "--vmm") {
        let config = arguments
            .get(1)
            .cloned()
            .map(PathBuf::from)
            .ok_or("VMM mode requires a private configuration path")?;
        VmProcessConfig::read(config)?.run()?;
        return Ok(());
    }
    let prove_mpp = arguments.last().is_some_and(|value| value == "--prove-mpp");
    if prove_mpp {
        arguments.pop();
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_host(arguments, prove_mpp))
}

#[allow(
    clippy::too_many_lines,
    reason = "the executable intentionally presents one linear end-to-end VM tool proof"
)]
async fn run_host(arguments: Vec<std::ffi::OsString>, prove_mpp: bool) -> Result<(), AnyError> {
    let root = arguments
        .first()
        .cloned()
        .map(PathBuf::from)
        .ok_or("usage: vm-tools ROOTFS [GUEST_RUNTIME_EXT4] [--prove-mpp]")?;
    let runtime = arguments.get(1).cloned().map(PathBuf::from);
    let (egress, mpp_proof) = if prove_mpp {
        let proof = MppProof::start().await?;
        let layer = mpp_egress_layer(Arc::clone(&proof.egress))?;
        (EgressLease::internet().with_layer(layer)?, Some(proof))
    } else {
        (EgressLease::disabled(), None)
    };
    let (config, guest) = if let Some(runtime) = runtime {
        let config = VmConfig::ext4(root)
            .cpus(2)
            .memory_mib(768)
            .block_device(BlockDevice::read_only("nanocodex-runtime", runtime));
        let init = format!(
            "set -eu; mkdir -p \"$1\" {RUNTIME_MOUNT}; \
             mount -t ext4 -o ro {RUNTIME_BLOCK_DEVICE} {RUNTIME_MOUNT}; \
             exec {RUNTIME_MOUNT}/nanocodex-vm-guest \"$1\""
        );
        let guest = GuestCommand::new("/bin/sh")
            .arg("-c")
            .arg(init)
            .arg("nanocodex-vm-init")
            .arg("/workspace");
        (config, guest)
    } else {
        (
            VmConfig::new(root).cpus(2).memory_mib(768),
            GuestCommand::new(GUEST_RUNTIME).arg("/workspace"),
        )
    };
    let (config, command) = egress.configure(config, &guest);
    let process_config = VmProcessConfig::new(config, command).write_private()?;
    let executable = std::env::current_exe()?;
    let mut vmm = Command::new(executable);
    vmm.env_clear();
    for name in ["DYLD_LIBRARY_PATH", "LD_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            vmm.env(name, value);
        }
    }
    vmm.arg("--vmm").arg(process_config.path());
    let mut session = VmToolSession::spawn(&mut vmm)?;
    session.provision_egress(egress).await?;
    let vm = session.tools();
    let _agent_tools = vm
        .tools_builder()
        .working_directory("/workspace")
        .default_shell("sh")
        .build()?;
    let context = ToolContext {
        model: "vm-proof",
        session_id: "session-1",
        call_id: "call-1",
        history: &[],
        output_token_budget: 10_000,
    };

    let execution = vm
        .exec_command_tool()
        .execute(
            function_input(&serde_json::json!({
                "cmd": "printf 'kernel='; uname -srm; printf 'pid1='; cat /proc/1/comm",
                "workdir": "/workspace",
                "login": false
            }))?,
            context,
        )
        .await?;
    let output = command_output(execution)?;
    println!("exec_command: {}", output.output.trim());
    if output.exit_code != Some(0) {
        return Err("exec_command did not exit successfully".into());
    }

    let execution = vm
        .exec_command_tool()
        .execute(
            function_input(&serde_json::json!({
                "cmd": "cat",
                "workdir": "/workspace",
                "login": false,
                "yield_time_ms": 250
            }))?,
            context,
        )
        .await?;
    let output = command_output(execution)?;
    let shell_session = output
        .session_id
        .ok_or("exec_command did not retain an interactive session")?;
    println!("exec_command session: {shell_session}");

    let execution = vm
        .write_stdin_tool()
        .execute(
            function_input(&serde_json::json!({
                "session_id": shell_session,
                "chars": "from-host\n",
                "yield_time_ms": 1_000
            }))?,
            context,
        )
        .await?;
    let mut output = command_output(execution)?;
    for _ in 0..3 {
        if output.output.contains("from-host") {
            break;
        }
        output = command_output(
            vm.write_stdin_tool()
                .execute(
                    function_input(&serde_json::json!({
                        "session_id": shell_session,
                        "yield_time_ms": 1_000
                    }))?,
                    context,
                )
                .await?,
        )?;
    }
    println!("write_stdin: {}", output.output.trim());
    if !output.output.contains("from-host") {
        return Err("write_stdin did not reach the retained guest process".into());
    }

    let proof_file = format!("vm-proof-{}.txt", std::process::id());
    let patch = format!(
        "*** Begin Patch\n*** Add File: {proof_file}\n+changed inside the guest\n*** End Patch"
    );
    let execution = vm
        .apply_patch_tool()
        .execute(ToolInput::Freeform(patch), context)
        .await?;
    println!("apply_patch: {}", text_output(execution)?.trim());

    let execution = vm
        .exec_command_tool()
        .execute(
            function_input(&serde_json::json!({
                "cmd": "printf iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII= | base64 -d > pixel.png",
                "workdir": "/workspace",
                "login": false
            }))?,
            context,
        )
        .await?;
    if command_output(execution)?.exit_code != Some(0) {
        return Err("failed to prepare guest image fixture".into());
    }

    let execution = vm
        .view_image_tool()
        .execute(
            function_input(&serde_json::json!({
                "path": "pixel.png",
                "detail": "original"
            }))?,
            context,
        )
        .await?;
    let ToolOutputBody::Content(image_items) = execution.output else {
        return Err("view_image did not return multimodal content".into());
    };
    let Some(ToolOutputContent::InputImage { image_url, detail }) = image_items.into_iter().next()
    else {
        return Err("view_image did not return an image".into());
    };
    println!(
        "view_image: detail={detail:?}, data_url_bytes={}",
        image_url.len()
    );
    if let Some(proof) = &mpp_proof {
        let execution = vm
            .exec_command_tool()
            .execute(
                function_input(&serde_json::json!({
                    "cmd": format!(
                        "curl --fail --silent --show-error --request POST --data same-body {}",
                        proof.url
                    ),
                    "workdir": "/workspace",
                    "login": false
                }))?,
                context,
            )
            .await?;
        let output = command_output(execution)?;
        if output.exit_code != Some(0) || output.output.trim() != "paid" {
            return Err(format!("MPP curl proof failed: {}", output.output).into());
        }
        if proof.payments.load(Ordering::SeqCst) != 1 || proof.calls.load(Ordering::SeqCst) != 2 {
            return Err("MPP curl was not paid and replayed exactly once".into());
        }
        println!("mpp egress: guest curl paid and replayed exactly once");
    }
    println!("all VM-owned tools executed through one retained libkrun VM");
    session.shutdown().await?;
    Ok(())
}

struct MppProof {
    egress: Arc<MppEgress>,
    url: String,
    payments: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
}

impl MppProof {
    async fn start() -> Result<Self, AnyError> {
        let calls = Arc::new(AtomicUsize::new(0));
        let challenge = payment_challenge()?;
        let app = Router::new().route(
            "/paid",
            post({
                let calls = Arc::clone(&calls);
                move |request: Request| {
                    let calls = Arc::clone(&calls);
                    let challenge = challenge.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if request.headers().contains_key("authorization") {
                            (StatusCode::OK, "paid").into_response()
                        } else {
                            (
                                StatusCode::PAYMENT_REQUIRED,
                                [(WWW_AUTHENTICATE, challenge)],
                                "payment required",
                            )
                                .into_response()
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("MPP proof origin failed: {error}");
            }
        });
        let provider = ProofPaymentProvider::default();
        let payments = Arc::clone(&provider.payments);
        let egress = Arc::new(MppEgress::start(provider, EgressPolicy::default()).await?);
        Ok(Self {
            egress,
            url: format!("http://{address}/paid"),
            payments,
            calls,
        })
    }
}

#[derive(Clone, Default)]
struct ProofPaymentProvider {
    payments: Arc<AtomicUsize>,
}

impl PaymentProvider for ProofPaymentProvider {
    fn supports(&self, method: &str, intent: &str) -> bool {
        method == "test" && intent == "charge"
    }

    async fn pay(&self, challenge: &PaymentChallenge) -> Result<PaymentCredential, MppError> {
        self.payments.fetch_add(1, Ordering::SeqCst);
        Ok(PaymentCredential::new(
            challenge.to_echo(),
            PaymentPayload::hash("vm-mpp-proof"),
        ))
    }
}

fn payment_challenge() -> Result<String, AnyError> {
    let request = Base64UrlJson::from_value(&serde_json::json!({
        "amount": "1",
        "currency": "test"
    }))?;
    Ok(format_www_authenticate(&PaymentChallenge::new(
        "vm-proof",
        "test.local",
        "test",
        "charge",
        request,
    ))?)
}

fn function_input(value: &serde_json::Value) -> Result<ToolInput, serde_json::Error> {
    to_raw_value(value).map(ToolInput::Function)
}

fn command_output(execution: ToolExecution) -> Result<CommandOutput, AnyError> {
    let text = text_output(execution)?;
    serde_json::from_str(&text).map_err(Into::into)
}

fn text_output(execution: ToolExecution) -> Result<String, AnyError> {
    if !execution.success {
        return Err("tool execution reported failure".into());
    }
    match execution.output {
        ToolOutputBody::Text(text) => Ok(text),
        ToolOutputBody::Content(_) => Err("expected text tool output".into()),
    }
}
