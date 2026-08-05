//! Typed evaluation manifests and profile resolution.
//!
//! Benchmark-format configuration is generic so `nanocodex-eval` can own the
//! durable matrix contract while `nanocodex-eval-adapters` owns concrete
//! third-party source recipes.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fs,
    future::Future,
    io::Write as _,
    path::{Path, PathBuf},
    str::FromStr as _,
};

use nanocodex_agent::NanocodexBuilder;
use nanocodex_oai_api::{Model, Thinking};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};

const PREPARATION_RECEIPT_VERSION: u32 = 2;

/// Repository-level evaluation configuration parameterized by benchmark recipe.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, bound(deserialize = "B: Deserialize<'de>"))]
pub struct Manifest<B> {
    default: Option<String>,
    #[serde(default)]
    hosts: BTreeMap<String, Host>,
    #[serde(default)]
    harness: BTreeMap<String, Harness>,
    #[serde(default, rename = "benchmark")]
    benchmarks: BTreeMap<String, B>,
    #[serde(default)]
    profiles: BTreeMap<String, Profile>,
}

/// One SSH-reachable heavyweight evaluator runner.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    ssh: String,
    #[serde(default)]
    dir: Option<PathBuf>,
}

/// One additional agent CLI and its named argument variants.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Harness {
    command: PathBuf,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    variant: BTreeMap<String, HarnessVariant>,
}

/// Arguments appended to one harness treatment without shell parsing.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessVariant {
    #[serde(default)]
    args: Vec<String>,
}

/// One complete reusable evaluation matrix preset.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    hosts: Vec<String>,
    #[serde(default)]
    harnesses: Vec<String>,
    tasks: Vec<String>,
    trials: u16,
    model: Vec<String>,
    thinking: Vec<String>,
    #[serde(default)]
    web_search: bool,
}

/// A parsed manifest with its canonical path and content identity.
#[derive(Clone, Debug)]
pub struct LoadedManifest<B> {
    root: PathBuf,
    sha256: String,
    manifest: Manifest<B>,
}

/// A validated profile whose references and sweep values are typed.
#[derive(Clone, Debug)]
pub struct ResolvedProfile {
    name: String,
    hosts: Vec<String>,
    harnesses: Vec<ResolvedHarness>,
    selections: BTreeMap<String, BenchmarkSelection>,
    trials: u16,
    models: Vec<Model>,
    thinking: Vec<Thinking>,
    web_search: bool,
}

/// One exact harness treatment selected by a profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ResolvedHarness {
    name: String,
    driver: String,
    command: PathBuf,
    args: Vec<String>,
}

/// Whole-benchmark or exact normalized-task selection.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BenchmarkSelection {
    all: bool,
    tasks: BTreeSet<String>,
}

/// One immutable normalized task bound into a preparation receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedTask {
    selector: String,
    root: PathBuf,
    digest: String,
}

/// One exact additional harness bound into a preparation receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparedHarness {
    name: String,
    driver: String,
    command: PathBuf,
    args: Vec<String>,
    executable_sha256: String,
}

/// Immutable proof that every input required by one profile was prepared.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PreparationReceipt {
    version: u32,
    profile: String,
    manifest_sha256: String,
    #[serde(default)]
    executor_sha256: String,
    tasks: Vec<PreparedTask>,
    harnesses: Vec<PreparedHarness>,
    trials: u16,
    model: Vec<String>,
    thinking: Vec<String>,
    web_search: bool,
}

/// Durable content-addressed store for profile preparation receipts.
#[derive(Clone, Debug)]
pub struct PreparationStore {
    root: PathBuf,
}

/// One published receipt and its content identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishedPreparation {
    path: PathBuf,
    digest: String,
}

/// Complete runtime-preparation request for one resolved profile.
pub struct TaskPreparation {
    tasks: Vec<crate::Task>,
    cache_directory: PathBuf,
}

/// Complete execution request for one validated preparation receipt.
pub struct ProfileRunRequest {
    receipt: PreparationReceipt,
    output_directory: PathBuf,
    cache_directory: PathBuf,
    rerun_tasks: Option<Vec<String>>,
}

/// Runtime boundary that executes one prepared profile.
pub trait ProfileRunner: Sized {
    /// Concrete execution failure.
    type Error: Error + Send + Sync + 'static;
    /// Typed completed-run result.
    type Output;

    /// Executes or resumes the complete prepared matrix.
    fn run(
        self,
        request: ProfileRunRequest,
    ) -> impl Future<Output = Result<Self::Output, Self::Error>> + Send;
}

/// Validated task/treatment/model/thinking sweep derived from a receipt.
pub struct ProfileRunPlan {
    tasks: Vec<crate::Task>,
    sweep: crate::Sweep,
}

