use std::path::PathBuf;

use clap::Parser;
use eyre::Result;
use nanocodex_vm::GuestRuntimeDisk;
use nanovm_image::{CachePolicy, VmImageBuilder};

const GIBIBYTE: u64 = 1_024 * 1_024 * 1_024;

#[derive(Debug, Parser)]
#[command(about = "Prepare a content-addressed browser VM image from its Dockerfile")]
struct Args {
    /// Directory containing the browser image Dockerfile and build context.
    #[arg(default_value = "crates/nanocodex-browser-vm/image")]
    context: PathBuf,

    /// Signed VMM executable accepting the private VM config path.
    #[arg(long)]
    vmm: PathBuf,

    /// Static aarch64 Linux nanocodex-vm-guest ELF.
    #[arg(long)]
    guest_runtime: PathBuf,

    /// Directory containing the libkrun firmware runtime libraries.
    #[arg(long)]
    firmware_directory: Option<PathBuf>,

    /// Argument placed before the private VM config path; repeat as needed.
    #[arg(long)]
    vmm_arg: Vec<String>,

    /// Content-addressed image and runtime cache.
    #[arg(long, default_value = ".cache/browser-vm")]
    cache: PathBuf,

    /// Final immutable disk size in GiB.
    #[arg(long, default_value_t = 2)]
    disk_gib: u64,

    /// Refresh mutable OCI tags before building.
    #[arg(long)]
    refresh: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let runtime = GuestRuntimeDisk::prepare(&args.guest_runtime, &args.cache)?;
    let mut builder = VmImageBuilder::new(&args.vmm, runtime.path()).vmm_args(args.vmm_arg);
    if let Some(firmware) = args.firmware_directory {
        builder = builder.firmware_directory(firmware);
    }
    let image = builder
        .prepare(
            args.context,
            args.disk_gib.saturating_mul(GIBIBYTE),
            &args.cache,
            if args.refresh {
                CachePolicy::Refresh
            } else {
                CachePolicy::Reuse
            },
        )
        .await?;
    println!("{}", image.path().display());
    Ok(())
}
