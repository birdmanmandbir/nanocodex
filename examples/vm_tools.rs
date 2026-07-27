use std::{
    error::Error,
    fs, io,
    net::Ipv4Addr,
    path::{Path, PathBuf},
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
use nanocodex_browser::{Browser, BrowserTool};
use nanocodex_tools::ToolRuntime;
use nanocodex_vm::{GuestRuntimeDisk, VmToolSession, mpp_egress_layer};
use nanovm::{
    BlockDevice, EgressLease, EgressMount, GUEST_EGRESS_ROOT, GuestCommand, VmConfig,
    VmProcessConfig,
};
use serde::Deserialize;
use serde_json::value::to_raw_value;
use tokio::process::Command;

const GUEST_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";
const RUNTIME_BLOCK_DEVICE: &str = "/dev/vdb";
const RUNTIME_MOUNT: &str = "/run/nanocodex";
const VM_BROWSER_PROOF: &str = r#"
const [shell, opened] = await Promise.all([
  tools.exec_command({
    cmd: "printf vm-workspace",
    workdir: "/workspace",
    login: false
  }),
  tools.browser({
    action: "open",
    url: "data:text/html,<main>host-browser</main>"
  })
]);
const page = await tools.browser({
  action: "get_text",
  target: { by: "css", selector: "main" }
});
if (shell.exit_code !== 0 || !shell.output.includes("vm-workspace")) {
  throw new Error("exec_command did not execute in the VM");
}
if (opened.result !== "action" || page.text !== "host-browser") {
  throw new Error("browser did not execute in the shared host session");
}
text({ vm: shell.output, browser: page.text });
"#;
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
        let _ = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::io::stderr)
            .try_init();
        let config = arguments
            .get(1)
            .cloned()
            .map(PathBuf::from)
            .ok_or("VMM mode requires a private configuration path")?;
        VmProcessConfig::read(config)?.run()?;
        return Ok(());
    }
    let prove_mpp = take_flag(&mut arguments, "--prove-mpp");
    let prove_browser = take_flag(&mut arguments, "--prove-browser");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_host(arguments, prove_mpp, prove_browser))
}