/// A preparation receipt could not become an executable Nanocodex sweep.
#[derive(Debug, thiserror::Error)]
pub enum ProfileRunPlanError {
    /// A retained normalized task no longer loads.
    #[error(transparent)]
    Task(#[from] crate::TaskLoadError),
    /// A retained model or thinking value is invalid.
    #[error("invalid retained profile value: {0}")]
    Invalid(String),
    /// An agent coordinate identity was invalid.
    #[error(transparent)]
    Agent(#[from] crate::AgentIdError),
    /// The task × agent × trial sweep was invalid.
    #[error(transparent)]
    Sweep(#[from] crate::SweepError),
}

/// Runtime boundary that prepares every normalized task before a receipt is published.
pub trait TaskPreparer {
    /// Concrete preparation failure.
    type Error: Error + Send + Sync + 'static;

    /// Prepares images, disks, and runtime inputs for the complete task set.
    fn prepare(
        &self,
        request: TaskPreparation,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

impl TaskPreparation {
    /// Binds the normalized task set to its workspace-owned preparation cache.
    #[must_use]
    pub fn new(tasks: Vec<crate::Task>, cache_directory: impl Into<PathBuf>) -> Self {
        Self {
            tasks,
            cache_directory: cache_directory.into(),
        }
    }

    /// Content-addressed cache shared by repeated preparations in this workspace.
    pub fn cache_directory(&self) -> &Path {
        &self.cache_directory
    }

    /// Consumes the request and returns every normalized task.
    #[must_use]
    pub fn into_tasks(self) -> Vec<crate::Task> {
        self.tasks
    }
}

impl ProfileRunRequest {
    /// Binds one receipt to workspace-owned run and cache directories.
    #[must_use]
    pub fn new(
        receipt: PreparationReceipt,
        output_directory: impl Into<PathBuf>,
        cache_directory: impl Into<PathBuf>,
    ) -> Self {
        Self {
            receipt,
            output_directory: output_directory.into(),
            cache_directory: cache_directory.into(),
            rerun_tasks: None,
        }
    }

    /// Forces a fresh run containing all or a selected subset of prepared tasks.
    #[must_use]
    pub fn rerun(mut self, tasks: Vec<String>) -> Self {
        self.rerun_tasks = Some(tasks);
        self
    }

    /// Validated immutable preparation receipt.
    pub const fn receipt(&self) -> &PreparationReceipt {
        &self.receipt
    }

    /// Parent for durable resumable evaluator jobs.
    pub fn output_directory(&self) -> &Path {
        &self.output_directory
    }

    /// Content-addressed VM cache shared with preparation.
    pub fn cache_directory(&self) -> &Path {
        &self.cache_directory
    }

    /// Requested fresh-run task selectors; an empty slice means the full profile.
    pub fn rerun_tasks(&self) -> Option<&[String]> {
        self.rerun_tasks.as_deref()
    }
}

impl ProfileRunPlan {
    /// Prepared normalized tasks in deterministic order.
    pub fn tasks(&self) -> &[crate::Task] {
        &self.tasks
    }

    /// Consumes the plan into its finite sweep.
    #[must_use]
    pub fn into_sweep(self) -> crate::Sweep {
        self.sweep
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct CurrentPreparation {
    receipt: PathBuf,
    digest: String,
}

/// Durable preparation receipt failure.
#[derive(Debug, thiserror::Error)]
pub enum PreparationError {
    /// No preparation was published for the requested profile.
    #[error("profile {0:?} has not been prepared")]
    Missing(String),
    /// Receipt I/O failed.
    #[error("preparation receipt I/O failed at {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// Receipt JSON failed to encode or decode.
    #[error("preparation receipt JSON failed at {path}: {source}")]
    Json {
        /// Affected path.
        path: PathBuf,
        /// JSON failure.
        #[source]
        source: serde_json::Error,
    },
    /// Prepared input identity is invalid or changed.
    #[error("invalid preparation receipt: {0}")]
    Invalid(String),
}

/// A profile manifest could not be loaded or resolved safely.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// Manifest I/O failed.
    #[error("evaluation manifest I/O failed at {path}: {source}")]
    Io {
        /// Affected path.
        path: PathBuf,
        /// Filesystem failure.
        #[source]
        source: std::io::Error,
    },
    /// TOML decoding failed.
    #[error("failed to decode evaluation manifest {path}: {source}")]
    Decode {
        /// Manifest path.
        path: PathBuf,
        /// TOML error.
        #[source]
        source: toml::de::Error,
    },
    /// A typed profile invariant was invalid.
    #[error("invalid evaluation profile: {0}")]
    Invalid(String),
}

impl<B: DeserializeOwned> LoadedManifest<B> {
    /// Loads a manifest and retains its exact SHA-256 identity.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ProfileError> {
        let requested = path.as_ref();
        let path = fs::canonicalize(requested).map_err(|source| ProfileError::Io {
            path: requested.to_path_buf(),
            source,
        })?;
        let bytes = fs::read(&path).map_err(|source| ProfileError::Io {
            path: path.clone(),
            source,
        })?;
        let text = std::str::from_utf8(&bytes).map_err(|source| {
            ProfileError::Invalid(format!("{} is not UTF-8: {source}", path.display()))
        })?;
        let manifest = toml::from_str(text).map_err(|source| ProfileError::Decode {
            path: path.clone(),
            source,
        })?;
        let root = path
            .parent()
            .ok_or_else(|| ProfileError::Invalid("manifest has no parent directory".to_owned()))?
            .to_path_buf();
        Ok(Self {
            root,
            sha256: hex::encode(Sha256::digest(bytes)),
            manifest,
        })
    }
}

impl<B> LoadedManifest<B> {
    /// Resolves one profile using the adapter catalog for built-in names.
    pub fn resolve_profile(
        &self,
        requested: Option<&str>,
        is_builtin: impl Fn(&str) -> bool,
    ) -> Result<ResolvedProfile, ProfileError> {
        for name in self.manifest.benchmarks.keys() {
            if is_builtin(name) {
                return Err(invalid(format!(
                    "custom benchmark {name:?} shadows a built-in benchmark"
                )));
            }
        }
        let name = requested
            .or(self.manifest.default.as_deref())
            .ok_or_else(|| invalid("no profile supplied and manifest has no `default`"))?;
        validate_storage_name("profile", name).map_err(invalid)?;
        let profile = self
            .manifest
            .profiles
            .get(name)
            .ok_or_else(|| invalid(format!("unknown profile {name:?}")))?;
        validate_profile_shape(name, profile)?;
        ensure_unique("host", &profile.hosts)?;
        ensure_unique("harness", &profile.harnesses)?;
        ensure_unique("model", &profile.model)?;
        ensure_unique("thinking", &profile.thinking)?;

        for host in &profile.hosts {
            let configured = self.manifest.hosts.get(host).ok_or_else(|| {
                invalid(format!("profile {name:?} references unknown host {host:?}"))
            })?;
            if configured.ssh.trim().is_empty() {
                return Err(invalid(format!(
                    "host {host:?} has an empty SSH destination"
                )));
            }
            if configured
                .dir
                .as_ref()
                .is_some_and(|directory| directory.as_os_str().is_empty())
            {
                return Err(invalid(format!(
                    "host {host:?} has an empty state directory"
                )));
            }
        }

        let harnesses = profile
            .harnesses
            .iter()
            .map(|selection| self.resolve_harness(selection))
            .collect::<Result<Vec<_>, _>>()?;
        let selections = self.resolve_selections(&profile.tasks, &is_builtin)?;
        let models = profile
            .model
            .iter()
            .map(|value| Model::from_str(value).map_err(invalid))
            .collect::<Result<Vec<_>, _>>()?;
        let thinking = profile
            .thinking
            .iter()
            .map(|value| Thinking::from_str(value).map_err(invalid))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ResolvedProfile {
            name: name.to_owned(),
            hosts: profile.hosts.clone(),
            harnesses,
            selections,
            trials: profile.trials,
            models,
            thinking,
            web_search: profile.web_search,
        })
    }

    /// Returns the manifest directory used to resolve relative source paths.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Returns the exact manifest content digest.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// Returns one custom benchmark recipe by name.
    pub fn benchmark(&self, name: &str) -> Option<&B> {
        self.manifest.benchmarks.get(name)
    }

    fn resolve_harness(&self, selection: &str) -> Result<ResolvedHarness, ProfileError> {
        let (name, variant) = selection
            .split_once('.')
            .map_or((selection, None), |(name, variant)| (name, Some(variant)));
        let harness = self
            .manifest
            .harness
            .get(name)
            .ok_or_else(|| invalid(format!("unknown harness {name:?}")))?;
        let mut args = harness.args.clone();
        if let Some(variant) = variant {
            let variant = harness.variant.get(variant).ok_or_else(|| {
                invalid(format!("harness {name:?} has no variant named {variant:?}"))
            })?;
            args.extend(variant.args.clone());
        } else if !harness.variant.is_empty() {
            return Err(invalid(format!(
                "harness {name:?} defines variants; select {name}.<variant>"
            )));
        }
        Ok(ResolvedHarness {
            name: selection.to_owned(),
            driver: harness.driver.clone().unwrap_or_else(|| name.to_owned()),
            command: resolve_command(&self.root, &harness.command)?,
            args,
        })
    }

    fn resolve_selections(
        &self,
        selectors: &[String],
        is_builtin: &impl Fn(&str) -> bool,
    ) -> Result<BTreeMap<String, BenchmarkSelection>, ProfileError> {
        let mut selections = BTreeMap::<String, BenchmarkSelection>::new();
        for selector in selectors {
            let (benchmark, task) = selector
                .split_once('/')
                .map_or((selector.as_str(), None), |(benchmark, task)| {
                    (benchmark, Some(task))
                });
            if benchmark.is_empty() || task == Some("") {
                return Err(invalid(format!("invalid task selector {selector:?}")));
            }
            if !self.manifest.benchmarks.contains_key(benchmark) && !is_builtin(benchmark) {
                return Err(invalid(format!("unknown benchmark {benchmark:?}")));
            }
            let selection = selections.entry(benchmark.to_owned()).or_default();
            match task {
                None if selection.all || !selection.tasks.is_empty() => {
                    return Err(invalid(format!("overlapping task selector {selector:?}")));
                }
                None => selection.all = true,
                Some(_) if selection.all => {
                    return Err(invalid(format!("overlapping task selector {selector:?}")));
                }
                Some(task) if !selection.tasks.insert(task.to_owned()) => {
                    return Err(invalid(format!("duplicate task selector {selector:?}")));
                }
                Some(_) => {}
            }
        }
        Ok(selections)
    }
}

impl ResolvedProfile {
    /// Stable profile name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Selected host aliases; empty means local execution.
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// Selected additional CLI treatments.
    pub fn harnesses(&self) -> &[ResolvedHarness] {
        &self.harnesses
    }

    /// Benchmark and normalized-task selections.
    pub const fn selections(&self) -> &BTreeMap<String, BenchmarkSelection> {
        &self.selections
    }

    /// Valid trial target per coordinate.
    pub const fn trials(&self) -> u16 {
        self.trials
    }

    /// Model sweep values.
    pub fn models(&self) -> &[Model] {
        &self.models
    }

    /// Thinking-effort sweep values.
    pub fn thinking(&self) -> &[Thinking] {
        &self.thinking
    }

    /// Whether web search is exposed.
    pub const fn web_search(&self) -> bool {
        self.web_search
    }
}

impl ResolvedHarness {
    /// Stable treatment name, including any selected variant.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Concrete driver name.
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// Executable path resolved relative to the manifest.
    pub fn command(&self) -> &Path {
        &self.command
    }

    /// Exact base and variant argv suffix.
    pub fn args(&self) -> &[String] {
        &self.args
    }
}

impl BenchmarkSelection {
    /// Whether every task in the imported benchmark is selected.
    pub const fn is_all(&self) -> bool {
        self.all
    }

    /// Exact normalized task IDs selected from the benchmark.
    pub const fn tasks(&self) -> &BTreeSet<String> {
        &self.tasks
    }
}

impl PreparedTask {
    /// Binds one normalized task selector, package root, and content digest.
    pub fn new(selector: impl Into<String>, task: &crate::Task) -> Self {
        Self {
            selector: selector.into(),
            root: task.root().to_path_buf(),
            digest: task.content_digest().to_owned(),
        }
    }

    /// Stable `<benchmark>/<task>` selector.
    pub fn selector(&self) -> &str {
        &self.selector
    }

    /// Prepared immutable task root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Prepared task-package content digest.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

impl PreparationReceipt {
    /// Creates a receipt after task/image preparation and harness validation.
    pub fn new(
        manifest_sha256: impl Into<String>,
        profile: &ResolvedProfile,
        tasks: Vec<PreparedTask>,
    ) -> Result<Self, PreparationError> {
        if tasks.is_empty() {
            return Err(PreparationError::Invalid(
                "a prepared profile must contain at least one task".to_owned(),
            ));
        }
        let harnesses = profile
            .harnesses()
            .iter()
            .map(PreparedHarness::from_resolved)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            version: PREPARATION_RECEIPT_VERSION,
            profile: profile.name().to_owned(),
            manifest_sha256: manifest_sha256.into(),
            executor_sha256: Self::current_executor_sha256()?,
            tasks,
            harnesses,
            trials: profile.trials(),
            model: profile.models().iter().map(ToString::to_string).collect(),
            thinking: profile.thinking().iter().map(ToString::to_string).collect(),
            web_search: profile.web_search(),
        })
    }

    /// Profile name bound into the receipt.
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Exact manifest content identity used during preparation.
    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }

    /// Exact executable that prepared the native Nanocodex treatment.
    pub fn executor_sha256(&self) -> &str {
        &self.executor_sha256
    }

    /// Prepared tasks in deterministic matrix order.
    pub fn tasks(&self) -> &[PreparedTask] {
        &self.tasks
    }

    /// Exact additional harness treatments.
    pub fn harnesses(&self) -> &[PreparedHarness] {
        &self.harnesses
    }

    /// Trial target per coordinate.
    pub const fn trials(&self) -> u16 {
        self.trials
    }

    /// Model identifiers in sweep order.
    pub fn models(&self) -> &[String] {
        &self.model
    }

    /// Thinking values in sweep order.
    pub fn thinking(&self) -> &[String] {
        &self.thinking
    }

    /// Whether web search is enabled.
    pub const fn web_search(&self) -> bool {
        self.web_search
    }

    /// Builds the exact native and guest-harness sweep in this receipt.
    pub fn run_plan(&self, base: &NanocodexBuilder) -> Result<ProfileRunPlan, ProfileRunPlanError> {
        self.run_plan_for(base, None)
    }

    /// Builds a fresh-run plan for exact prepared selectors or task names.
    pub fn run_plan_for(
        &self,
        base: &NanocodexBuilder,
        requested: Option<&[String]>,
    ) -> Result<ProfileRunPlan, ProfileRunPlanError> {
        let loaded = self
            .tasks
            .iter()
            .map(|prepared| Ok((prepared, crate::Task::load(prepared.root())?)))
            .collect::<Result<Vec<_>, ProfileRunPlanError>>()?;
        let tasks = match requested {
            None | Some([]) => loaded.into_iter().map(|(_, task)| task).collect(),
            Some(requested) => {
                let mut selected = Vec::with_capacity(requested.len());
                for request in requested {
                    let matches = loaded
                        .iter()
                        .filter(|(prepared, task)| {
                            prepared.selector() == request
                                || prepared
                                    .selector()
                                    .rsplit_once('/')
                                    .is_some_and(|(_, name)| name == request)
                                || task.name() == request
                        })
                        .collect::<Vec<_>>();
                    match matches.as_slice() {
                        [(_, task)] => selected.push(task.clone()),
                        [] => {
                            return Err(ProfileRunPlanError::Invalid(format!(
                                "rerun task {request:?} is not in prepared profile {:?}",
                                self.profile
                            )));
                        }
                        _ => {
                            return Err(ProfileRunPlanError::Invalid(format!(
                                "rerun task {request:?} is ambiguous; use its complete benchmark/task selector"
                            )));
                        }
                    }
                }
                selected
            }
        };
        let mut sweep = crate::Sweep::builder()
            .tasks(tasks.clone())
            .trials(self.trials);
        for model in &self.model {
            let parsed_model = Model::from_str(model).map_err(ProfileRunPlanError::Invalid)?;
            for thinking in &self.thinking {
                let parsed_thinking =
                    Thinking::from_str(thinking).map_err(ProfileRunPlanError::Invalid)?;
                sweep = sweep.agent(
                    format!("nanocodex.{model}.{thinking}"),
                    base.clone().model(parsed_model).thinking(parsed_thinking),
                )?;
                for harness in &self.harnesses {
                    sweep = sweep.agent(
                        format!("harness.{}.{model}.{thinking}", harness.name()),
                        base.clone().model(parsed_model).thinking(parsed_thinking),
                    )?;
                }
            }
        }
        Ok(ProfileRunPlan {
            tasks,
            sweep: sweep.build()?,
        })
    }

    fn current_executor_sha256() -> Result<String, PreparationError> {
        let path = std::env::current_exe().map_err(|source| PreparationError::Io {
            path: PathBuf::from("<current executable>"),
            source,
        })?;
        let bytes = fs::read(&path).map_err(|source| PreparationError::Io { path, source })?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }
}

impl PreparedHarness {
    fn from_resolved(harness: &ResolvedHarness) -> Result<Self, PreparationError> {
        let command = Self::resolve_guest_executable(harness)?;
        let bytes = fs::read(&command).map_err(|source| PreparationError::Io {
            path: command.clone(),
            source,
        })?;
        if !matches!(harness.driver(), "codex" | "nanocodex") {
            return Err(PreparationError::Invalid(format!(
                "unknown harness driver {:?}; expected codex or nanocodex",
                harness.driver()
            )));
        }
        Self::validate_guest_elf(&command, &bytes)?;
        Ok(Self {
            name: harness.name().to_owned(),
            driver: harness.driver().to_owned(),
            command,
            args: harness.args().to_vec(),
            executable_sha256: hex::encode(Sha256::digest(bytes)),
        })
    }

    fn resolve_guest_executable(harness: &ResolvedHarness) -> Result<PathBuf, PreparationError> {
        let command = harness.command();
        let bytes = fs::read(command).map_err(|source| PreparationError::Io {
            path: command.to_path_buf(),
            source,
        })?;
        if bytes.starts_with(b"\x7fELF") {
            return Ok(command.to_path_buf());
        }
        if harness.driver() != "codex" {
            return Ok(command.to_path_buf());
        }
        let launcher = command
            .canonicalize()
            .map_err(|source| PreparationError::Io {
                path: command.to_path_buf(),
                source,
            })?;
        let package = launcher.parent().and_then(Path::parent).ok_or_else(|| {
            PreparationError::Invalid(format!(
                "cannot locate the Codex package containing {}",
                launcher.display()
            ))
        })?;
        #[cfg(target_arch = "x86_64")]
        let platform = ("codex-linux-x64", "x86_64-unknown-linux-musl");
        #[cfg(target_arch = "aarch64")]
        let platform = ("codex-linux-arm64", "aarch64-unknown-linux-musl");
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let platform = ("unsupported", "unsupported");
        let payload = package
            .join("node_modules/@openai")
            .join(platform.0)
            .join("vendor")
            .join(platform.1)
            .join("bin/codex");
        if !payload.is_file() {
            return Err(PreparationError::Invalid(format!(
                "Codex launcher {} does not contain the guest executable {}; configure command with the exact Linux payload",
                launcher.display(),
                payload.display()
            )));
        }
        Ok(payload)
    }

    fn validate_guest_elf(path: &Path, bytes: &[u8]) -> Result<(), PreparationError> {
        let Some(header) = bytes.get(..20) else {
            return Err(PreparationError::Invalid(format!(
                "harness executable {} is too short to be an ELF binary",
                path.display()
            )));
        };
        let machine = u16::from_le_bytes([header[18], header[19]]);
        #[cfg(target_arch = "x86_64")]
        let expected = 62;
        #[cfg(target_arch = "aarch64")]
        let expected = 183;
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        let expected = u16::MAX;
        if &header[..4] != b"\x7fELF" || header[4] != 2 || header[5] != 1 || machine != expected {
            return Err(PreparationError::Invalid(format!(
                "harness executable {} is not a guest-architecture ELF binary",
                path.display()
            )));
        }
        Ok(())
    }

    /// Stable treatment name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Concrete harness driver.
    pub fn driver(&self) -> &str {
        &self.driver
    }

    /// Prepared executable path.
    pub fn command(&self) -> &Path {
        &self.command
    }

    /// Exact argv suffix.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// SHA-256 of the exact executable validated during preparation.
    pub fn executable_sha256(&self) -> &str {
        &self.executable_sha256
    }

    fn validate(&self) -> Result<(), PreparationError> {
        let bytes = fs::read(&self.command).map_err(|source| PreparationError::Io {
            path: self.command.clone(),
            source,
        })?;
        let digest = hex::encode(Sha256::digest(bytes));
        if digest != self.executable_sha256 {
            return Err(PreparationError::Invalid(format!(
                "prepared harness {} changed: expected {}, found {digest}",
                self.name, self.executable_sha256
            )));
        }
        Ok(())
    }
}

impl PreparationStore {
    /// Creates a receipt store under the evaluator state directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Publishes or reuses an immutable receipt and advances its profile pointer.
    pub fn publish(
        &self,
        receipt: &PreparationReceipt,
    ) -> Result<PublishedPreparation, PreparationError> {
        validate_storage_name("profile", receipt.profile()).map_err(PreparationError::Invalid)?;
        let bytes = serde_json::to_vec(receipt).map_err(|source| PreparationError::Json {
            path: self.root.clone(),
            source,
        })?;
        let digest = hex::encode(Sha256::digest(&bytes));
        let directory = self.root.join(receipt.profile()).join(&digest);
        let path = directory.join("receipt.json");
        if path.exists() {
            let retained = read_receipt(&path)?;
            if retained != *receipt {
                return Err(PreparationError::Invalid(format!(
                    "receipt digest collision at {}",
                    path.display()
                )));
            }
        } else {
            write_json_atomic(&path, receipt)?;
        }
        write_json_atomic(
            &self.root.join(receipt.profile()).join("current.json"),
            &CurrentPreparation {
                receipt: path.clone(),
                digest: digest.clone(),
            },
        )?;
        Ok(PublishedPreparation { path, digest })
    }

    /// Loads the current receipt for a profile and validates all task packages.
    pub fn load_current(&self, profile: &str) -> Result<PreparationReceipt, PreparationError> {
        self.open_current(profile).map(|(receipt, _)| receipt)
    }

    /// Opens the current receipt and publication identity for one profile.
    pub fn open_current(
        &self,
        profile: &str,
    ) -> Result<(PreparationReceipt, PublishedPreparation), PreparationError> {
        validate_storage_name("profile", profile).map_err(PreparationError::Invalid)?;
        let current_path = self.root.join(profile).join("current.json");
        let bytes = fs::read(&current_path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                PreparationError::Missing(profile.to_owned())
            } else {
                PreparationError::Io {
                    path: current_path.clone(),
                    source,
                }
            }
        })?;
        let current: CurrentPreparation =
            serde_json::from_slice(&bytes).map_err(|source| PreparationError::Json {
                path: current_path,
                source,
            })?;
        let expected_receipt = self
            .root
            .join(profile)
            .join(&current.digest)
            .join("receipt.json");
        if current.receipt != expected_receipt {
            return Err(PreparationError::Invalid(format!(
                "current preparation points to {}, expected {}",
                current.receipt.display(),
                expected_receipt.display()
            )));
        }
        let receipt = read_receipt(&current.receipt)?;
        if receipt.version != PREPARATION_RECEIPT_VERSION {
            return Err(PreparationError::Invalid(format!(
                "prepared receipt version {} is incompatible with evaluator version {PREPARATION_RECEIPT_VERSION}; run prepare again",
                receipt.version
            )));
        }
        let digest = hex::encode(Sha256::digest(serde_json::to_vec(&receipt).map_err(
            |source| PreparationError::Json {
                path: current.receipt.clone(),
                source,
            },
        )?));
        if digest != current.digest {
            return Err(PreparationError::Invalid(format!(
                "current preparation digest mismatch: expected {}, found {digest}",
                current.digest
            )));
        }
        let executor_sha256 = PreparationReceipt::current_executor_sha256()?;
        if receipt.executor_sha256() != executor_sha256 {
            return Err(PreparationError::Invalid(format!(
                "prepared native Nanocodex executable changed: expected {}, found {executor_sha256}; run prepare again",
                receipt.executor_sha256()
            )));
        }
        for harness in receipt.harnesses() {
            harness.validate()?;
        }
        for prepared in receipt.tasks() {
            let task = crate::Task::load(prepared.root()).map_err(|source| {
                PreparationError::Invalid(format!(
                    "prepared task {} cannot be loaded: {source}",
                    prepared.selector()
                ))
            })?;
            if task.content_digest() != prepared.digest() {
                return Err(PreparationError::Invalid(format!(
                    "prepared task {} changed: expected {}, found {}",
                    prepared.selector(),
                    prepared.digest(),
                    task.content_digest()
                )));
            }
        }
        Ok((
            receipt,
            PublishedPreparation {
                path: current.receipt,
                digest,
            },
        ))
    }
}

