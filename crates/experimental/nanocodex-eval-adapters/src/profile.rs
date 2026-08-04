//! Manifest recipes that bind typed evaluation profiles to benchmark adapters.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use nanocodex_eval::{
    Task,
    aggregate::AggregateDataset,
    harbor::{HarborError, HarborJob, HarborJobProgress},
    import::{Environment, Harness, ImportError, ImportStore, ImportedDataset},
    profile::{
        BenchmarkSelection, LoadedManifest, PreparationError, PreparationReceipt, PreparationStore,
        PreparedTask, ProfileError, ProfileRunRequest, ProfileRunner, PublishedPreparation,
        ResolvedProfile, TaskPreparation, TaskPreparer,
    },
    profile_run::{ProfileRunControl, ProfileRunControlError, ProfileRunStatus},
};
use serde::Deserialize;

use crate::{
    ArenaHard, ExternalHarness, HarborDataset, OpenAiEvals, OpenAiSimpleEval, OpenAiSimpleEvals,
    SweBench,
};

/// A complete manifest using Nanocodex's concrete third-party benchmark recipes.
pub type EvalManifest = LoadedManifest<Benchmark>;

/// Stable built-in benchmark inventory and manifest loader.
#[derive(Clone, Copy, Debug, Default)]
pub struct BenchmarkCatalog;

/// Stateful importer for one manifest into one content-addressed store.
pub struct ProfileImporter<'a> {
    catalog: BenchmarkCatalog,
    manifest: &'a EvalManifest,
    store: &'a ImportStore,
}

/// Builder for one manifest-driven evaluation workspace.
#[derive(Clone, Debug, Default)]
pub struct EvaluationWorkspaceBuilder<P = TaskPreparationRequired> {
    manifest: Option<PathBuf>,
    state_directory: Option<PathBuf>,
    preparer: P,
}

/// Owned manifest, adapter catalog, and retained state root.
pub struct EvaluationWorkspace<P> {
    catalog: BenchmarkCatalog,
    manifest: EvalManifest,
    state_directory: PathBuf,
    preparer: P,
}

/// Builder marker requiring callers to install one task preparation strategy.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskPreparationRequired;

/// One completed profile preparation ready for execution.
pub struct PreparedEvaluation {
    receipt: PreparationReceipt,
    published: PublishedPreparation,
}

/// Durable coordinator state enriched with live Harbor trial counts.
pub struct EvaluationStatus {
    run: ProfileRunStatus,
    progress: Option<HarborJobProgress>,
}

/// One custom benchmark source configured in a manifest.
#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "adapter", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Benchmark {
    /// Harbor-family task packages.
    Harbor {
        /// Task or suite directory.
        source: PathBuf,
        /// Pinned source revision.
        revision: String,
    },
    /// Arena-Hard question records and official judge harness.
    ArenaHard {
        /// Question JSONL.
        questions: PathBuf,
        /// Official judge wrapper directory.
        harness: PathBuf,
        /// Pinned source revision.
        revision: String,
        /// Candidate environment.
        #[serde(default = "default_image")]
        image: String,
        /// Optional deterministic prefix used by smoke profiles.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// Declarative OpenAI Evals Match/Includes source.
    OpenaiEvals {
        /// Registry root.
        registry: PathBuf,
        /// Deterministic official grader wrapper.
        harness: PathBuf,
        /// Exact registry eval ID.
        eval: String,
        /// Pinned source revision.
        revision: String,
        /// Candidate environment.
        #[serde(default = "default_image")]
        image: String,
    },
    /// OpenAI simple-evals source and official grader implementation.
    OpenaiSimpleEvals {
        /// Pinned simple-evals checkout.
        checkout: PathBuf,
        /// Official grader wrapper.
        harness: PathBuf,
        /// Official CSV or JSONL data.
        data: PathBuf,
        /// Published eval semantics.
        eval: SimpleEval,
        /// Pinned source revision.
        revision: String,
        /// Candidate environment.
        #[serde(default = "default_image")]
        image: String,
        /// Optional deterministic prefix used by smoke profiles.
        #[serde(default)]
        limit: Option<usize>,
    },
    /// SWE-bench instances and official verifier wrapper.
    SweBench {
        /// Instance JSONL.
        instances: PathBuf,
        /// Official verifier wrapper.
        harness: PathBuf,
        /// Pinned source revision.
        revision: String,
        /// Official image namespace.
        #[serde(default = "default_swe_namespace")]
        namespace: String,
        /// Official image architecture component.
        #[serde(default = "default_swe_architecture")]
        architecture: String,
        /// Official image tag.
        #[serde(default = "default_image_tag")]
        image_tag: String,
    },
    /// Benchmark-owned executable manifest.
    External {
        /// External harness manifest.
        manifest: PathBuf,
    },
}

