use std::path::PathBuf;

use clap::Parser;
use eyre::Result;
use nanocodex_react::ReactDoctor;

#[derive(Debug, Parser)]
#[command(about = "Run the Rust-native Nanocodex React source analyzer")]
struct Args {
    /// Workspace root containing JavaScript or TypeScript source.
    #[arg(default_value = ".")]
    root: PathBuf,

    /// Optional file or subtree relative to the workspace root.
    #[arg(long)]
    path: Option<PathBuf>,
}

fn main() -> Result<()> {
    let arguments = Args::parse();
    let doctor = ReactDoctor::builder(arguments.root).build()?;
    let report = match arguments.path {
        Some(path) => doctor.analyze_path(path)?,
        None => doctor.analyze()?,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