impl PublishedPreparation {
    /// Receipt path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Receipt content digest.
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

fn read_receipt(path: &Path) -> Result<PreparationReceipt, PreparationError> {
    let bytes = fs::read(path).map_err(|source| PreparationError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| PreparationError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<(), PreparationError> {
    let parent = path.parent().ok_or_else(|| {
        PreparationError::Invalid(format!("retained path has no parent: {}", path.display()))
    })?;
    fs::create_dir_all(parent).map_err(|source| PreparationError::Io {
        path: parent.to_path_buf(),
        source,
    })?;
    let mut temporary =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| PreparationError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    serde_json::to_writer_pretty(&mut temporary, value).map_err(|source| {
        PreparationError::Json {
            path: path.to_path_buf(),
            source,
        }
    })?;
    temporary
        .write_all(b"\n")
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| PreparationError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| PreparationError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

fn validate_profile_shape(name: &str, profile: &Profile) -> Result<(), ProfileError> {
    if profile.tasks.is_empty() {
        return Err(invalid(format!("profile {name:?} selects no tasks")));
    }
    if profile.trials == 0 {
        return Err(invalid(format!("profile {name:?} requests zero trials")));
    }
    if profile.model.is_empty() || profile.thinking.is_empty() {
        return Err(invalid(format!(
            "profile {name:?} requires non-empty `model` and `thinking` arrays"
        )));
    }
    Ok(())
}

fn ensure_unique(field: &str, values: &[String]) -> Result<(), ProfileError> {
    let mut unique = BTreeSet::new();
    for value in values {
        if !unique.insert(value) {
            return Err(invalid(format!("duplicate {field} value {value:?}")));
        }
    }
    Ok(())
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn resolve_command(root: &Path, command: &Path) -> Result<PathBuf, ProfileError> {
    if command.components().count() != 1 {
        return Ok(resolve_path(root, command));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return Err(invalid(format!(
            "cannot resolve harness command {:?} because PATH is unavailable",
            command.display()
        )));
    };
    std::env::split_paths(&path)
        .map(|directory| directory.join(command))
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            invalid(format!(
                "harness command {:?} is not on PATH",
                command.display()
            ))
        })
}

fn invalid(message: impl Into<String>) -> ProfileError {
    ProfileError::Invalid(message.into())
}

fn validate_storage_name(kind: &str, name: &str) -> Result<(), String> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\\']) {
        return Err(format!(
            "{kind} name {name:?} is not a safe state-directory key"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use nanocodex_agent::{Nanocodex, OpenAi};

    use super::*;

    #[derive(Clone, Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Benchmark {
        adapter: String,
    }

    #[test]
    fn resolves_arrays_variants_and_relative_commands() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("codex"), "fixture").unwrap();
        fs::write(
            root.path().join("nanocodex.toml"),
            r#"default = "smoke"

[harness.codex]
command = "codex"
args = ["exec"]

[harness.codex.variant.code-mode-only]
args = ["--code-mode-only"]

[benchmark.private]
adapter = "external"

[profiles.smoke]
harnesses = ["codex.code-mode-only"]
tasks = ["terminal-bench-2.1/fix-git", "private/case"]
trials = 1
model = ["gpt-5.6-sol", "gpt-5.6-terra"]
thinking = ["low", "high"]
"#,
        )
        .unwrap();
        let loaded = LoadedManifest::<Benchmark>::load(root.path().join("nanocodex.toml")).unwrap();
        let profile = loaded
            .resolve_profile(None, |name| name == "terminal-bench-2.1")
            .unwrap();

        assert_eq!(profile.models(), [Model::Sol, Model::Terra]);
        assert_eq!(profile.thinking(), [Thinking::Low, Thinking::High]);
        assert_eq!(profile.harnesses()[0].driver(), "codex");
        assert_eq!(profile.harnesses()[0].args(), ["exec", "--code-mode-only"]);
        assert_eq!(loaded.benchmark("private").unwrap().adapter, "external");
    }

    #[test]
    fn rejects_overlapping_selectors() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("nanocodex.toml"),
            r#"default = "bad"

[profiles.bad]
tasks = ["terminal-bench-2.1", "terminal-bench-2.1/fix-git"]
trials = 1
model = ["gpt-5.6-sol"]
thinking = ["low"]
"#,
        )
        .unwrap();
        let loaded = LoadedManifest::<Benchmark>::load(root.path().join("nanocodex.toml")).unwrap();
        let error = loaded
            .resolve_profile(None, |name| name == "terminal-bench-2.1")
            .unwrap_err();