/// OpenAI simple-evals format selected by a custom benchmark recipe.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SimpleEval {
    /// BrowseComp.
    BrowseComp,
    /// HealthBench.
    HealthBench,
    /// HealthBench Professional.
    HealthBenchProfessional,
    /// GPQA Diamond.
    GpqaDiamond,
}

/// One selected normalized task and its stable profile selector.
#[derive(Clone, Debug)]
pub struct SelectedTask {
    selector: String,
    task: Task,
}

/// Profile import or task selection failed.
#[derive(Debug, thiserror::Error)]
pub enum ProfileImportError {
    /// Generic profile resolution failed.
    #[error(transparent)]
    Profile(#[from] ProfileError),
    /// A benchmark adapter rejected its source.
    #[error(transparent)]
    Import(#[from] ImportError),
    /// A built-in recipe is known but unavailable in this build.
    #[error("{0}")]
    BuiltinUnavailable(String),
    /// A normalized task selector did not exist.
    #[error("{0}")]
    Selection(String),
    /// Preparation receipt publication or validation failed.
    #[error(transparent)]
    Preparation(#[from] PreparationError),
    /// Runtime image, disk, or execution-input preparation failed.
    #[error("task runtime preparation failed: {0}")]
    RuntimePreparation(Box<dyn std::error::Error + Send + Sync>),
    /// Cross-process run status or stop control failed.
    #[error(transparent)]
    Control(#[from] ProfileRunControlError),
    /// Retained Harbor evidence could not be loaded or aggregated.
    #[error(transparent)]
    Harbor(#[from] HarborError),
}

impl EvaluationWorkspace<TaskPreparationRequired> {
    /// Starts a workspace builder.
    pub fn builder() -> EvaluationWorkspaceBuilder {
        EvaluationWorkspaceBuilder::default()
    }
}

impl<P> EvaluationWorkspace<P> {
    /// Loads durable status for the current or most recent profile invocation.
    pub fn status(
        &self,
        profile: Option<&str>,
    ) -> Result<Option<EvaluationStatus>, ProfileImportError> {
        let profile = self
            .manifest
            .resolve_profile(profile, |name| self.catalog.contains(name))?;
        let Some(run) =
            ProfileRunControl::new(self.state_directory.join("runs").join(profile.name()))
                .status()?
        else {
            return Ok(None);
        };
        let progress = run
            .job_directory()
            .map(HarborJob::open)
            .transpose()?
            .map(|job| job.progress())
            .transpose()?;
        Ok(Some(EvaluationStatus { run, progress }))
    }

    /// Requests graceful drain from the active profile coordinator.
    pub fn stop(&self, profile: Option<&str>) -> Result<ProfileRunStatus, ProfileImportError> {
        let profile = self
            .manifest
            .resolve_profile(profile, |name| self.catalog.contains(name))?;
        Ok(
            ProfileRunControl::new(self.state_directory.join("runs").join(profile.name()))
                .request_stop()?,
        )
    }

    /// Rebuilds a stable aggregate report from the current retained job.
    pub fn report(&self, profile: Option<&str>) -> Result<AggregateDataset, ProfileImportError> {
        let status = self.status(profile)?.ok_or_else(|| {
            ProfileImportError::Selection("profile has not run; no report is available".to_owned())
        })?;
        let job = status.run.job_directory().ok_or_else(|| {
            ProfileImportError::Selection(
                "profile run has not opened an evaluator job yet; no report is available"
                    .to_owned(),
            )
        })?;
        Ok(HarborJob::open(job)?.aggregate_dataset()?)
    }
}

impl EvaluationStatus {
    /// Durable coordinator phase and identity.
    pub const fn run(&self) -> &ProfileRunStatus {
        &self.run
    }

    /// Live retained trial counts once an evaluator job exists.
    pub const fn progress(&self) -> Option<HarborJobProgress> {
        self.progress
    }
}

impl fmt::Display for EvaluationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.run)?;
        if let Some(progress) = self.progress {
            write!(
                formatter,
                " completed={} running={} pending={} errored={} total={}",
                progress.completed(),
                progress.running(),
                progress.pending(),
                progress.errored(),
                progress.total()
            )?;
        }
        Ok(())
    }
}

impl<P: TaskPreparer> EvaluationWorkspace<P> {
    /// Resolves, imports, prepares, and publishes one immutable profile receipt.
    pub async fn prepare(
        &self,
        profile: Option<&str>,
    ) -> Result<PreparedEvaluation, ProfileImportError> {
        let profile = self
            .manifest
            .resolve_profile(profile, |name| self.catalog.contains(name))?;
        if !profile.hosts().is_empty() {
            return Err(ProfileImportError::Selection(
                "remote profile preparation is not implemented; omit `hosts` for local execution"
                    .to_owned(),
            ));
        }
        let imports = ImportStore::new(self.state_directory.join("imports"));
        let selected =
            ProfileImporter::new(self.catalog, &self.manifest, &imports).import(&profile)?;
        self.preparer
            .prepare(TaskPreparation::new(
                selected
                    .iter()
                    .map(|selected| selected.task().clone())
                    .collect(),
                self.state_directory.join("vm"),
            ))
            .await
            .map_err(|source| ProfileImportError::RuntimePreparation(Box::new(source)))?;
        let tasks = selected
            .iter()
            .map(|selected| PreparedTask::new(selected.selector(), selected.task()))
            .collect();
        let receipt = PreparationReceipt::new(self.manifest.sha256(), &profile, tasks)?;
        let published =
            PreparationStore::new(self.state_directory.join("prepared")).publish(&receipt)?;
        Ok(PreparedEvaluation { receipt, published })
    }

    /// Opens the current preparation after revalidating its manifest and inputs.
    pub fn prepared(
        &self,
        profile: Option<&str>,
    ) -> Result<PreparedEvaluation, ProfileImportError> {
        let profile = self
            .manifest
            .resolve_profile(profile, |name| self.catalog.contains(name))?;
        let (receipt, published) = PreparationStore::new(self.state_directory.join("prepared"))
            .open_current(profile.name())?;
        if receipt.manifest_sha256() != self.manifest.sha256() {
            return Err(ProfileImportError::Preparation(PreparationError::Invalid(
                format!(
                    "profile {:?} was prepared from manifest {}, but the current manifest is {}; run `nanocodex eval prepare {}` again",
                    profile.name(),
                    receipt.manifest_sha256(),
                    self.manifest.sha256(),
                    profile.name()
                ),
            )));
        }
        Ok(PreparedEvaluation { receipt, published })
    }
}

impl<P: ProfileRunner + TaskPreparer> EvaluationWorkspace<P> {
    /// Opens the mandatory preparation and executes or resumes its full matrix.
    pub async fn run(self, profile: Option<&str>) -> Result<P::Output, ProfileImportError> {
        let prepared = self.prepared(profile)?;
        let profile = prepared.receipt.profile().to_owned();
        self.preparer
            .run(ProfileRunRequest::new(
                prepared.receipt,
                self.state_directory.join("runs").join(profile),
                self.state_directory.join("vm"),
            ))
            .await
            .map_err(|source| ProfileImportError::RuntimePreparation(Box::new(source)))
    }
}

impl<P> EvaluationWorkspaceBuilder<P> {
    /// Selects the repository manifest.
    pub fn manifest(mut self, path: impl Into<PathBuf>) -> Self {
        self.manifest = Some(path.into());
        self
    }

    /// Selects the retained evaluator state directory.
    pub fn state_directory(mut self, path: impl Into<PathBuf>) -> Self {
        self.state_directory = Some(path.into());
        self
    }
}

impl EvaluationWorkspaceBuilder<TaskPreparationRequired> {
    /// Installs the complete task-runtime preparation policy for this workspace.
    pub fn task_preparer<P: TaskPreparer>(self, preparer: P) -> EvaluationWorkspaceBuilder<P> {
        EvaluationWorkspaceBuilder {
            manifest: self.manifest,
            state_directory: self.state_directory,
            preparer,
        }
    }
}

impl<P: TaskPreparer> EvaluationWorkspaceBuilder<P> {
    /// Loads the manifest and builds the owned workspace.
    pub fn build(self) -> Result<EvaluationWorkspace<P>, ProfileImportError> {
        let manifest = self.manifest.ok_or_else(|| {
            ProfileImportError::Selection("evaluation manifest is required".to_owned())
        })?;
        let state_directory = match self.state_directory {
            Some(directory) => directory,
            None => std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|home| home.join(".nanocodex/evals"))
                .ok_or_else(|| {
                    ProfileImportError::Selection(
                        "cannot resolve ~/.nanocodex/evals because HOME is not configured; pass --dir"
                            .to_owned(),
                    )
                })?,
        };
        Ok(EvaluationWorkspace {
            catalog: BenchmarkCatalog::new(),
            manifest: EvalManifest::load(manifest)?,
            state_directory,
            preparer: self.preparer,
        })
    }
}

impl PreparedEvaluation {
    /// Immutable preparation receipt.
    pub const fn receipt(&self) -> &PreparationReceipt {
        &self.receipt
    }

