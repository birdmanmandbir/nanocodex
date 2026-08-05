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
use serde::{Deserialize, Serialize};

use crate::{
    ArenaHard, BuiltinSourceError, BuiltinSources, ExternalHarness, GeneBenchPro, GraphWalks,
    HarborDataset, HealthBenchProfessional, Mrcr, OpenAiEvals, SweBench,
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
    sources: &'a BuiltinSources,
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
    sources: BuiltinSources,
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
#[derive(Clone, Debug, Deserialize, Serialize)]
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
        /// Official baseline model answers used by the pairwise judge.
        #[serde(default)]
        baseline: Option<PathBuf>,
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
    /// OpenAI's public GeneBench-Pro case-study package.
    #[serde(rename = "genebench-pro")]
    GeneBenchPro {
        /// Root containing the official manifest, grader, configs, and data files.
        package: PathBuf,
        /// Pinned package revision.
        revision: String,
        /// Scientific candidate environment Dockerfile context.
        environment: PathBuf,
        /// Wrapper around the official deterministic reference grader.
        harness: PathBuf,
    },
    /// OpenAI's public GraphWalks Parquet release and published F1 grader.
    Graphwalks {
        /// Directory containing both official Parquet partitions.
        source: PathBuf,
        /// Pinned dataset revision.
        revision: String,
        /// Wrapper around the published deterministic F1 grader.
        harness: PathBuf,
        /// Candidate environment.
        #[serde(default = "default_image")]
        image: String,
    },
    /// OpenAI's public MRCR Parquet release and SequenceMatcher grader.
    Mrcr {
        /// Directory containing all six official Parquet partitions.
        source: PathBuf,
        /// Pinned dataset revision.
        revision: String,
        /// Wrapper around the published deterministic similarity grader.
        harness: PathBuf,
        /// Candidate environment.
        #[serde(default = "default_image")]
        image: String,
    },
    /// OpenAI's public HealthBench Professional conversations and rubrics.
    HealthbenchProfessional {
        /// Official JSONL release.
        source: PathBuf,
        /// Pinned dataset revision.
        revision: String,
        /// Evaluator-owned rubric judge wrapper.
        harness: PathBuf,
        /// Candidate environment.
        #[serde(default = "default_image")]
        image: String,
    },
    /// Benchmark-owned executable manifest.
    External {
        /// External harness manifest.
        manifest: PathBuf,
    },
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
    /// Pinned built-in source acquisition failed.
    #[error(transparent)]
    Source(#[from] BuiltinSourceError),
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
        self.sources.prepare(&profile).await?;
        let selected = ProfileImporter::new(self.catalog, &self.manifest, &imports, &self.sources)
            .import(&profile)?;
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
        let receipt =
            PreparationReceipt::new(self.manifest.profile_sha256(&profile)?, &profile, tasks)?;
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
        let profile_sha256 = self.manifest.profile_sha256(&profile)?;
        if receipt.profile_sha256() != profile_sha256 {
            return Err(ProfileImportError::Preparation(PreparationError::Invalid(
                format!(
                    "profile {:?} was prepared from profile identity {}, but the current profile identity is {}; run `nanocodex eval prepare {}` again",
                    profile.name(),
                    receipt.profile_sha256(),
                    profile_sha256,
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
        self.execute(profile, None).await
    }

    /// Opens the mandatory preparation and starts a fresh selected matrix.
    pub async fn start_new(
        self,
        profile: Option<&str>,
        tasks: Vec<String>,
    ) -> Result<P::Output, ProfileImportError> {
        self.execute(profile, Some(tasks)).await
    }

    async fn execute(
        self,
        profile: Option<&str>,
        new_tasks: Option<Vec<String>>,
    ) -> Result<P::Output, ProfileImportError> {
        let prepared = self.prepared(profile)?;
        let profile = prepared.receipt.profile().to_owned();
        let control_directory = self.state_directory.join("runs").join(profile);
        let job_directory = control_directory
            .join("preparations")
            .join(prepared.published.digest());
        let mut request = ProfileRunRequest::begin(
            prepared.receipt,
            control_directory,
            job_directory,
            self.state_directory.join("vm"),
        )?;
        if let Some(tasks) = new_tasks {
            request = request.start_new(tasks);
        }
        self.preparer
            .run(request)
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
        let sources = BuiltinSources::new(state_directory.join("source/upstream"));
        Ok(EvaluationWorkspace {
            catalog: BenchmarkCatalog::new(),
            manifest: EvalManifest::load(manifest)?,
            state_directory,
            preparer: self.preparer,
            sources,
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
            "terminal-bench-2.1" => "harbor",
            "arena-hard-v2" => "arena-hard",
            "openai-evals" => "openai-evals",
            "swe-bench-verified-smoke" => "swe-bench",
            "genebench-pro-public" => "genebench-pro",
            "deep-swe-v1.1" => "harbor",
            "graphwalks" => "graphwalks",
            "mrcr-v2" => "mrcr",
            "healthbench-professional" => "healthbench-professional",
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
        sources: &'a BuiltinSources,
    ) -> Self {
        Self {
            catalog,
            manifest,
            store,
            sources,
        }
    }

    /// Imports and selects every benchmark task required by a resolved profile.
    pub fn import(
        &self,
        profile: &ResolvedProfile,
    ) -> Result<Vec<SelectedTask>, ProfileImportError> {
        let mut selected = Vec::new();
        for (name, selection) in profile.selections() {
            let builtin;
            let benchmark = if let Some(benchmark) = self.manifest.benchmark(name) {
                benchmark
            } else {
                builtin = self.sources.benchmark(name).map_err(|error| {
                    let built_in = self
                        .catalog
                        .recipe(name)
                        .expect("resolved built-in benchmark");
                    ProfileImportError::BuiltinUnavailable(format!(
                        "built-in benchmark {name:?} uses the {} adapter: {error}",
                        built_in.adapter
                    ))
                })?;
                &builtin
            };
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
                baseline,
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
                if let Some(baseline) = baseline {
                    importer = importer.baseline_answers(resolve_path(root, baseline));
                }
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
            Benchmark::GeneBenchPro {
                package,
                revision,
                environment,
                harness,
            } => store.import(&GeneBenchPro::new(
                resolve_path(root, package),
                revision,
                Environment::Dockerfile(resolve_path(root, environment)),
                Harness::directory(resolve_path(root, harness))?,
            )),
            Benchmark::Graphwalks {
                source,
                revision,
                harness,
                image,
            } => store.import(&GraphWalks::new(
                resolve_path(root, source),
                revision,
                Environment::OciImage(image.clone()),
                resolve_path(root, harness),
            )),
            Benchmark::Mrcr {
                source,
                revision,
                harness,
                image,
            } => store.import(&Mrcr::new(
                resolve_path(root, source),
                revision,
                Environment::OciImage(image.clone()),
                resolve_path(root, harness),
            )),
            Benchmark::HealthbenchProfessional {
                source,
                revision,
                harness,
                image,
            } => store.import(&HealthBenchProfessional::new(
                resolve_path(root, source),
                revision,
                Environment::OciImage(image.clone()),
                resolve_path(root, harness),
            )),
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
            let selected_name = selection
                .tasks()
                .iter()
                .find(|selected| Self::matches_task(benchmark, selected, task.name()));
            if selection.is_all() || selected_name.is_some() {
                if let Some(selected_name) = selected_name {
                    missing.remove(selected_name);
                }
                output.push(SelectedTask {
                    selector: format!(
                        "{benchmark}/{}",
                        selected_name.map_or_else(|| task.name(), String::as_str)
                    ),
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

    fn matches_task(benchmark: &str, selected: &str, normalized: &str) -> bool {
        selected == normalized
            || (benchmark == "terminal-bench-2.1"
                && normalized
                    .strip_prefix("terminal-bench/")
                    .is_some_and(|task| task == selected))
            || (benchmark == "deep-swe-v1.1"
                && normalized
                    .strip_prefix("datacurve/")
                    .is_some_and(|task| task == selected))
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

    use sha2::{Digest as _, Sha256};

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
        type Error = ProfileRunControlError;
        type Output = String;

        fn run(
            self,
            request: ProfileRunRequest,
        ) -> impl std::future::Future<Output = Result<Self::Output, Self::Error>> + Send {
            let profile = request.receipt().profile().to_owned();
            let output = request.job_directory().to_path_buf();
            async move {
                *self.run_output.lock().unwrap() = Some(output);
                request.complete()?;
                Ok(profile)
            }
        }
    }

    #[test]
    fn catalog_contains_only_materialized_builtins() {
        for benchmark in [
            "terminal-bench-2.1",
            "arena-hard-v2",
            "openai-evals",
            "swe-bench-verified-smoke",
            "genebench-pro-public",
            "deep-swe-v1.1",
            "graphwalks",
            "mrcr-v2",
            "healthbench-professional",
        ] {
            assert!(BenchmarkCatalog::new().contains(benchmark), "{benchmark}");
        }
        for benchmark in ["swe-bench-pro", "exploitbench", "kernelgen", "arc-agi"] {
            assert!(!BenchmarkCatalog::new().contains(benchmark), "{benchmark}");
        }
        assert!(ProfileImporter::matches_task(
            "deep-swe-v1.1",
            "aiomonitor-task-snapshots-diff",
            "datacurve/aiomonitor-task-snapshots-diff",
        ));
    }

    #[tokio::test]
    async fn custom_adapter_smoke_prepares_every_fixture_route_as_one_profile() {
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

        fs::write(
            sources.join("swe.jsonl"),
            "{\"instance_id\":\"owner__repo-1\",\"problem_statement\":\"Fix it.\",\"repo\":\"owner/repo\",\"base_commit\":\"abc\",\"version\":\"1\",\"patch\":\"diff\",\"test_patch\":\"tests\",\"FAIL_TO_PASS\":[],\"PASS_TO_PASS\":[]}\n",
        )
        .unwrap();

        let gene = sources.join("gene");
        let gene_environment = gene.join("environment");
        let gene_harness = make_harness(&gene.join("harness"));
        make_genebench_package(&gene.join("package"));
        fs::create_dir_all(&gene_environment).unwrap();
        fs::write(
            gene_environment.join("Dockerfile"),
            "FROM python:3.12-slim\nCOPY data_files /workspace/data_files\n",
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

[benchmark.swe-smoke]
adapter = "swe-bench"
instances = {swe:?}
harness = {harness:?}
revision = "swe-bench@1"

[benchmark.gene-smoke]
adapter = "genebench-pro"
package = {gene_package:?}
revision = "openai/genebench@1"
environment = {gene_environment:?}
harness = {gene_harness:?}

[benchmark.external-smoke]
adapter = "external"
manifest = {external_manifest:?}

[profiles.adapter-smoke]
tasks = ["harbor-smoke", "arena-smoke", "openai-evals-smoke", "swe-smoke", "gene-smoke", "external-smoke"]
trials = 1
model = ["gpt-5.6-sol"]
thinking = ["low"]
"#,
                harbor = harbor,
                arena = sources.join("arena.jsonl"),
                harness = harness,
                registry = registry,
                swe = sources.join("swe.jsonl"),
                gene_package = gene.join("package"),
                gene_environment = gene_environment,
                gene_harness = gene_harness,
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

        {
            let tasks = observation.tasks.lock().unwrap();
            assert_eq!(tasks.len(), 6);
        }
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
        let preparation_digest = prepared.published().digest().to_owned();
        let runner = EvaluationWorkspace::builder()
            .manifest(&manifest)
            .state_directory(&state)
            .task_preparer(observation.clone())
            .build()
            .unwrap();
        assert_eq!(runner.run(None).await.unwrap(), "adapter-smoke");
        assert_eq!(
            workspace.status(None).unwrap().unwrap().run().phase(),
            nanocodex_eval::profile_run::ProfileRunPhase::Completed
        );
        assert_eq!(
            observation.run_output.lock().unwrap().as_deref(),
            Some(
                state
                    .join("runs/adapter-smoke/preparations")
                    .join(preparation_digest)
                    .as_path()
            )
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
            .manifest(&manifest)
            .state_directory(&state)
            .task_preparer(RecordingPreparer::default())
            .build()
            .unwrap();
        assert_eq!(
            changed.prepared(None).unwrap().published(),
            prepared.published()
        );

        let contents = fs::read_to_string(&manifest).unwrap();
        fs::write(&manifest, contents.replace("trials = 1", "trials = 2")).unwrap();
        let changed = EvaluationWorkspace::builder()
            .manifest(manifest)
            .state_directory(state)
            .task_preparer(RecordingPreparer::default())
            .build()
            .unwrap();
        let Err(error) = changed.prepared(None) else {
            panic!("changed selected profile must invalidate preparation");
        };
        assert!(error.to_string().contains("run `nanocodex eval prepare"));
    }

    #[test]
    fn repository_native_smoke_selects_every_installed_adapter_shape() {
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../nanocodex.toml");
        let (_, profile) = BenchmarkCatalog::new()
            .load_profile(manifest, Some("adapter-smoke-native"))
            .unwrap();
        let selected = profile
            .selections()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();

        assert_eq!(
            selected,
            [
                "arena-hard-v2",
                "external-smoke",
                "genebench-pro-public",
                "graphwalks",
                "healthbench-professional",
                "mrcr-v2",
                "openai-evals",
                "swe-bench-verified-smoke",
                "terminal-bench-2.1",
            ]
        );
        assert!(profile.harnesses().is_empty());
    }

    fn make_harness(path: &Path) -> PathBuf {
        fs::create_dir_all(path).unwrap();
        fs::write(path.join("Dockerfile"), "FROM debian:bookworm-slim\n").unwrap();
        fs::write(path.join("grade.py"), "# smoke grader\n").unwrap();
        fs::write(
            path.join("test.sh"),
            "#!/bin/sh\nprintf '1\\n' > /logs/verifier/reward.txt\n",
        )
        .unwrap();
        path.to_path_buf()
    }

    fn make_genebench_package(path: &Path) {
        let problem = path.join("problems/gene-case");
        fs::create_dir_all(problem.join("data_files")).unwrap();
        let config = br#"{"id":"gene-case","task":"Analyze it.","data_files":["data_files/input.tsv.gz"],"ground_truth":{"value":1},"grader":{"type":"numeric_tolerance","config":{"key":"value"}}}"#;
        let data = b"fixture";
        fs::write(problem.join("eval_config.json"), config).unwrap();
        fs::write(problem.join("data_files/input.tsv.gz"), data).unwrap();
        fs::write(path.join("reference_grader.py"), "# grader\n").unwrap();
        let file = |path: &str, bytes: &[u8]| {
            serde_json::json!({
                "path": path,
                "bytes": bytes.len(),
                "sha256": hex::encode(Sha256::digest(bytes)),
            })
        };
        fs::write(
            path.join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "problem_count": 1,
                "problems": [{
                    "eval_id": "gene-case",
                    "eval_config": "problems/gene-case/eval_config.json",
                    "files": [
                        file("problems/gene-case/eval_config.json", config),
                        file("problems/gene-case/data_files/input.tsv.gz", data),
                    ],
                }],
            }))
            .unwrap(),
        )
        .unwrap();
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
}