        assert!(error.to_string().contains("overlapping task selector"));
    }

    #[test]
    fn rejects_profile_names_that_escape_the_state_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::write(
            root.path().join("nanocodex.toml"),
            r#"default = "../outside"

[profiles."../outside"]
tasks = ["terminal-bench-2.1"]
trials = 1
model = ["gpt-5.6-sol"]
thinking = ["low"]
"#,
        )
        .unwrap();
        let loaded = LoadedManifest::<Benchmark>::load(root.path().join("nanocodex.toml")).unwrap();
        let error = loaded
            .resolve_profile(None, |name| name == "terminal-bench-2.1")
            .unwrap_err();

        assert!(error.to_string().contains("safe state-directory key"));
    }

    #[test]
    fn preparation_store_is_idempotent_and_rejects_changed_tasks() {
        let root = tempfile::tempdir().unwrap();
        let harness = root.path().join("codex");
        let mut executable = vec![0_u8; 32];
        executable[..6].copy_from_slice(b"\x7fELF\x02\x01");
        #[cfg(target_arch = "x86_64")]
        executable[18..20].copy_from_slice(&62_u16.to_le_bytes());
        #[cfg(target_arch = "aarch64")]
        executable[18..20].copy_from_slice(&183_u16.to_le_bytes());
        fs::write(&harness, &executable).unwrap();
        let task_root = root.path().join("task");
        fs::create_dir(&task_root).unwrap();
        fs::create_dir(task_root.join("environment")).unwrap();
        fs::create_dir(task_root.join("tests")).unwrap();
        fs::write(task_root.join("instruction.md"), "Do the work.\n").unwrap();
        fs::write(task_root.join("tests/test.sh"), "exit 0\n").unwrap();
        fs::write(
            task_root.join("task.toml"),
            r#"schema_version = "1.1"
[task]
name = "smoke/task"
description = "receipt fixture"
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "alpine:3.21"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
        )
        .unwrap();
        let task = crate::Task::load(&task_root).unwrap();
        let profile = ResolvedProfile {
            name: "smoke".to_owned(),
            hosts: Vec::new(),
            harnesses: vec![ResolvedHarness {
                name: "codex".to_owned(),
                driver: "codex".to_owned(),
                command: harness.clone(),
                args: vec!["exec".to_owned()],
            }],
            selections: BTreeMap::new(),
            trials: 1,
            models: vec![Model::Sol, Model::Terra],
            thinking: vec![Thinking::Low, Thinking::High],
            web_search: false,
        };
        let receipt = PreparationReceipt::new(
            "manifest",
            &profile,
            vec![PreparedTask::new("fixture/task", &task)],
        )
        .unwrap();
        let plan = receipt
            .run_plan(&Nanocodex::builder(OpenAi::new("test-key").unwrap()))
            .unwrap();
        assert_eq!(plan.into_sweep().attempt_count(), 8);
        let plan = receipt
            .run_plan_for(
                &Nanocodex::builder(OpenAi::new("test-key").unwrap()),
                Some(&["fixture/task".to_owned()]),
            )
            .unwrap();
        assert_eq!(plan.into_sweep().attempt_count(), 8);
        let Err(error) = receipt.run_plan_for(
            &Nanocodex::builder(OpenAi::new("test-key").unwrap()),
            Some(&["missing".to_owned()]),
        ) else {
            panic!("missing rerun task must fail");
        };
        assert!(error.to_string().contains("is not in prepared profile"));
        let mut nanocodex_receipt = receipt.clone();
        nanocodex_receipt.harnesses.clear();
        let plan = nanocodex_receipt
            .run_plan(&Nanocodex::builder(OpenAi::new("test-key").unwrap()))
            .unwrap();
        assert_eq!(plan.into_sweep().attempt_count(), 4);
        let store = PreparationStore::new(root.path().join("prepared"));

        let first = store.publish(&receipt).unwrap();
        let second = store.publish(&receipt).unwrap();
        assert_eq!(first, second);
        assert_eq!(store.load_current("smoke").unwrap(), receipt);

        fs::write(&harness, "changed binary").unwrap();
        let error = store.load_current("smoke").unwrap_err();
        assert!(error.to_string().contains("prepared harness codex changed"));

        fs::write(&harness, executable).unwrap();
        fs::write(task_root.join("instruction.md"), "Changed after prepare.\n").unwrap();
        let error = store.load_current("smoke").unwrap_err();
        assert!(error.to_string().contains("changed"));
    }
}