    /// Published receipt path and identity.
    pub const fn published(&self) -> &PublishedPreparation {
        &self.published
    }
}

impl fmt::Display for PreparedEvaluation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(formatter, "prepared {}", self.receipt.profile())?;
        writeln!(formatter, "receipt: {}", self.published.path().display())?;
        write!(formatter, "digest:  {}", self.published.digest())
    }
}

impl BenchmarkCatalog {
    /// Creates the installed benchmark catalog.
    pub const fn new() -> Self {
        Self
    }

    /// Loads the manifest and resolves a profile against the built-in catalog.
    pub fn load_profile(
        self,
        path: impl AsRef<Path>,
        profile: Option<&str>,
    ) -> Result<(EvalManifest, ResolvedProfile), ProfileImportError> {
        let manifest = EvalManifest::load(path)?;
        let profile = manifest.resolve_profile(profile, |name| self.contains(name))?;
        Ok((manifest, profile))
    }

    /// Returns whether a stable name belongs to the installed catalog.
    pub fn contains(self, name: &str) -> bool {
        self.recipe(name).is_some()
    }

    fn recipe(self, name: &str) -> Option<Builtin> {
        let adapter = match name {
            "terminal-bench-2.1" | "frontier-bench" | "stable-bench" => "harbor",
            "arena-hard-v2" => "arena-hard",
            "openai-evals" => "openai-evals",
            "browsecomp" | "healthbench" | "healthbench-professional" | "gpqa-diamond" => {
                "openai-simple-evals"
            }
            "swe-bench-pro" => "swe-bench",
            "agents-last-exam"
            | "gdpval-aa"
            | "artificial-analysis"
            | "frontiermath"
            | "osworld"
            | "benchcad"
            | "ctf"
            | "sec-bench"
            | "exploitbench"
            | "exploitgym"
            | "kernelbench"
            | "kernelgen"
            | "nanogpt"
            | "posttrainbench"
            | "mmmu-pro"
            | "toolathlon"
            | "mrcr"
            | "graphwalks"
            | "arc-agi" => "external",
            _ => return None,
        };
        Some(Builtin { adapter })
    }
}

