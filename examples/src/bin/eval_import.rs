use std::{env, path::PathBuf};

use nanocodex_eval::import::ImportStore;
use nanocodex_eval_adapters::HarborDataset;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let suite = env::args_os()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: eval-import HARBOR_SUITE UPSTREAM_REVISION")?;
    let revision = env::args()
        .nth(2)
        .ok_or("usage: eval-import HARBOR_SUITE UPSTREAM_REVISION")?;

    let dataset = ImportStore::new(".cache/evals/imports").import(&HarborDataset::new(
        "imported-suite",
        suite,
        revision,
    ))?;

    println!("{}", dataset.root().join("tasks").display());
    println!(
        "{} cases, digest {}",
        dataset.tasks().len(),
        dataset.digest()
    );
    Ok(())
}