#[allow(
    clippy::too_many_lines,
    reason = "the executable intentionally presents one linear end-to-end VM tool proof"
)]
async fn run_host(
    arguments: Vec<std::ffi::OsString>,
    prove_mpp: bool,
    prove_browser: bool,
) -> Result<(), AnyError> {
    let root = arguments.first().cloned().map(PathBuf::from).ok_or(
        "usage: vm-tools ROOTFS [GUEST_RUNTIME_BINARY_OR_EXT4] \
             [--prove-mpp] [--prove-browser]",
    )?;
    let (_private_root, root) = if root.is_file() {
        let directory = tempfile::tempdir()?;
        let private = directory.path().join("rootfs.ext4");
        reflink_or_sparse_copy(&root, &private)?;
        (Some(directory), private)
    } else {
        (None, root)
    };
    let runtime_input = arguments.get(1).cloned().map(PathBuf::from);
    let prepared_runtime = match runtime_input.as_deref() {
        Some(runtime) if is_elf(runtime)? => Some(GuestRuntimeDisk::prepare(runtime, ".cache/vm")?),
        Some(_) | None => None,
    };
    let runtime = prepared_runtime
        .as_ref()
        .map(|runtime| runtime.path().to_owned())
        .or(runtime_input);
    let (egress, mpp_proof) = if prove_mpp {
        let proof = MppProof::start().await?;
        let mpp = mpp_egress_layer(Arc::clone(&proof.egress))?;
        let secrets = secret_style_proof_layer()?;
        (
            EgressLease::internet()
                .with_layer(mpp)?
                .with_layer(secrets)?,
            Some(proof),
        )
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
    let executable = std::env::current_exe()?;
    let mut vmm = Command::new(executable);
    vmm.env_clear();
    for name in ["DYLD_LIBRARY_PATH", "LD_LIBRARY_PATH"] {
        if let Some(value) = std::env::var_os(name) {
            vmm.env(name, value);
        }
    }
    vmm.arg("--vmm");
    let session = VmToolSession::spawn_configured(vmm, config, guest, egress).await?;
    let vm = session.tools();
    let browser = prove_browser.then(Browser::new).transpose()?;
    let mut agent_tools = vm
        .tools_builder()
        .working_directory("/workspace")
        .default_shell("sh");
    if let Some(browser) = &browser {
        agent_tools = agent_tools.tool(BrowserTool::from_browser(browser.clone()));
    }
    let agent_tools = agent_tools.build()?;
    let context = ToolContext::new("vm-proof", "session-1", "call-1", &[], 10_000);

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
                        "test \"$NANOCENTAUR_SECRET_BASE_URL\" = \
                         https://secret-gateway.invalid/v1; \
                         if printf tamper 2>/dev/null >> {GUEST_EGRESS_ROOT}/secrets/route.txt; \
                         then exit 9; fi; \
                         cat {GUEST_EGRESS_ROOT}/secrets/route.txt"
                    ),
                    "workdir": "/workspace",
                    "login": false
                }))?,
                context,
            )
            .await?;
        let output = command_output(execution)?;
        if output.exit_code != Some(0) || output.output.trim() != "public-route" {
            return Err(format!("secret-style egress proof failed: {}", output.output).into());
        }
        println!("secret egress: independent environment and read-only mount composed");

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
    if let Some(browser) = browser {
        let runtime = ToolRuntime::new_with_tools(".", None, None, &agent_tools);
        let execution = runtime.execute_code(VM_BROWSER_PROOF, context).await;
        if !execution.success {
            return Err("combined VM and browser Code Mode proof failed".into());
        }
        browser.close().await?;
        println!("browser: host browser and VM tools composed in one Code Mode cell");
    }
    println!("all VM-owned tools executed through one retained libkrun VM");
    drop(agent_tools);
    drop(vm);
    session.shutdown().await?;
    Ok(())
}

fn reflink_or_sparse_copy(source: &Path, destination: &Path) -> io::Result<u64> {
    match reflink_copy::reflink(source, destination) {
        Ok(()) => return Ok(fs::metadata(destination)?.len()),
        Err(_) => remove_partial_copy(destination)?,
    }

    #[cfg(target_os = "linux")]
    {
        let status = std::process::Command::new("cp")
            .args(["--reflink=never", "--sparse=always", "--"])
            .arg(source)
            .arg(destination)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()?;
        if status.success() {
            return Ok(fs::metadata(destination)?.len());
        }
        remove_partial_copy(destination)?;
        Err(io::Error::other(format!(
            "sparse disk copy failed with {status}"
        )))
    }

    #[cfg(not(target_os = "linux"))]
    fs::copy(source, destination)
}

fn is_elf(path: &Path) -> io::Result<bool> {
    let mut file = fs::File::open(path)?;
    let mut magic = [0_u8; 4];
    match io::Read::read_exact(&mut file, &mut magic) {
        Ok(()) => Ok(magic == *b"\x7fELF"),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(error),
    }
}

fn secret_style_proof_layer() -> Result<EgressLease, AnyError> {
    let directory = Arc::new(tempfile::tempdir()?);
    fs::write(directory.path().join("route.txt"), "public-route\n")?;
    let mut layer = EgressLease::internet();
    layer.insert_environment(
        "NANOCENTAUR_SECRET_BASE_URL",
        "https://secret-gateway.invalid/v1",
    )?;
    layer.insert_mount(EgressMount::read_only(
        "secret-proof",
        directory.path(),
        Path::new(GUEST_EGRESS_ROOT).join("secrets"),
    ))?;
    layer.retain(directory);
    Ok(layer)
}

fn remove_partial_copy(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn take_flag(arguments: &mut Vec<std::ffi::OsString>, flag: &str) -> bool {
    let mut found = false;
    arguments.retain(|argument| {
        if argument == flag {
            found = true;
            false
        } else {
            true
        }
    });
    found
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