impl<'a> ProfileImporter<'a> {
    /// Binds a manifest and import store for one preparation operation.
    pub const fn new(
        catalog: BenchmarkCatalog,
        manifest: &'a EvalManifest,
        store: &'a ImportStore,
    ) -> Self {
        Self {
            catalog,
            manifest,
            store,
        }
    }

    /// Imports and selects every benchmark task required by a resolved profile.
    pub fn import(
        &self,
        profile: &ResolvedProfile,
    ) -> Result<Vec<SelectedTask>, ProfileImportError> {
        let mut selected = Vec::new();
        for (name, selection) in profile.selections() {
            let benchmark = self.manifest.benchmark(name).ok_or_else(|| {
                let built_in = self
                    .catalog
                    .recipe(name)
                    .expect("resolved built-in benchmark");
                ProfileImportError::BuiltinUnavailable(format!(
                    "built-in benchmark {name:?} uses the {} adapter, but its pinned acquisition recipe is not installed yet",
                    built_in.adapter
                ))
            })?;
            let dataset = self.import_benchmark(name, benchmark)?;
            self.select_tasks(name, selection, &dataset, &mut selected)?;
        }
        Ok(selected)
    }

    fn import_benchmark(
        &self,
        name: &str,
        benchmark: &Benchmark,
    ) -> Result<ImportedDataset, ImportError> {
        let root = self.manifest.root();
        let store = self.store;
        match benchmark {
            Benchmark::Harbor { source, revision } => store.import(&HarborDataset::new(
                name,
                resolve_path(root, source),
                revision,
            )),
            Benchmark::ArenaHard {
                questions,
                harness,
                revision,
                image,
                limit,
            } => {
                let mut importer = ArenaHard::new(
                    name,
                    resolve_path(root, questions),
                    revision,
                    Environment::OciImage(image.clone()),
                    Harness::directory(resolve_path(root, harness))?,
                );
                if let Some(limit) = limit {
                    importer = importer.limit(*limit);
                }
                store.import(&importer)
            }
            Benchmark::OpenaiEvals {
                registry,
                harness,
                eval,
                revision,
                image,
            } => store.import(&OpenAiEvals::new(
                name,
                resolve_path(root, registry),
                resolve_path(root, harness),
                eval,
                revision,
                Environment::OciImage(image.clone()),
            )),
            Benchmark::OpenaiSimpleEvals {
                checkout,
                harness,
                data,
                eval,
                revision,
                image,
                limit,
            } => {
                let mut importer = OpenAiSimpleEvals::new(
                    name,
                    resolve_path(root, checkout),
                    resolve_path(root, harness),
                    resolve_path(root, data),
                    revision,
                    (*eval).into(),
                    Environment::OciImage(image.clone()),
                );
                if let Some(limit) = limit {
                    importer = importer.limit(*limit);
                }
                store.import(&importer)
            }
            Benchmark::SweBench {
                instances,
                harness,
                revision,
                namespace,
                architecture,
                image_tag,
            } => store.import(
                &SweBench::new(
                    name,
                    resolve_path(root, instances),
                    revision,
                    namespace,
                    Harness::directory(resolve_path(root, harness))?,
                )
                .architecture(architecture)
                .image_tag(image_tag),
            ),
            Benchmark::External { manifest } => {
                store.import(&ExternalHarness::new(resolve_path(root, manifest)))
            }
        }
    }

