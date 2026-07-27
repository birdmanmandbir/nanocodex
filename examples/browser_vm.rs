use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use axum::{
    Router,
    extract::Request,
    http::{StatusCode, header::AUTHORIZATION},
    response::Html,
    routing::get,
};
use clap::Parser;
use eyre::{Result, bail};
use mpp::client::MultiProvider;
use nanocodex_browser::{BrowserAction, BrowserActionResult, BrowserTarget};
use nanocodex_browser_vm::BrowserVm;
use nanocodex_vm_egress::{
    EgressContext, SecretDelivery, SecretError, SecretGuestConfig, SecretHttpMethod, SecretManager,
    SecretRef, SecretRequestRule, SecretSpec, StaticSecretPolicy, UnmatchedEgress, VmEgress,
};

const PROOF_PAGE: &str = "data:text/html,<main><h1>Browser VM</h1><button>Continue</button></main>";
const PROOF_SECRET: &str = "browser-host-only";

#[derive(Debug, Parser)]
#[command(about = "Run the typed browser controller against headed Chromium in a libkrun VM")]
struct Args {
    /// Immutable headed-browser ext4 image.
    root_disk: PathBuf,

    /// Dedicated VMM executable, or an executable accepting the private config path.
    vmm: PathBuf,

    /// gvproxy executable used for the VM's private network.
    gvproxy: PathBuf,

    /// Argument placed before the private VM config path; repeat as needed.
    #[arg(long)]
    vmm_arg: Vec<String>,

    /// Directory containing the libkrun firmware runtime libraries.
    #[arg(long)]
    firmware_directory: Option<PathBuf>,

    /// Route one browser navigation through host-only secret injection.
    #[arg(long)]
    prove_egress: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut builder =
        BrowserVm::builder(args.root_disk, args.vmm, args.gvproxy).vmm_args(args.vmm_arg);
    if let Some(firmware) = args.firmware_directory {
        builder = builder.firmware_directory(firmware);
    }
    let egress = if args.prove_egress {
        Some(EgressProof::start().await?)
    } else {
        None
    };
    if let Some(egress) = &egress {
        builder = builder.egress(egress.egress.lease());
    }
    let vm = builder.spawn().await?;

    let proof_page = egress
        .as_ref()
        .map_or_else(|| PROOF_PAGE.to_owned(), |egress| egress.url.clone());
    vm.browser()
        .execute(BrowserAction::Open { url: proof_page })
        .await?;
    if let Some(egress) = &egress {
        prove_egress(&vm, egress).await?;
    }
    let snapshot = vm
        .browser()
        .execute(BrowserAction::Snapshot {
            interactive: true,
            compact: true,
            depth: None,
            selector: None,
            include_urls: false,
        })
        .await?;
    let BrowserActionResult::Snapshot { snapshot, refs, .. } = snapshot else {
        bail!("browser VM returned an unexpected snapshot result");
    };
    println!("{snapshot}\nreferences={}", refs.len());

    let screenshot = vm
        .browser()
        .execute(BrowserAction::Screenshot {
            full_page: false,
            annotate: false,
        })
        .await?;
    let BrowserActionResult::Screenshot {
        image: Some(image), ..
    } = screenshot
    else {
        bail!("browser VM returned no screenshot");
    };
    println!("screenshot={}", image.path.display());

    vm.shutdown().await?;
    if let Some(egress) = egress {
        egress.egress.shutdown().await?;
    }
    Ok(())
}

async fn prove_egress(vm: &BrowserVm, egress: &EgressProof) -> Result<()> {
    let diagnostic = vm
        .browser()
        .execute(BrowserAction::Snapshot {
            interactive: false,
            compact: false,
            depth: None,
            selector: None,
            include_urls: true,
        })
        .await?;
    let text = match vm
        .browser()
        .execute(BrowserAction::GetText {
            target: BrowserTarget::css("#status"),
        })
        .await
    {
        Ok(text) => text,
        Err(error) => {
            bail!("browser secret-egress proof page was unavailable: {error}; page={diagnostic:?}")
        }
    };
    if !matches!(
        text,
        BrowserActionResult::Text { ref text, .. } if text == "browser-secret-authorized"
    ) || egress.calls.load(Ordering::SeqCst) != 1
    {
        bail!("browser secret-egress proof did not authorize exactly one request");
    }
    println!("egress=host-only browser credential injected");

    vm.browser()
        .execute(BrowserAction::Open {
            url: "https://example.com/".to_owned(),
        })
        .await?;
    let text = vm
        .browser()
        .execute(BrowserAction::GetText {
            target: BrowserTarget::css("h1"),
        })
        .await?;
    if !matches!(
        text,
        BrowserActionResult::Text { ref text, .. } if text == "Example Domain"
    ) {
        bail!("browser HTTPS egress proof did not render the trusted page");
    }
    println!("egress=https proxy CA trusted by Chromium");
    Ok(())
}

struct EgressProof {
    egress: VmEgress,
    url: String,
    calls: Arc<AtomicUsize>,
}

impl EgressProof {
    async fn start() -> Result<Self> {
        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/allowed",
            get({
                let calls = Arc::clone(&calls);
                move |request: Request| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        if request
                            .headers()
                            .get(AUTHORIZATION)
                            .and_then(|value| value.to_str().ok())
                            == Some("Bearer browser-host-only")
                        {
                            (
                                StatusCode::OK,
                                Html(
                                    "<main><h1 id=status>browser-secret-authorized</h1>\
                                     <button>Continue</button></main>",
                                ),
                            )
                        } else {
                            (
                                StatusCode::UNAUTHORIZED,
                                Html("<main><h1 id=status>missing credential</h1></main>"),
                            )
                        }
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        tokio::spawn(async move {
            if let Err(error) = axum::serve(listener, app).await {
                eprintln!("browser egress proof origin failed: {error}");
            }
        });

        let upstream = format!("http://{address}");
        let secret = SecretSpec::builder(
            "browser-proof",
            SecretRef::new("proof", "browser-token"),
            &upstream,
            SecretDelivery::inject_header("authorization", "Bearer "),
            SecretGuestConfig::new("NANOCODEX_BROWSER_PROOF_BASE_URL"),
        )
        .rule(
            SecretRequestRule::new()
                .method(SecretHttpMethod::Get)
                .path_prefix("/allowed"),
        )
        .build()?;
        let egress = VmEgress::builder(MultiProvider::new())
            .unmatched_egress(UnmatchedEgress::Allow)
            .secrets(
                EgressContext::new("browser-proof", "local-example"),
                Arc::new(StaticSecretPolicy::new([secret])),
                Arc::new(ProofSecretManager),
            )
            .spawn()
            .await?;
        Ok(Self {
            egress,
            url: format!("{upstream}/allowed"),
            calls,
        })
    }
}

struct ProofSecretManager;

#[async_trait]
impl SecretManager for ProofSecretManager {
    async fn resolve(&self, reference: &SecretRef) -> Result<String, SecretError> {
        if reference.provider() == "proof" && reference.key() == "browser-token" {
            Ok(PROOF_SECRET.to_owned())
        } else {
            Err(SecretError::NotFound {
                provider: reference.provider().to_owned(),
                key: reference.key().to_owned(),
            })
        }
    }
}
