use std::{num::NonZeroUsize, path::PathBuf};

use clap::{Args, Subcommand, ValueEnum};
use eyre::Result;
use nanocodex_eval::import::{Environment, Harness, ImportStore};
use nanocodex_eval_adapters::{
    ArenaHard, ExternalHarness, HarborDataset, OpenAiEvals, OpenAiSimpleEval, OpenAiSimpleEvals,
    SweBench,
};
use serde::Serialize;

#[derive(Args)]
pub(crate) struct Import {
    #[command(subcommand)]
    source: ImportSource,

    /// Content-addressed imported-dataset store.
    #[arg(long, default_value = ".cache/evals/imports", global = true)]
    store: PathBuf,

    /// Emit the imported dataset summary as JSON.
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum ImportSource {
    /// Snapshot Harbor, Terminal-Bench, Frontier-Bench, or StableBench tasks.
    Harbor(Harbor),
    /// Import Arena-Hard prompts with an official judge harness.
    ArenaHard(Arena),
    /// Import a declarative OpenAI Evals Match or Includes eval.
    OpenaiEvals(OpenAi),
    /// Import BrowseComp, HealthBench, HealthBench Professional, or GPQA.
    OpenaiSimpleEvals(OpenAiSimple),
    /// Import SWE-bench instances with official images and harness.
    SweBench(Swe),
    /// Import MLE-bench, PaperBench, or another benchmark-owned harness.
    External(Manifest),
}

#[derive(Args)]
struct Harbor {
    #[command(flatten)]
    identity: Identity,
    /// Harbor task or suite root.
    source: PathBuf,
}

#[derive(Args)]
struct Arena {
    #[command(flatten)]
    identity: Identity,
    /// Arena-Hard `question.jsonl`.
    questions: PathBuf,
    #[command(flatten)]
    hermetic: Hermetic,
    #[command(flatten)]
    selection: Selection,
}

#[derive(Args)]
struct OpenAi {
    #[command(flatten)]
    identity: Identity,
    /// OpenAI Evals `evals/registry` directory.
    registry: PathBuf,
    /// Deterministic Match/Includes verifier harness to snapshot.
    #[arg(long)]
    harness: PathBuf,
    /// Exact registry eval ID, such as `actors-sequence.dev.match-v1`.
    #[arg(long)]
    eval: String,
    /// OCI image used for the candidate turn.
    #[arg(long, default_value = "debian:bookworm-slim")]
    image: String,
}

#[derive(Args)]
struct OpenAiSimple {
    #[command(flatten)]
    identity: Identity,
    /// Pinned OpenAI simple-evals checkout.
    checkout: PathBuf,
    /// Verifier harness containing Dockerfile, test.sh, grade.py, and
    /// gpqa_prepare.py. These files are snapshotted during import rather than
    /// embedded in the Nanocodex binary.
    #[arg(long)]
    harness: PathBuf,
    /// Official CSV or JSONL dataset file.
    data: PathBuf,
    /// Reference eval and grading implementation to package.
    #[arg(long, value_enum)]
    eval: SimpleEval,
    /// OCI image used for the candidate turn.
    #[arg(long, default_value = "debian:bookworm-slim")]
    image: String,
    #[command(flatten)]
    selection: Selection,
}

#[derive(Clone, Copy, ValueEnum)]
enum SimpleEval {
    BrowseComp,
    HealthBench,
    HealthBenchProfessional,
    GpqaDiamond,
}

impl From<SimpleEval> for OpenAiSimpleEval {
    fn from(value: SimpleEval) -> Self {
        match value {
            SimpleEval::BrowseComp => Self::BrowseComp,
            SimpleEval::HealthBench => Self::HealthBench,
            SimpleEval::HealthBenchProfessional => Self::HealthBenchProfessional,
            SimpleEval::GpqaDiamond => Self::GpqaDiamond,
        }
    }
}

#[derive(Args)]
struct Swe {
    #[command(flatten)]
    identity: Identity,
    /// SWE-bench instance JSONL exported from the official dataset.
    instances: PathBuf,
    /// Official image registry namespace.
    #[arg(long, default_value = "swebench")]
    namespace: String,
    /// Official instance image architecture.
    #[arg(long, default_value = "x86_64")]
    architecture: String,
    /// Directory containing the official per-instance `test.sh` adapter.
    #[arg(long)]
    harness: PathBuf,
}

#[derive(Args)]
struct Manifest {
    /// Prepared external harness TOML manifest.
    manifest: PathBuf,
}

#[derive(Args)]
struct Identity {
    /// Filesystem-safe dataset name.
    #[arg(long)]
    name: String,
    /// Pinned upstream commit, release, or dataset revision.
    #[arg(long)]
    revision: String,
}

#[derive(Args)]
struct Hermetic {
    /// OCI image used for the candidate turn.
    #[arg(long, default_value = "debian:bookworm-slim")]
    image: String,
    /// Directory containing official `test.sh` and optional Dockerfile inputs.
    #[arg(long)]
    harness: PathBuf,
}

#[derive(Args, Default)]
struct Selection {
    /// Import only the first N cases for smoke validation.
    #[arg(long)]
    limit: Option<NonZeroUsize>,
}

impl Import {
    pub(crate) fn run(self) -> Result<()> {
        let store = ImportStore::new(&self.store);
        let dataset = match self.source {
            ImportSource::Harbor(arguments) => store.import(&HarborDataset::new(
                arguments.identity.name,
                arguments.source,
                arguments.identity.revision,
            ))?,
            ImportSource::ArenaHard(arguments) => {
                let mut importer = ArenaHard::new(
                    arguments.identity.name,
                    arguments.questions,
                    arguments.identity.revision,
                    Environment::OciImage(arguments.hermetic.image),
                    Harness::directory(arguments.hermetic.harness)?,
                );
                if let Some(limit) = arguments.selection.limit {
                    importer = importer.limit(limit.get());
                }
                store.import(&importer)?
            }
            ImportSource::OpenaiEvals(arguments) => store.import(&OpenAiEvals::new(
                arguments.identity.name,
                arguments.registry,
                arguments.harness,
                arguments.eval,
                arguments.identity.revision,
                Environment::OciImage(arguments.image),
            ))?,
            ImportSource::OpenaiSimpleEvals(arguments) => {
                let mut importer = OpenAiSimpleEvals::new(
                    arguments.identity.name,
                    arguments.checkout,
                    arguments.harness,
                    arguments.data,
                    arguments.identity.revision,
                    arguments.eval.into(),
                    Environment::OciImage(arguments.image),
                );
                if let Some(limit) = arguments.selection.limit {
                    importer = importer.limit(limit.get());
                }
                store.import(&importer)?
            }
            ImportSource::SweBench(arguments) => store.import(
                &SweBench::new(
                    arguments.identity.name,
                    arguments.instances,
                    arguments.identity.revision,
                    arguments.namespace,
                    Harness::directory(arguments.harness)?,
                )
                .architecture(arguments.architecture),
            )?,
            ImportSource::External(arguments) => {
                store.import(&ExternalHarness::new(arguments.manifest))?
            }
        };
        let summary = ImportSummary {
            root: dataset.root().to_path_buf(),
            tasks: dataset.root().join("tasks"),
            digest: dataset.digest().to_owned(),
            source: dataset.source().kind().to_owned(),
            revision: dataset.source().revision().to_owned(),
            cases: dataset.tasks().len(),
        };
        if self.json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        } else {
            println!("imported {} cases", summary.cases);
            println!("dataset: {}", summary.root.display());
            println!("tasks:   {}", summary.tasks.display());
            println!("digest:  {}", summary.digest);
        }
        Ok(())
    }
}

#[derive(Serialize)]
struct ImportSummary {
    root: PathBuf,
    tasks: PathBuf,
    digest: String,
    source: String,
    revision: String,
    cases: usize,
}