    fn select_tasks(
        &self,
        benchmark: &str,
        selection: &BenchmarkSelection,
        dataset: &ImportedDataset,
        output: &mut Vec<SelectedTask>,
    ) -> Result<(), ProfileImportError> {
        let mut missing = selection.tasks().clone();
        for task in dataset.tasks() {
            if selection.is_all() || selection.tasks().contains(task.name()) {
                missing.remove(task.name());
                output.push(SelectedTask {
                    selector: format!("{benchmark}/{}", task.name()),
                    task: task.clone(),
                });
            }
        }
        if !missing.is_empty() {
            return Err(ProfileImportError::Selection(format!(
                "benchmark {benchmark:?} has no normalized task(s): {}",
                missing.into_iter().collect::<Vec<_>>().join(", ")
            )));
        }
        Ok(())
    }
}

impl SelectedTask {
    /// Stable `<benchmark>/<task>` selector.
    pub fn selector(&self) -> &str {
        &self.selector
    }

    /// Immutable normalized evaluator task.
    pub const fn task(&self) -> &Task {
        &self.task
    }

    /// Consumes the selection and returns its task.
    pub fn into_task(self) -> Task {
        self.task
    }
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

#[derive(Clone, Copy)]
struct Builtin {
    adapter: &'static str,
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn default_image() -> String {
    "debian:bookworm-slim".to_owned()
}

fn default_swe_namespace() -> String {
    "swebench".to_owned()
}

fn default_swe_architecture() -> String {
    "x86_64".to_owned()
}

fn default_image_tag() -> String {
    "latest".to_owned()
}

#[cfg(test)]
mod tests {
    use std::{
        convert::Infallible,
        fs,
        sync::{Arc, Mutex},
    };

