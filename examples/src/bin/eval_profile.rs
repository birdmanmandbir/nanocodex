use std::{env, path::PathBuf};

use nanocodex::{Nanocodex, OpenAi};
use nanocodex_eval::vm::VmProfileRunner;
use nanocodex_eval_adapters::profile::EvaluationWorkspace;
use nanocodex_examples::eval_support::{self as support, AnyError};

#[tokio::main]
async fn main() -> Result<(), AnyError> {
    let manifest = env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("nanocodex.toml"), PathBuf::from);
    let profile = env::args().nth(2);
    let vmm = env::var_os("NANOCODEX_BIN")
        .map_or_else(|| PathBuf::from("target/debug/nanocodex"), PathBuf::from);
    let runtime = env::var_os("NANOCODEX_VM_RUNTIME")
        .map_or_else(|| PathBuf::from(".cache/vm/runtime.ext4"), PathBuf::from);

    let runtime = VmProfileRunner::new(
        vmm,
        runtime,
        Nanocodex::builder(OpenAi::new(support::auth()?)?),
    );
    let workspace = EvaluationWorkspace::builder()
        .manifest(manifest)
        .task_preparer(runtime)
        .build()?;
    let prepared = workspace.prepare(profile.as_deref()).await?;
    println!("{prepared}");

    let result = workspace.run(profile.as_deref()).await?;
    println!("{result}");
    Ok(())
}
