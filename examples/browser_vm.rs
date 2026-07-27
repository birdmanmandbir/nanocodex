use std::path::PathBuf;

use clap::Parser;
use eyre::{Result, bail};
use nanocodex_browser::{BrowserAction, BrowserActionResult};
use nanocodex_browser_vm::BrowserVm;

const PROOF_PAGE: &str = "data:text/html,<main><h1>Browser VM</h1><button>Continue</button></main>";

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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut builder =
        BrowserVm::builder(args.root_disk, args.vmm, args.gvproxy).vmm_args(args.vmm_arg);
    if let Some(firmware) = args.firmware_directory {
        builder = builder.firmware_directory(firmware);
    }
    let vm = builder.spawn().await?;

    vm.browser()
        .execute(BrowserAction::Open {
            url: PROOF_PAGE.to_owned(),
        })
        .await?;
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
    Ok(())
}