    use super::*;

    #[derive(Clone, Default)]
    struct RecordingPreparer {
        tasks: Arc<Mutex<Vec<String>>>,
        cache: Arc<Mutex<Option<PathBuf>>>,
        run_output: Arc<Mutex<Option<PathBuf>>>,
    }

    impl TaskPreparer for RecordingPreparer {
        type Error = Infallible;

        fn prepare(
            &self,
            request: TaskPreparation,
        ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
            let cache = request.cache_directory().to_path_buf();
            let tasks = request
                .into_tasks()
                .into_iter()
                .map(|task| task.name().to_owned())
                .collect();
            let recorded_tasks = Arc::clone(&self.tasks);
            let recorded_cache = Arc::clone(&self.cache);
            async move {
                *recorded_tasks.lock().unwrap() = tasks;
                *recorded_cache.lock().unwrap() = Some(cache);
                Ok(())
            }
        }
    }

    impl ProfileRunner for RecordingPreparer {
        type Error = Infallible;
        type Output = String;

        fn run(
            self,
            request: ProfileRunRequest,
        ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
            let profile = request.receipt().profile().to_owned();
            let output = request.output_directory().to_path_buf();
            async move {
                *self.run_output.lock().unwrap() = Some(output);
                Ok(profile)
            }
        }
    }

    #[test]
    fn catalog_covers_recorded_gpt_5_6_families() {
        for benchmark in [
            "terminal-bench-2.1",
            "browsecomp",
            "healthbench",
            "healthbench-professional",
            "gpqa-diamond",
            "swe-bench-pro",
            "agents-last-exam",
            "gdpval-aa",
            "artificial-analysis",
            "frontiermath",
            "osworld",
            "benchcad",
            "ctf",
            "sec-bench",
            "exploitbench",
            "exploitgym",
            "kernelbench",
            "kernelgen",
            "nanogpt",
            "posttrainbench",
            "mmmu-pro",
            "toolathlon",
            "mrcr",
            "graphwalks",
            "arc-agi",
        ] {
            assert!(BenchmarkCatalog::new().contains(benchmark), "{benchmark}");
        }
    }

    #[tokio::test]
    async fn adapter_smoke_prepares_every_installed_route_as_one_profile() {
        let root = tempfile::tempdir().unwrap();
        let sources = root.path().join("sources");
        let harness = make_harness(&sources.join("harness"));
        let harbor = sources.join("harbor/task");
        make_harbor_task(&harbor);

        fs::create_dir_all(&sources).unwrap();
        fs::write(
            sources.join("arena.jsonl"),
            "{\"uid\":\"arena-case\",\"category\":\"smoke\",\"prompt\":\"Answer once.\"}\n",
        )
        .unwrap();

        let registry = sources.join("openai-registry");
        fs::create_dir_all(registry.join("evals")).unwrap();
        fs::create_dir_all(registry.join("data/demo")).unwrap();
        fs::write(
            registry.join("evals/demo.yaml"),
            "demo.match-v1:\n  class: evals.elsuite.basic.match:Match\n  args:\n    samples_jsonl: demo/samples.jsonl\n",
        )
        .unwrap();
        fs::write(
            registry.join("data/demo/samples.jsonl"),
            "{\"input\":[{\"role\":\"user\",\"content\":\"2 + 2?\"}],\"ideal\":[\"4\"]}\n",
        )
        .unwrap();

        let simple = sources.join("simple-evals");
        make_simple_evals_checkout(&simple);
        fs::write(
            sources.join("health.jsonl"),
            "{\"prompt\":[{\"role\":\"user\",\"content\":\"Be helpful.\"}],\"rubrics\":[{\"criterion\":\"Helpful\",\"points\":1,\"tags\":[]}],\"example_tags\":[],\"prompt_id\":\"health-case\"}\n",
        )
        .unwrap();

        fs::write(
            sources.join("swe.jsonl"),
            "{\"instance_id\":\"owner__repo-1\",\"problem_statement\":\"Fix it.\",\"repo\":\"owner/repo\",\"base_commit\":\"abc\",\"FAIL_TO_PASS\":[],\"PASS_TO_PASS\":[]}\n",
        )
        .unwrap();

        let external = sources.join("external");
        let external_harness = make_harness(&external.join("harness"));
        fs::write(
            external.join("manifest.toml"),
            r#"schema_version = "1"
name = "external-smoke"

[source]
kind = "smoke"
revision = "smoke@1"

[[case]]
id = "external-case"
prompt = "Create the artifact."
output = "workspace"
oci_image = "debian:bookworm-slim"
harness = "harness"
allow_internet = false
"#,
        )
        .unwrap();
        assert!(external_harness.is_dir());

        let manifest = root.path().join("nanocodex.toml");
        fs::write(
            &manifest,
            format!(
                r#"default = "adapter-smoke"

[benchmark.harbor-smoke]
adapter = "harbor"
source = {harbor:?}
revision = "harbor@1"

[benchmark.arena-smoke]
adapter = "arena-hard"
questions = {arena:?}
harness = {harness:?}
revision = "arena@1"
limit = 1

[benchmark.openai-evals-smoke]
adapter = "openai-evals"
registry = {registry:?}
harness = {harness:?}
eval = "demo.match-v1"
revision = "openai-evals@1"

[benchmark.simple-evals-smoke]
adapter = "openai-simple-evals"
checkout = {simple:?}
harness = {harness:?}
data = {health:?}
eval = "health-bench"
revision = "simple-evals@1"
limit = 1

[benchmark.swe-smoke]
adapter = "swe-bench"
instances = {swe:?}
harness = {harness:?}
revision = "swe-bench@1"

[benchmark.external-smoke]
adapter = "external"
manifest = {external_manifest:?}

[profiles.adapter-smoke]
tasks = ["harbor-smoke", "arena-smoke", "openai-evals-smoke", "simple-evals-smoke", "swe-smoke", "external-smoke"]
trials = 1
model = ["gpt-5.6-sol"]
thinking = ["low"]
"#,
                harbor = harbor,
                arena = sources.join("arena.jsonl"),
                harness = harness,
                registry = registry,
                simple = simple,
                health = sources.join("health.jsonl"),
                swe = sources.join("swe.jsonl"),
                external_manifest = external.join("manifest.toml"),
            ),
        )
        .unwrap();

        let state = root.path().join("state");
        let preparer = RecordingPreparer::default();
        let observation = preparer.clone();
        let workspace = EvaluationWorkspace::builder()
            .manifest(&manifest)
            .state_directory(&state)
            .task_preparer(preparer)
            .build()
            .unwrap();
        let Err(error) = workspace.prepared(None) else {
            panic!("profile must require preparation before it can be opened");
        };
        assert!(error.to_string().contains("has not been prepared"));

        let prepared = workspace.prepare(None).await.unwrap();

        let tasks = observation.tasks.lock().unwrap();
        assert_eq!(tasks.len(), 6);
        assert_eq!(
            observation.cache.lock().unwrap().as_deref(),
            Some(state.join("vm").as_path())
        );
        assert_eq!(prepared.receipt().profile(), "adapter-smoke");
        assert!(prepared.published().path().is_file());
        assert_eq!(
            workspace.prepared(None).unwrap().published(),
            prepared.published()
        );
        let runner = EvaluationWorkspace::builder()
            .manifest(&manifest)
            .state_directory(&state)
            .task_preparer(observation.clone())
            .build()
            .unwrap();
        assert_eq!(runner.run(None).await.unwrap(), "adapter-smoke");
        assert_eq!(
            observation.run_output.lock().unwrap().as_deref(),
            Some(state.join("runs/adapter-smoke").as_path())
        );

        fs::write(
            &manifest,
            format!(
                "{}\n# changed after preparation\n",
                fs::read_to_string(&manifest).unwrap()
            ),
        )
        .unwrap();
        let changed = EvaluationWorkspace::builder()
            .manifest(manifest)
            .state_directory(state)
            .task_preparer(RecordingPreparer::default())
            .build()
            .unwrap();
        let Err(error) = changed.prepared(None) else {
            panic!("changed manifest must invalidate preparation");
        };
        assert!(error.to_string().contains("run `nanocodex eval prepare"));
    }

    fn make_harness(path: &Path) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
        fs::write(path.join("grade.py"), "# smoke grader\n").unwrap();
        fs::write(path.join("gpqa_prepare.py"), "# smoke preparation\n").unwrap();
        fs::write(
            path.join("test.sh"),
            "#!/bin/sh\nprintf '1\\n' > /logs/verifier/reward.txt\n",
        )
        .unwrap();
        path.to_path_buf()
    }

    fn make_harbor_task(path: &Path) {
        fs::create_dir_all(path.join("tests")).unwrap();
        fs::create_dir(path.join("environment")).unwrap();
        fs::write(path.join("instruction.md"), "Complete the smoke task.\n").unwrap();
        fs::write(
            path.join("tests/test.sh"),
            "#!/bin/sh\nprintf '1\\n' > /logs/verifier/reward.txt\n",
        )
        .unwrap();
        fs::write(
            path.join("task.toml"),
            r#"schema_version = "1.1"
[task]
name = "harbor-case"
description = "adapter smoke"
[agent]
timeout_sec = 30.0
[verifier]
timeout_sec = 30.0
[environment]
docker_image = "debian:bookworm-slim"
cpus = 1
memory_mb = 256
storage_mb = 256
gpus = 0
allow_internet = false
"#,
        )
        .unwrap();
    }

    fn make_simple_evals_checkout(path: &Path) {
        fs::create_dir_all(path.join("sampler")).unwrap();
        for relative in [
            "LICENSE",
            "browsecomp_eval.py",
            "common.py",
            "gpqa_eval.py",
            "healthbench_eval.py",
            "types.py",
            "sampler/chat_completion_sampler.py",
            "sampler/responses_sampler.py",
        ] {
            fs::write(path.join(relative), format!("# fixture {relative}\n")).unwrap();
        }
    }
}
