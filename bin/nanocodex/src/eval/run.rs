use std::{
    cmp::Reverse,
    collections::{BTreeMap, BTreeSet},
    error::Error,
    ffi::OsStr,
    fs,
    future::Future,
    io::{self, Write},
    num::ParseFloatError,
    path::{Path, PathBuf},
    pin::Pin,
    str::FromStr,
    time::{Duration, Instant, UNIX_EPOCH},
};

use arcbox_ext4::{
    Formatter, Reader,
    constants::{file_mode, make_mode},
};
use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};
use eyre::{Result, eyre};
use fs2::FileExt as _;
use nanocodex::{
    NanocodexBuilder, Thinking, Tools,
    tools::{ToolsBuildError, standard::UpdatePlanTool},
};
use nanocodex_eval::harbor::{Harbor, HarborJob, HarborRecorder};
use nanocodex_eval::{
    AttemptAgent, AttemptVerification, AttemptVerifier, EvalAttempt, EvalEnvironment,
    EvalEventKind, EvalEventStream, EvalFailure, EvalFailureKind, EvalResult, EvalStatus,
    Evaluator, EvaluatorBuilder, NetworkPolicy, Sweep, SweepResults, Task, VerifierEnvironmentMode,
    VerifierResult,
};
use nanocodex_vm::image::{CachePolicy, VmImageBuilder, reflink_or_sparse_copy};
use nanocodex_vm::{BlockDevice, GuestCommand, Network, VmConfig};
use nanocodex_vm::{
    GuestRuntimeDisk, GuestRuntimeDiskStatus, VmCommand, VmCommandOutput, VmCommandPartialOutput,
    VmToolSession, VmToolSessionError,
};
use regex::RegexSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sysinfo::{MemoryRefreshKind, RefreshKind, System};
use tokio::{process::Command, sync::watch};
use tracing::{info, info_span, warn};
use yansi::Painted;

use super::{
    config::AgentArgs,
    image::{prepare_task_image, prepare_verifier_image},
    observability::ObservabilityArgs,
    vm_network::{Gvproxy, GvproxyError, prepare_gvproxy},
};

const DEFAULT_OUTPUT_DIRECTORY: &str = ".nanocodex/evals";
const INVOCATION_FILE: &str = "invocation.json";
const LAST_RUN_FILE: &str = ".nanocodex/eval/last-run.json";
const LEGACY_LAST_RUN_FILE: &str = ".nanoeval/last-run.json";
const INVOCATION_VERSION: u32 = 2;
const PRICING_REVISION: &str = "gpt-5.6-sol-standard-priority-v1";
const SCHEDULING_POLICY: &str = "bounded_fifo_work_conserving-v1";
const DEFAULT_TRIALS: u16 = 5;
const DEFAULT_HOST_UTILIZATION_PERCENT: u8 = 80;
const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Args)]
#[group(id = "task_input", required = true, multiple = true)]
pub(crate) struct Run {
    #[command(flatten)]
    observability: ObservabilityArgs,

    /// Terminal-Bench task directory. Repeat for multiple evals in one job.
    #[arg(long = "task", value_name = "DIRECTORY", group = "task_input")]
    tasks: Vec<PathBuf>,

    /// Terminal-Bench suite directory whose immediate task children should run.
    #[arg(long = "suite", value_name = "DIRECTORY", group = "task_input")]
    suites: Vec<PathBuf>,

    #[command(flatten)]
    retry: RetryArgs,

    /// Parent directory for the retained Harbor-compatible job.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Number of fresh, independent attempts per task. Defaults to k=5.
    #[arg(
        long,
        default_value_t = DEFAULT_TRIALS,
        value_parser = clap::value_parser!(u16).range(1..)
    )]
    trials: u16,

    /// Maximum number of attempts executing at once.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    concurrency: Option<u16>,

    /// Maximum sum of task-declared memory across concurrent attempts.
    #[arg(long, value_name = "MIB", value_parser = clap::value_parser!(u64).range(1..))]
    max_memory_mb: Option<u64>,

    /// Percentage of detected host CPU and memory used for omitted scheduler limits.
    #[arg(
        long,
        default_value_t = DEFAULT_HOST_UTILIZATION_PERCENT,
        value_name = "PERCENT",
        value_parser = clap::value_parser!(u8).range(1..=100)
    )]
    host_utilization: u8,

    #[command(flatten)]
    lifecycle: RunLifecycleArgs,

    /// Print typed results as JSON instead of a human summary.
    #[arg(long)]
    json: bool,

    /// Run workspace tools inside a libkrun microVM.
    #[arg(long)]
    vm: bool,

    /// Override the prepared rootfs directory or raw ext4 image used by `--vm`.
    #[arg(long, value_name = "PATH")]
    vm_rootfs: Option<PathBuf>,

    /// Resolve the task image at the registry instead of reusing its local resolution.
    #[arg(long, requires = "vm", conflicts_with = "vm_rootfs")]
    vm_refresh: bool,

    /// Writable VM root-disk retention policy.
    #[arg(long, value_enum)]
    vm_retention: Option<VmRetention>,

    #[command(flatten)]
    agent: AgentArgs,
}

#[derive(Args)]
struct RetryArgs {
    /// Rerun unresolved tasks from the latest completed Evaluator job.
    #[arg(long, group = "task_input", conflicts_with_all = ["tasks", "suites"])]
    rerun: bool,

    /// Literal task-name substring to rerun. Repeat positional values for OR matching.
    #[arg(value_name = "NAME", requires = "rerun")]
    names: Vec<String>,

    /// Resolve the retry queue from this job instead of the latest completed job.
    #[arg(long, value_name = "JOB", requires = "rerun")]
    rerun_from: Option<PathBuf>,

    /// Advanced regular expression over full task names. Repeat for OR matching.
    #[arg(long, value_name = "REGEX", requires = "rerun")]
    match_task: Vec<String>,

    /// Print the selected task names without starting a new evaluation job.
    #[arg(long, requires = "rerun")]
    list: bool,

    #[command(flatten)]
    statuses: RetryStatusArgs,
}

#[derive(Args)]
struct RetryStatusArgs {
    /// Include typed safety refusals in the rerun selection.
    #[arg(long, requires = "rerun")]
    include_refused: bool,

    /// Include harness-errored tasks in the rerun selection.
    #[arg(long, requires = "rerun")]
    include_errored: bool,
}

#[derive(Args)]
struct RunLifecycleArgs {
    /// Start a new job even when a matching incomplete job can be resumed.
    #[arg(long = "new")]
    new_job: bool,
}

#[derive(Clone, Debug)]
struct ResolvedRun {
    task_paths: Vec<PathBuf>,
    output: PathBuf,
    trials: u16,
    concurrency: u16,
    max_memory_mb: Option<u64>,
    vm: bool,
    vm_rootfs: Option<PathBuf>,
    vm_retention: VmRetention,
    thinking: Thinking,
    web_search: bool,
    rerun_from: Option<PathBuf>,
    automatic_scheduling: Option<AutomaticScheduling>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct HostResources {
    logical_cpus: usize,
    physical_memory_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SchedulingDefaults {
    concurrency: u16,
    max_memory_mb: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ResolvedScheduling {
    concurrency: u16,
    max_memory_mb: Option<u64>,
    automatic: Option<AutomaticScheduling>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct AutomaticScheduling {
    utilization_percent: u8,
    host: HostResources,
    concurrency: bool,
    memory: bool,
}

struct RerunSelection {
    job: PathBuf,
    tasks: Vec<PathBuf>,
}

struct RetainedRetryQueue {
    task_names: BTreeSet<String>,
    unresolved_tasks: usize,
    lineage: Vec<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RunInvocation {
    version: u32,
    nanocodex_build: RetainedBuild,
    model: String,
    pricing_revision: String,
    tool_profile: String,
    seed: Option<u64>,
    scheduling: RetainedScheduling,
    trials: u16,
    concurrency: u16,
    max_memory_mb: Option<u64>,
    vm: bool,
    vm_rootfs: Option<PathBuf>,
    vm_retention: VmRetention,
    thinking: String,
    #[serde(default)]
    web_search: bool,
    rerun_from: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedBuild {
    version: String,
    git_sha: String,
    built_at: String,
    executable_sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct RetainedScheduling {
    policy: String,
    automatic_utilization_percent: Option<u8>,
    concurrency_source: String,
    memory_source: String,
}

impl RunInvocation {
    fn same_workload(&self, other: &Self) -> bool {
        self.version == other.version
            && self.nanocodex_build == other.nanocodex_build
            && self.model == other.model
            && self.pricing_revision == other.pricing_revision
            && self.tool_profile == other.tool_profile
            && self.seed == other.seed
            && self.scheduling.policy == other.scheduling.policy
            && self.trials == other.trials
            && self.vm == other.vm
            && self.vm_rootfs == other.vm_rootfs
            && self.vm_retention == other.vm_retention
            && self.thinking == other.thinking
            && self.web_search == other.web_search
            && self.rerun_from == other.rerun_from
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LastRun {
    job: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RetainedRun {
    tasks: Vec<RetainedRunTask>,
}

#[derive(Debug, Deserialize)]
struct RetainedRunTask {
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct RetainedJobIdentity {
    started_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct RetainedTrialResult {
    task_name: String,
    verifier_result: Option<RetainedVerifierResult>,
    exception_info: Option<RetainedTrialException>,
}

#[derive(Debug, Deserialize)]
struct RetainedVerifierResult {
    rewards: BTreeMap<String, f64>,
}

#[derive(Debug, Deserialize)]
struct RetainedTrialException {
    exception_type: String,
}

#[derive(Debug, Deserialize)]
struct LegacyJobConfig {
    n_concurrent_trials: usize,
    agents: Vec<LegacyAgentConfig>,
}

#[derive(Debug, Deserialize)]
struct LegacyAgentConfig {
    kwargs: LegacyAgentKwargs,
}

#[derive(Debug, Deserialize)]
struct LegacyAgentKwargs {
    effort: String,
}

impl HostResources {
    fn detect() -> Self {
        let logical_cpus = std::thread::available_parallelism().map_or(1, usize::from);
        let system = System::new_with_specifics(
            RefreshKind::nothing().with_memory(MemoryRefreshKind::nothing().with_ram()),
        );
        let physical_memory_bytes = match system.total_memory() {
            0 => None,
            bytes => Some(bytes),
        };
        Self {
            logical_cpus,
            physical_memory_bytes,
        }
    }

    fn scheduling_defaults(self, utilization_percent: u8) -> SchedulingDefaults {
        let logical_cpus = u64::try_from(self.logical_cpus).unwrap_or(u64::MAX);
        let concurrency = percentage(logical_cpus, utilization_percent).max(1);
        let concurrency = u16::try_from(concurrency).unwrap_or(u16::MAX);
        let max_memory_mb = self
            .physical_memory_bytes
            .map(|bytes| (percentage(bytes, utilization_percent) / BYTES_PER_MIB).max(1));
        SchedulingDefaults {
            concurrency,
            max_memory_mb,
        }
    }
}

const fn percentage(value: u64, percent: u8) -> u64 {
    value.saturating_mul(percent as u64) / 100
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RetainedTrialStatus {
    Passed,
    Failed,
    Refused,
    Errored,
}

impl Run {
    fn resolve_scheduling(
        &self,
        retained: Option<&RunInvocation>,
        legacy: Option<&LegacyJobConfig>,
    ) -> ResolvedScheduling {
        let host = HostResources::detect();
        let defaults = host.scheduling_defaults(self.host_utilization);
        let retained_concurrency = retained.map(|invocation| invocation.concurrency);
        let legacy_concurrency = legacy.and_then(|job| u16::try_from(job.n_concurrent_trials).ok());
        let automatic_concurrency = self.concurrency.is_none()
            && retained_concurrency.is_none()
            && legacy_concurrency.is_none();
        let concurrency = self
            .concurrency
            .or(retained_concurrency)
            .or(legacy_concurrency)
            .unwrap_or(defaults.concurrency);
        let (max_memory_mb, automatic_memory) = if let Some(memory) = self.max_memory_mb {
            (Some(memory), false)
        } else if let Some(invocation) = retained {
            (invocation.max_memory_mb, false)
        } else if legacy.is_some() {
            (None, false)
        } else {
            (defaults.max_memory_mb, defaults.max_memory_mb.is_some())
        };
        let automatic =
            (automatic_concurrency || automatic_memory).then_some(AutomaticScheduling {
                utilization_percent: self.host_utilization,
                host,
                concurrency: automatic_concurrency,
                memory: automatic_memory,
            });
        ResolvedScheduling {
            concurrency,
            max_memory_mb,
            automatic,
        }
    }

    fn resolve_run(&self) -> Result<ResolvedRun> {
        let rerun = self
            .retry
            .rerun
            .then(|| resolve_rerun_source(self))
            .transpose()?;
        let retained_invocation = match &rerun {
            Some(rerun) => load_invocation(&rerun.job)?,
            None => None,
        };
        let legacy = rerun
            .as_ref()
            .map(|rerun| load_legacy_job_config(&rerun.job))
            .transpose()?;
        let task_paths = match &rerun {
            Some(rerun) => rerun.tasks.clone(),
            None => load_task_paths(self.tasks.clone(), self.suites.clone())?,
        };
        let output = self.output.clone().unwrap_or_else(|| {
            rerun
                .as_ref()
                .and_then(|rerun| rerun.job.parent())
                .map_or_else(
                    || PathBuf::from(DEFAULT_OUTPUT_DIRECTORY),
                    Path::to_path_buf,
                )
        });
        let retained_thinking = retained_invocation
            .as_ref()
            .map(|invocation| {
                Thinking::from_str(&invocation.thinking).map_err(|error| {
                    eyre!(
                        "invalid thinking level {:?} in {INVOCATION_FILE}: {error}",
                        invocation.thinking
                    )
                })
            })
            .transpose()?;
        let legacy_thinking = legacy.as_ref().map(LegacyJobConfig::thinking).transpose()?;
        let thinking = self
            .agent
            .thinking()
            .or(retained_thinking)
            .or(legacy_thinking)
            .unwrap_or_default();
        let web_search = self
            .agent
            .web_search()
            .or_else(|| {
                retained_invocation
                    .as_ref()
                    .map(|invocation| invocation.web_search)
            })
            .unwrap_or(false);
        let vm_rootfs = self.vm_rootfs.clone().or_else(|| {
            retained_invocation
                .as_ref()
                .and_then(|invocation| invocation.vm_rootfs.clone())
        });
        let vm = self.vm
            || retained_invocation
                .as_ref()
                .is_some_and(|invocation| invocation.vm)
            || rerun
                .as_ref()
                .is_some_and(|rerun| retained_job_used_vm(&rerun.job));
        let scheduling = self.resolve_scheduling(retained_invocation.as_ref(), legacy.as_ref());
        Ok(ResolvedRun {
            task_paths,
            output,
            trials: self.trials,
            concurrency: scheduling.concurrency,
            max_memory_mb: scheduling.max_memory_mb,
            vm,
            vm_rootfs,
            vm_retention: self
                .vm_retention
                .or_else(|| {
                    retained_invocation
                        .as_ref()
                        .map(|invocation| invocation.vm_retention)
                })
                .unwrap_or_default(),
            thinking,
            web_search,
            rerun_from: rerun.map(|rerun| rerun.job),
            automatic_scheduling: scheduling.automatic,
        })
    }

    fn resolve_executable_run(&self) -> Result<Option<ResolvedRun>> {
        let resolved = self.resolve_run()?;
        if self.retry.list {
            write_task_names(&resolved.task_paths, self.json)?;
            return Ok(None);
        }
        resolved.report_automatic_scheduling();
        resolved.report_configuration();
        Ok(Some(resolved))
    }

    pub(crate) async fn run(self) -> Result<()> {
        let total_started = Instant::now();
        let Some(resolved) = self.resolve_executable_run()? else {
            return Ok(());
        };
        let observability_started = Instant::now();
        let _observability = self.observability.install()?;
        let observability = observability_started.elapsed();
        let (tasks, task_loading) =
            load_prioritized_tasks(resolved.task_paths.clone(), &resolved.output)?;
        let (vmm, runtime_image, vm_runtime) =
            prepare_run_vm(resolved.vm, resolved.vm_rootfs.as_deref()).await?;
        let gvproxy =
            prepare_task_network(resolved.vm || resolved.vm_rootfs.is_some(), &tasks).await?;
        let vm_environments_started = Instant::now();
        let vm_environments = prepare_run_environments(
            &tasks,
            &resolved,
            self.vm_refresh,
            &vmm,
            &runtime_image,
            gvproxy.as_deref(),
        )
        .await?;
        let vm_environments_duration = vm_environments_started.elapsed();
        let evaluation_setup_started = Instant::now();
        let new_job = self.lifecycle.new_job;
        let nanocodex = self.agent.builder(resolved.thinking, resolved.web_search)?;
        let (mut evaluator, sweep, attempt_count) =
            Self::build_evaluator(&resolved, tasks, nanocodex, new_job)?;
        if let Some(environments) = vm_environments {
            evaluator = evaluator
                .attempt_environment(EvalEnvironment::MicroVm)
                .attempt_agent(move |attempt, builder| {
                    let environment = environments.get(attempt.task().root()).ok_or_else(|| {
                        VmAttemptError::MissingPreparedEnvironment(
                            attempt.task().root().to_path_buf(),
                        )
                    })?;
                    let runtime = vm_attempt(
                        environment,
                        VmAttemptHost {
                            runtime_image: &runtime_image,
                            vmm: &vmm,
                            gvproxy: gvproxy.as_deref(),
                            retain_passed_rootfs: resolved.vm_retention.retains_passes(),
                            web_search: resolved.web_search,
                        },
                        attempt,
                    )?;
                    let readiness = runtime
                        .verifier
                        .agent_session
                        .as_ref()
                        .ok_or(VmAttemptError::AgentSessionAlreadyFinished)?
                        .handle();
                    Ok::<_, VmAttemptError>(
                        AttemptAgent::new(builder.tools(runtime.tools))
                            .ready(async move { readiness.ready().await })
                            .verifier(runtime.verifier),
                    )
                });
        }
        let (eval, events) = evaluator.build()?;
        persist_invocation(eval.directory(), &resolved.invocation()?)?;
        let remaining_attempts = eval.remaining_attempts(&sweep)?;
        let skipped_attempts = attempt_count.saturating_sub(remaining_attempts);
        report_resume(&eval, skipped_attempts, attempt_count);
        let harbor = Harbor::new(&eval)?.record(events.subscribe())?;
        let (expected_attempts, expected_attempts_rx) = watch::channel(remaining_attempts);
        let progress = tokio::spawn(report_progress(
            events.subscribe(),
            expected_attempts_rx,
            usize::from(resolved.concurrency),
            resolved.max_memory_mb,
        ));
        let evaluation_setup = evaluation_setup_started.elapsed();
        let attempts_started = Instant::now();
        let execution = finish_or_drain(
            eval.sweep(sweep),
            tokio::signal::ctrl_c(),
            remaining_attempts,
            || {
                let admitted = eval.begin_drain();
                eprintln!(
                    "Interrupt received; stopped admitting new trials after {admitted} \
                     attempt(s), draining admitted work"
                );
                admitted
            },
        )
        .await?;
        expected_attempts.send_replace(execution.terminal_attempts);
        drop(expected_attempts);
        let attempts = attempts_started.elapsed();
        let finished = finish_evaluation(
            harbor,
            execution.terminal_attempts,
            progress,
            execution.result,
        )
        .await?;
        let output_started = Instant::now();
        persist_aggregate(&finished.job)?;
        Self::write_report(
            &finished.job,
            finished.outcomes,
            skipped_attempts,
            self.json,
        )?;
        let output = output_started.elapsed();
        let measurements = RunMeasurements {
            observability,
            task_loading,
            vm_runtime,
            vm_environments: vm_environments_duration,
            evaluation_setup,
            attempts,
            harbor_finish: finished.harbor_finish,
            output,
            total: total_started.elapsed(),
        };
        measurements.persist(finished.job.directory())?;
        measurements.record(&finished.results, attempt_count, finished.failed);
        if !execution.interrupted {
            record_last_run(finished.job.directory())?;
        }
        finish_run(finished.run_error)?;
        if execution.interrupted {
            return Err(eyre!(
                "evaluation interrupted after draining admitted attempts; rerun the same \
                 workload to resume {}",
                finished.job.directory().display()
            ));
        }
        Ok(())
    }

    fn build_evaluator(
        resolved: &ResolvedRun,
        tasks: Vec<Task>,
        nanocodex: NanocodexBuilder,
        new_job: bool,
    ) -> Result<(EvaluatorBuilder, Sweep, usize)> {
        let sweep = Sweep::builder()
            .tasks(tasks)
            .trials(resolved.trials)
            .agent("default", nanocodex.clone())?
            .build()?;
        let attempt_count = sweep.attempt_count();
        let evaluator = Evaluator::builder(nanocodex)
            .output_directory(&resolved.output)
            .max_concurrency(usize::from(resolved.concurrency));
        let evaluator = configure_memory_limit(evaluator, resolved.max_memory_mb);
        let evaluator = bind_finite_run(evaluator, &sweep, new_job);
        Ok((evaluator, sweep, attempt_count))
    }

    fn write_report(
        job: &HarborJob,
        outcomes: Vec<AttemptOutcome>,
        skipped: usize,
        json: bool,
    ) -> Result<()> {
        let report = RunReport::new(job, outcomes, skipped);
        if json {
            serde_json::to_writer_pretty(io::stdout().lock(), &report)?;
            println!();
        } else {
            Self::write_summary(&report);
        }
        Ok(())
    }

    fn write_summary(report: &RunReport) {
        println!(
            "\nResult: {} passed; {} failed; {} refused; {} errored; {} total",
            Painted::new(report.summary.passed).green(),
            Painted::new(report.summary.failed).red(),
            Painted::new(report.summary.refused).yellow(),
            Painted::new(report.summary.errored).red(),
            report.summary.total
        );
        println!("Harbor job: {}", report.job_directory.display());
        match report.summary.known_estimated_cost_usd {
            Some(cost) => println!(
                "Known estimated cost: ${cost:.6} ({} of {} attempt{} priced)",
                report.summary.priced_attempts,
                report.summary.total,
                if report.summary.total == 1 { "" } else { "s" }
            ),
            None => {
                println!("Estimated cost: unavailable (provider usage was unavailable or unpriced)")
            }
        }
        if report.skipped > 0 {
            println!(
                "Resumed: {} previously completed attempt{} retained",
                report.skipped,
                if report.skipped == 1 { "" } else { "s" }
            );
        }
        if report.summary.failed + report.summary.refused + report.summary.errored > 0 {
            println!(
                "Inspect failures: nanocodex eval inspect {}",
                report.job_directory.display()
            );
        }
    }
}

fn persist_aggregate(job: &HarborJob) -> Result<()> {
    write_json_atomic(
        &job.directory().join("aggregate.json"),
        &job.aggregate_dataset()?,
    )
}

impl ResolvedRun {
    fn report_configuration(&self) {
        let environment = if self.vm { "microVM" } else { "host" };
        eprintln!(
            "Run config: thinking={} · trials={} · concurrency={} · environment={environment} · web_search={}",
            self.thinking, self.trials, self.concurrency, self.web_search
        );
    }

    fn report_automatic_scheduling(&self) {
        let Some(automatic) = self.automatic_scheduling else {
            return;
        };
        let memory = automatic.host.physical_memory_bytes.map_or_else(
            || "unknown RAM".to_owned(),
            |bytes| format!("{} MiB RAM", bytes / BYTES_PER_MIB),
        );
        let concurrency_source = if automatic.concurrency {
            "automatic"
        } else {
            "configured"
        };
        let memory_source = if automatic.memory {
            "automatic"
        } else {
            "configured"
        };
        let max_memory = self
            .max_memory_mb
            .map_or_else(|| "unbounded".to_owned(), |memory| format!("{memory} MiB"));
        eprintln!(
            "Host scheduling: target={}%, detected={} logical CPUs/{memory}, \
             concurrency={} ({concurrency_source}), memory={max_memory} ({memory_source})",
            automatic.utilization_percent, automatic.host.logical_cpus, self.concurrency,
        );
    }

    fn invocation(&self) -> Result<RunInvocation> {
        let scheduling = self.automatic_scheduling;
        let executable = std::env::current_exe()?;
        let executable_sha256 = hex::encode(Sha256::digest(fs::read(&executable)?));
        Ok(RunInvocation {
            version: INVOCATION_VERSION,
            nanocodex_build: RetainedBuild {
                version: env!("NANOCODEX_SEMVER_VERSION").to_owned(),
                git_sha: env!("VERGEN_GIT_SHA").to_owned(),
                built_at: env!("VERGEN_BUILD_TIMESTAMP").to_owned(),
                executable_sha256,
            },
            model: nanocodex::oai::MODEL.to_owned(),
            pricing_revision: PRICING_REVISION.to_owned(),
            tool_profile: if self.vm || self.vm_rootfs.is_some() {
                "microvm_workspace".to_owned()
            } else {
                "native_workspace".to_owned()
            },
            seed: None,
            scheduling: RetainedScheduling {
                policy: SCHEDULING_POLICY.to_owned(),
                automatic_utilization_percent: scheduling
                    .map(|automatic| automatic.utilization_percent),
                concurrency_source: if scheduling.is_some_and(|automatic| automatic.concurrency) {
                    "automatic"
                } else {
                    "configured"
                }
                .to_owned(),
                memory_source: if scheduling.is_some_and(|automatic| automatic.memory) {
                    "automatic"
                } else {
                    "configured"
                }
                .to_owned(),
            },
            trials: self.trials,
            concurrency: self.concurrency,
            max_memory_mb: self.max_memory_mb,
            vm: self.vm,
            vm_rootfs: self.vm_rootfs.clone(),
            vm_retention: self.vm_retention,
            thinking: self.thinking.to_string(),
            web_search: self.web_search,
            rerun_from: self.rerun_from.clone(),
        })
    }
}

impl LegacyJobConfig {
    fn thinking(&self) -> Result<Thinking> {
        let effort = self
            .agents
            .first()
            .ok_or_else(|| eyre!("retained job config contains no agent"))?
            .kwargs
            .effort
            .as_str();
        Thinking::from_str(effort).map_err(|error| eyre!(error))
    }
}

fn resolve_rerun_source(eval: &Run) -> Result<RerunSelection> {
    let job = match &eval.retry.rerun_from {
        Some(job) => resolve_job_path(job, eval.output.as_deref())?,
        None => latest_completed_job(eval.output.as_deref())?,
    };
    if !job.join("result.json").is_file() {
        return Err(eyre!(
            "rerun source is not a completed Evaluator job: {}",
            job.display()
        ));
    }
    let matcher = retry_matcher(&eval.retry)?;
    let queue = retained_retry_task_names(
        &job,
        eval.retry.statuses.include_refused,
        eval.retry.statuses.include_errored,
        matcher.as_ref(),
    )?;
    let tasks = retained_retry_task_roots(&queue.lineage, &queue.task_names)?;
    if tasks.is_empty() {
        let filter = if eval.retry.match_task.is_empty() && eval.retry.names.is_empty() {
            String::new()
        } else {
            format!(
                " matching names {:?} or regular expressions {:?}",
                eval.retry.names, eval.retry.match_task
            )
        };
        return Err(eyre!(
            "no unresolved tasks{filter}; inspect the queue with \
             `nanocodex eval --rerun --list`"
        ));
    }
    eprintln!(
        "{}",
        retry_selection_summary(eval, &queue, &job, tasks.len())
    );
    if !eval.retry.list && !eval.json {
        for task in &tasks {
            eprintln!("  {}", short_task_name(Task::load(task)?.name()));
        }
    }
    Ok(RerunSelection { job, tasks })
}

fn retry_matcher(retry: &RetryArgs) -> Result<Option<RegexSet>> {
    let mut patterns = retry.match_task.clone();
    patterns.extend(retry.names.iter().map(|name| regex::escape(name)));
    (!patterns.is_empty())
        .then(|| RegexSet::new(patterns))
        .transpose()
        .map_err(Into::into)
}

fn retry_selection_summary(
    eval: &Run,
    queue: &RetainedRetryQueue,
    job: &Path,
    selected: usize,
) -> String {
    let run = if queue.lineage.len() == 1 {
        "run"
    } else {
        "runs"
    };
    let job = job
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("<job>");
    if eval.retry.list {
        if selected == queue.unresolved_tasks {
            format!(
                "{} unresolved task{} across {} {run} (latest {job})",
                queue.unresolved_tasks,
                if queue.unresolved_tasks == 1 { "" } else { "s" },
                queue.lineage.len()
            )
        } else {
            format!(
                "{selected} selected of {} unresolved tasks across {} {run} (latest {job})",
                queue.unresolved_tasks,
                queue.lineage.len()
            )
        }
    } else {
        format!(
            "Retrying {selected} of {} unresolved task{} across {} {run} (latest {job})",
            queue.unresolved_tasks,
            if queue.unresolved_tasks == 1 { "" } else { "s" },
            queue.lineage.len()
        )
    }
}

fn write_task_names(tasks: &[PathBuf], json: bool) -> Result<()> {
    let names = tasks
        .iter()
        .map(|task| Task::load(task).map(|task| task.name().to_owned()))
        .collect::<Result<Vec<_>, _>>()?;
    if json {
        serde_json::to_writer_pretty(io::stdout().lock(), &names)?;
        println!();
    } else {
        for name in names {
            println!("{}", short_task_name(&name));
        }
    }
    Ok(())
}

fn short_task_name(name: &str) -> &str {
    name.rsplit_once('/').map_or(name, |(_, name)| name)
}

fn latest_completed_job(output: Option<&Path>) -> Result<PathBuf> {
    if let Some(job) = completed_job_from_last_run(
        output,
        [Path::new(LAST_RUN_FILE), Path::new(LEGACY_LAST_RUN_FILE)],
    ) {
        return Ok(job);
    }
    let current = std::env::current_dir()?;
    let mut roots = vec![output.map_or_else(|| current.clone(), Path::to_path_buf)];
    if output.is_none() {
        roots.extend(
            fs::read_dir(&current)?
                .filter_map(Result::ok)
                .filter_map(|entry| entry.file_type().ok()?.is_dir().then_some(entry.path())),
        );
    }
    let mut candidates = Vec::new();
    for root in roots {
        collect_completed_job(&root, &mut candidates);
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                collect_completed_job(&entry.path(), &mut candidates);
            }
        }
    }
    candidates.sort_unstable_by_key(|(started_at, _)| *started_at);
    candidates.pop().map(|(_, job)| job).ok_or_else(|| {
        eyre!("no completed Evaluator job was found; run an eval or pass --rerun-from <JOB>")
    })
}

fn completed_job_from_last_run<'a>(
    output: Option<&Path>,
    last_run_files: impl IntoIterator<Item = &'a Path>,
) -> Option<PathBuf> {
    for last_run in last_run_files {
        if let Ok(retained) = read_json::<LastRun>(last_run)
            && let Ok(job) = resolve_job_path(&retained.job, output)
            && job.join("result.json").is_file()
        {
            return Some(job);
        }
    }
    None
}

fn collect_completed_job(directory: &Path, candidates: &mut Vec<(DateTime<Utc>, PathBuf)>) {
    if !directory.join("result.json").is_file() || !directory.join("run.json").is_file() {
        return;
    }
    let Ok(identity) = read_json::<RetainedJobIdentity>(&directory.join("job.json")) else {
        return;
    };
    let Ok(directory) = fs::canonicalize(directory) else {
        return;
    };
    candidates.push((identity.started_at, directory));
}

fn resolve_job_path(job: &Path, output: Option<&Path>) -> Result<PathBuf> {
    let candidate = if job.is_dir() {
        job.to_path_buf()
    } else if job.components().count() == 1 {
        output
            .unwrap_or_else(|| Path::new(DEFAULT_OUTPUT_DIRECTORY))
            .join(job)
    } else {
        job.to_path_buf()
    };
    fs::canonicalize(&candidate).map_err(|error| {
        eyre!(
            "Evaluator job does not exist: {}: {error}",
            candidate.display()
        )
    })
}

fn retained_retry_task_names(
    job: &Path,
    include_refused: bool,
    include_errored: bool,
    matcher: Option<&RegexSet>,
) -> Result<RetainedRetryQueue> {
    let lineage = retained_retry_lineage(job)?;
    let mut statuses = BTreeMap::new();
    for job in &lineage {
        for (task_name, status) in retained_task_statuses(job)? {
            statuses.insert(task_name, status);
        }
    }
    let retryable_names = statuses
        .into_iter()
        .filter_map(|(task_name, status)| {
            let retryable = match status {
                RetainedTrialStatus::Failed => true,
                RetainedTrialStatus::Refused => include_refused,
                RetainedTrialStatus::Errored => include_errored,
                RetainedTrialStatus::Passed => false,
            };
            retryable.then_some(task_name)
        })
        .collect::<BTreeSet<_>>();
    let unresolved_tasks = retryable_names.len();
    let task_names = retryable_names
        .into_iter()
        .filter(|task_name| matcher.is_none_or(|matcher| matcher.is_match(task_name)))
        .collect();
    Ok(RetainedRetryQueue {
        task_names,
        unresolved_tasks,
        lineage,
    })
}

fn retained_retry_task_roots(
    lineage: &[PathBuf],
    selected_names: &BTreeSet<String>,
) -> Result<Vec<PathBuf>> {
    let mut roots = BTreeMap::new();
    for job in lineage {
        let retained: RetainedRun = read_json(&job.join("run.json"))?;
        for retained_task in retained.tasks {
            let task = Task::load(&retained_task.root)?;
            if selected_names.contains(task.name()) {
                roots.insert(task.name().to_owned(), retained_task.root);
            }
        }
    }
    let missing = selected_names
        .iter()
        .filter(|name| !roots.contains_key(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(eyre!(
            "retry lineage does not retain task definitions for {}",
            missing.join(", ")
        ));
    }
    Ok(roots.into_values().collect())
}

fn retained_retry_lineage(job: &Path) -> Result<Vec<PathBuf>> {
    let mut current = fs::canonicalize(job)?;
    let mut seen = BTreeSet::new();
    let mut lineage = Vec::new();
    loop {
        if !seen.insert(current.clone()) {
            return Err(eyre!(
                "retry lineage contains a cycle at {}",
                current.display()
            ));
        }
        lineage.push(current.clone());
        let Some(parent) = load_invocation(&current)?.and_then(|invocation| invocation.rerun_from)
        else {
            break;
        };
        current = fs::canonicalize(&parent).map_err(|error| {
            eyre!(
                "retry parent {} recorded by {} is unavailable: {error}",
                parent.display(),
                current.join(INVOCATION_FILE).display()
            )
        })?;
    }
    lineage.reverse();
    Ok(lineage)
}

fn retained_task_statuses(job: &Path) -> Result<BTreeMap<String, RetainedTrialStatus>> {
    let mut statuses: BTreeMap<String, RetainedTrialStatus> = BTreeMap::new();
    for entry in fs::read_dir(job)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let result_path = entry.path().join("result.json");
        if !result_path.is_file() {
            continue;
        }
        let result: RetainedTrialResult = read_json(&result_path)?;
        let status = result.status();
        statuses
            .entry(result.task_name)
            .and_modify(|retained| *retained = retained.merge(status))
            .or_insert(status);
    }
    Ok(statuses)
}

impl RetainedTrialResult {
    fn status(&self) -> RetainedTrialStatus {
        if let Some(exception) = &self.exception_info {
            return if exception.exception_type == "AgentSafetyRefusalError" {
                RetainedTrialStatus::Refused
            } else {
                RetainedTrialStatus::Errored
            };
        }
        if self
            .verifier_result
            .as_ref()
            .is_some_and(|verifier| verifier.rewards.values().all(|reward| *reward > 0.0))
        {
            RetainedTrialStatus::Passed
        } else {
            RetainedTrialStatus::Failed
        }
    }
}

impl RetainedTrialStatus {
    const fn merge(self, other: Self) -> Self {
        match (self, other) {
            (Self::Passed, _) | (_, Self::Passed) => Self::Passed,
            (Self::Failed, _) | (_, Self::Failed) => Self::Failed,
            (Self::Refused, _) | (_, Self::Refused) => Self::Refused,
            (Self::Errored, Self::Errored) => Self::Errored,
        }
    }
}

fn load_invocation(job: &Path) -> Result<Option<RunInvocation>> {
    let path = job.join(INVOCATION_FILE);
    match fs::read(&path) {
        Ok(contents) => {
            let invocation: RunInvocation = serde_json::from_slice(&contents)?;
            if invocation.version != INVOCATION_VERSION {
                return Err(eyre!(
                    "unsupported Evaluator invocation version {} in {}",
                    invocation.version,
                    path.display()
                ));
            }
            Ok(Some(invocation))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn load_legacy_job_config(job: &Path) -> Result<LegacyJobConfig> {
    read_json(&job.join("config.json"))
}

fn retained_job_used_vm(job: &Path) -> bool {
    fs::read_dir(job).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| entry.path().join("rootfs.ext4").is_file())
    })
}

fn persist_invocation(job: &Path, invocation: &RunInvocation) -> Result<()> {
    let path = job.join(INVOCATION_FILE);
    if path.is_file() {
        let retained: RunInvocation = read_json(&path)?;
        if retained == *invocation {
            return Ok(());
        }
        if !retained.same_workload(invocation) {
            return Err(eyre!(
                "retry invocation conflicts with durable {}",
                path.display()
            ));
        }
        info!(
            target: "nanocodex_eval",
            previous_concurrency = retained.concurrency,
            concurrency = invocation.concurrency,
            previous_max_memory_mb = retained.max_memory_mb,
            max_memory_mb = invocation.max_memory_mb,
            "updated scheduling for resumed evaluation"
        );
    }
    write_json_atomic(&path, invocation)
}

fn record_last_run(job: &Path) -> Result<()> {
    let job = fs::canonicalize(job)?;
    write_json_atomic(Path::new(LAST_RUN_FILE), &LastRun { job })
}

fn read_json<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| eyre!("JSON path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(&mut temporary, value)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .map(|_| ())
        .map_err(Into::into)
}

fn report_resume(eval: &Evaluator, skipped: usize, total: usize) {
    if eval.resumed() {
        eprintln!(
            "Resuming Harbor job {} ({skipped} of {total} attempts already durable)",
            eval.directory().display(),
        );
    }
}

fn finish_run(error: Option<nanocodex_eval::EvalError>) -> Result<()> {
    error.map_or(Ok(()), |error| Err(error.into()))
}

fn load_prioritized_tasks(
    task_paths: Vec<PathBuf>,
    output: &Path,
) -> Result<(Vec<Task>, Duration)> {
    let started_at = Instant::now();
    let mut tasks = load_tasks(task_paths, Vec::new())?;
    prioritize_tasks(&mut tasks, output)?;
    Ok((tasks, started_at.elapsed()))
}

async fn prepare_run_vm(vm: bool, rootfs: Option<&Path>) -> Result<(PathBuf, PathBuf, Duration)> {
    let vmm = std::env::current_exe()?;
    let started_at = Instant::now();
    let runtime = prepare_runtime_for_vm(vm, rootfs).await?;
    Ok((vmm, runtime, started_at.elapsed()))
}

struct FinishedEvaluation {
    job: HarborJob,
    outcomes: Vec<AttemptOutcome>,
    results: Vec<EvalResult>,
    run_error: Option<nanocodex_eval::EvalError>,
    failed: usize,
    harbor_finish: Duration,
}

struct DrainExecution<T, E> {
    result: Result<T, E>,
    terminal_attempts: usize,
    interrupted: bool,
}

async fn finish_or_drain<T, E, Work, Shutdown, Drain>(
    work: Work,
    shutdown: Shutdown,
    terminal_attempts: usize,
    drain: Drain,
) -> io::Result<DrainExecution<T, E>>
where
    Work: Future<Output = Result<T, E>>,
    Shutdown: Future<Output = io::Result<()>>,
    Drain: FnOnce() -> usize,
{
    tokio::pin!(work);
    tokio::pin!(shutdown);
    tokio::select! {
        result = &mut work => Ok(DrainExecution {
            result,
            terminal_attempts,
            interrupted: false,
        }),
        signal = &mut shutdown => {
            signal?;
            let terminal_attempts = drain();
            let result = work.await;
            Ok(DrainExecution {
                result,
                terminal_attempts,
                interrupted: true,
            })
        }
    }
}

async fn finish_evaluation(
    harbor: HarborRecorder,
    remaining_attempts: usize,
    progress: tokio::task::JoinHandle<Result<Progress>>,
    sweep_result: Result<SweepResults, nanocodex_eval::EvalError>,
) -> Result<FinishedEvaluation> {
    let started_at = Instant::now();
    let job = harbor.finish_all(remaining_attempts).await?;
    let progress = progress.await??;
    let (results, run_error) = match sweep_result {
        Ok(results) => (results.into_results(), None),
        Err(error) => (progress.scored_results(), Some(error)),
    };
    Ok(FinishedEvaluation {
        job,
        outcomes: progress.outcomes,
        results,
        run_error,
        failed: progress.failed,
        harbor_finish: started_at.elapsed(),
    })
}

fn bind_finite_run(evaluator: EvaluatorBuilder, sweep: &Sweep, fresh: bool) -> EvaluatorBuilder {
    if fresh {
        evaluator.fresh_run(sweep)
    } else {
        evaluator.resume_incomplete(sweep)
    }
}

const fn configure_memory_limit(
    evaluator: EvaluatorBuilder,
    max_memory_mb: Option<u64>,
) -> EvaluatorBuilder {
    match max_memory_mb {
        Some(max_memory_mb) => evaluator.max_memory_mb(max_memory_mb),
        None => evaluator,
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum VmRetention {
    /// Retain disks only for failures, refusals, and errors.
    #[default]
    Failures,
    /// Retain disks for every attempt, including passes.
    All,
}

impl VmRetention {
    const fn retains_passes(self) -> bool {
        matches!(self, Self::All)
    }
}

fn load_tasks(paths: Vec<PathBuf>, suites: Vec<PathBuf>) -> Result<Vec<Task>> {
    load_task_paths(paths, suites)?
        .into_iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn prioritize_tasks(tasks: &mut [Task], output: &Path) -> Result<()> {
    let estimates = retained_task_durations(output)?;
    tasks.sort_by_key(|task| {
        let declared_floor = task
            .agent_timeout()
            .div_f64(4.0)
            .min(Duration::from_mins(10));
        let estimate = estimates
            .get(task.name())
            .copied()
            .unwrap_or(declared_floor);
        Reverse((estimate, task.agent_timeout(), task.verifier().timeout()))
    });
    Ok(())
}

fn retained_task_durations(output: &Path) -> Result<BTreeMap<String, Duration>> {
    let jobs = match fs::read_dir(output) {
        Ok(jobs) => jobs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(error.into()),
    };
    let mut samples = BTreeMap::<String, Vec<Duration>>::new();
    for job in jobs {
        let job = job?;
        if !job.file_type()?.is_dir() {
            continue;
        }
        for trial in fs::read_dir(job.path())? {
            let trial = trial?;
            if !trial.file_type()?.is_dir() {
                continue;
            }
            let Ok(bytes) = fs::read(trial.path().join("result.json")) else {
                continue;
            };
            let Ok(result) = serde_json::from_slice::<RetainedTrialTiming>(&bytes) else {
                continue;
            };
            if result.exception_info.as_ref().is_some_and(|exception| {
                matches!(
                    exception.exception_type.as_str(),
                    "EnvironmentError" | "VerifierError" | "NanocodexEvalError" | "NanoevalError"
                )
            }) {
                continue;
            }
            let Ok(duration) = result
                .finished_at
                .signed_duration_since(result.started_at)
                .to_std()
            else {
                continue;
            };
            samples.entry(result.task_name).or_default().push(duration);
        }
    }
    Ok(samples
        .into_iter()
        .map(|(task, mut durations)| {
            durations.sort_unstable();
            let median = durations[durations.len() / 2];
            (task, median)
        })
        .collect())
}

#[derive(Deserialize)]
struct RetainedTrialTiming {
    task_name: String,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    exception_info: Option<RetainedException>,
}

#[derive(Deserialize)]
struct RetainedException {
    exception_type: String,
}

pub(crate) fn load_task_paths(
    mut paths: Vec<PathBuf>,
    suites: Vec<PathBuf>,
) -> Result<Vec<PathBuf>> {
    for suite in suites {
        let mut suite_tasks = fs::read_dir(&suite)?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir() && path.join("task.toml").is_file())
            .collect::<Vec<_>>();
        suite_tasks.sort();
        if suite_tasks.is_empty() {
            return Err(eyre!(
                "suite contains no immediate task directories: {}",
                suite.display()
            ));
        }
        paths.extend(suite_tasks);
    }
    Ok(paths)
}

async fn prepare_network_for_vm(enabled: bool) -> Result<Option<PathBuf>> {
    if enabled {
        Ok(Some(prepare_gvproxy(Path::new(DEFAULT_VM_CACHE)).await?))
    } else {
        Ok(None)
    }
}

async fn prepare_task_network(vm_enabled: bool, tasks: &[Task]) -> Result<Option<PathBuf>> {
    let public = tasks
        .iter()
        .any(|task| task.network() == NetworkPolicy::Public);
    prepare_network_for_vm(vm_enabled && public).await
}

async fn selected_vm_environments(
    tasks: &[Task],
    vm: bool,
    rootfs: Option<PathBuf>,
    refresh: bool,
    vmm: &Path,
    runtime_image: &Path,
) -> Result<Option<BTreeMap<PathBuf, VmEnvironment>>> {
    if let Some(rootfs) = rootfs {
        let workspace = if rootfs.is_file() {
            "/app"
        } else {
            "/workspace"
        };
        let environment = VmEnvironment {
            rootfs,
            workspace: workspace.to_owned(),
            environment: BTreeMap::new(),
            shell: "bash".to_owned(),
            verifier: None,
        };
        return Ok(Some(
            tasks
                .iter()
                .map(|task| (task.root().to_path_buf(), environment.clone()))
                .collect(),
        ));
    }
    if !vm {
        return Ok(None);
    }
    let policy = if refresh {
        CachePolicy::Refresh
    } else {
        CachePolicy::Reuse
    };
    let image_builder = eval_vm_image_builder(vmm, runtime_image);
    Ok(Some(
        prepare_vm_environments(tasks, Path::new(DEFAULT_VM_CACHE), policy, &image_builder).await?,
    ))
}

fn eval_vm_image_builder(vmm: &Path, runtime_image: &Path) -> VmImageBuilder {
    EVAL_IMAGE_BUILD_POLICY.apply(
        VmImageBuilder::new(vmm, runtime_image)
            .vmm_args(["eval", "vm", "run-config", "--config"])
            .firmware_directory(DEFAULT_KRUNFW_DIRECTORY),
    )
}

async fn prepare_run_environments(
    tasks: &[Task],
    resolved: &ResolvedRun,
    refresh: bool,
    vmm: &Path,
    runtime_image: &Path,
    gvproxy: Option<&Path>,
) -> Result<Option<BTreeMap<PathBuf, VmEnvironment>>> {
    let environments = selected_vm_environments(
        tasks,
        resolved.vm,
        resolved.vm_rootfs.clone(),
        refresh,
        vmm,
        runtime_image,
    )
    .await?;
    prepare_selected_verifier_caches(tasks, environments.as_ref(), vmm, runtime_image, gvproxy)
        .await?;
    Ok(environments)
}

struct RunMeasurements {
    observability: Duration,
    task_loading: Duration,
    vm_runtime: Duration,
    vm_environments: Duration,
    evaluation_setup: Duration,
    attempts: Duration,
    harbor_finish: Duration,
    output: Duration,
    total: Duration,
}

#[derive(Serialize)]
struct RetainedRunMeasurements {
    schema_version: u32,
    observability_ns: u64,
    task_loading_ns: u64,
    vm_runtime_build_ns: u64,
    cold_image_and_cache_ns: u64,
    evaluation_setup_ns: u64,
    attempts_wall_ns: u64,
    harbor_finish_ns: u64,
    output_ns: u64,
    total_wall_ns: u64,
}

impl RunMeasurements {
    fn persist(&self, job: &Path) -> Result<()> {
        write_json_atomic(
            &job.join("timing.json"),
            &RetainedRunMeasurements {
                schema_version: 1,
                observability_ns: duration_ns(self.observability),
                task_loading_ns: duration_ns(self.task_loading),
                vm_runtime_build_ns: duration_ns(self.vm_runtime),
                cold_image_and_cache_ns: duration_ns(self.vm_environments),
                evaluation_setup_ns: duration_ns(self.evaluation_setup),
                attempts_wall_ns: duration_ns(self.attempts),
                harbor_finish_ns: duration_ns(self.harbor_finish),
                output_ns: duration_ns(self.output),
                total_wall_ns: duration_ns(self.total),
            },
        )
    }

    fn record(&self, results: &[EvalResult], attempt_count: usize, errored_attempt_count: usize) {
        let model_ns = results
            .iter()
            .map(|result| result.agent.metadata.model_duration_ns)
            .sum::<u64>();
        let warmup_ns = results
            .iter()
            .map(|result| result.agent.metadata.warmup_duration_ns)
            .sum::<u64>();
        let tool_work_ns = results
            .iter()
            .map(|result| result.agent.metadata.tool_work_duration_ns)
            .sum::<u64>();
        let tool_wall_ns = results
            .iter()
            .map(|result| result.agent.metadata.tool_wall_duration_ns)
            .sum::<u64>();
        let verifier_ns = results
            .iter()
            .map(|result| {
                result
                    .timing
                    .verifier
                    .finished_at
                    .signed_duration_since(result.timing.verifier.started_at)
                    .num_nanoseconds()
                    .and_then(|duration| u64::try_from(duration).ok())
                    .unwrap_or_default()
            })
            .sum::<u64>();
        let response_retries = results
            .iter()
            .map(|result| u64::from(result.agent.metadata.response_retries))
            .sum::<u64>();
        let cached_input_tokens = results
            .iter()
            .map(|result| result.agent.usage.cached_input_tokens)
            .sum::<u64>();
        let input_tokens = results
            .iter()
            .map(|result| result.agent.usage.input_tokens)
            .sum::<u64>();
        info!(
            target: "nanocodex_eval",
            duration_ns = duration_ns(self.total),
            observability_duration_ns = duration_ns(self.observability),
            task_loading_duration_ns = duration_ns(self.task_loading),
            vm_runtime_duration_ns = duration_ns(self.vm_runtime),
            vm_environments_duration_ns = duration_ns(self.vm_environments),
            evaluation_setup_duration_ns = duration_ns(self.evaluation_setup),
            attempts_wall_duration_ns = duration_ns(self.attempts),
            harbor_finish_duration_ns = duration_ns(self.harbor_finish),
            output_duration_ns = duration_ns(self.output),
            attempt_count,
            scored_attempt_count = results.len(),
            errored_attempt_count,
            attempts_model_duration_ns = model_ns,
            attempts_warmup_duration_ns = warmup_ns,
            attempts_tool_work_duration_ns = tool_work_ns,
            attempts_tool_wall_duration_ns = tool_wall_ns,
            attempts_verifier_duration_ns = verifier_ns,
            response_retries,
            input_tokens,
            cached_input_tokens,
            "evaluation run completed"
        );
    }
}

fn duration_ns(duration: Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

async fn prepare_vm_environments(
    tasks: &[Task],
    cache: &Path,
    policy: CachePolicy,
    builder: &VmImageBuilder,
) -> Result<BTreeMap<PathBuf, VmEnvironment>> {
    let mut environments = BTreeMap::new();
    for task in tasks {
        if environments.contains_key(task.root()) {
            continue;
        }
        let prepared = prepare_task_image(builder, task, cache, policy).await?;
        let verifier = if task.verifier().environment_mode() == VerifierEnvironmentMode::Separate {
            let verifier = prepare_verifier_image(builder, task, cache, policy).await?;
            info!(
                target: "nanocodex_eval",
                task_name = task.name(),
                oci_manifest_digest = verifier.manifest_digest(),
                oci_manifest_source = verifier.manifest_source().as_str(),
                vm_rootfs_cache_status = verifier.disk_status().as_str(),
                vm_rootfs_path = %verifier.path().display(),
                "separate verifier VM root disk ready"
            );
            Some(VerifierVmEnvironment {
                rootfs: verifier.path().to_path_buf(),
                workspace: verifier.workdir().to_owned(),
                environment: verifier.environment().clone(),
                shell: verifier.shell().to_owned(),
            })
        } else {
            None
        };
        info!(
            target: "nanocodex_eval",
            task_name = task.name(),
            oci_manifest_digest = prepared.manifest_digest(),
            oci_manifest_source = prepared.manifest_source().as_str(),
            vm_rootfs_cache_status = prepared.disk_status().as_str(),
            vm_rootfs_path = %prepared.path().display(),
            "VM root disk ready"
        );
        environments.insert(
            task.root().to_path_buf(),
            VmEnvironment {
                rootfs: prepared.path().to_path_buf(),
                workspace: prepared.workdir().to_owned(),
                environment: prepared.environment().clone(),
                shell: prepared.shell().to_owned(),
                verifier,
            },
        );
    }
    Ok(environments)
}

async fn prepare_verifier_caches(
    tasks: &[Task],
    environments: &BTreeMap<PathBuf, VmEnvironment>,
    vmm: &Path,
    runtime_image: &Path,
    gvproxy: Option<&Path>,
) -> Result<()> {
    let mut prepared = BTreeSet::new();
    for task in tasks {
        let environment = environments
            .get(task.root())
            .ok_or_else(|| VmAttemptError::MissingPreparedEnvironment(task.root().to_path_buf()))?;
        if environment.verifier.is_some() {
            continue;
        }
        let Some(cache) = prepare_verifier_cache(&environment.rootfs, task)? else {
            continue;
        };
        if !prepared.insert(cache.key.clone()) {
            continue;
        }
        cache
            .prepare_once(task, environment, vmm, runtime_image, gvproxy)
            .await?;
    }
    Ok(())
}

async fn prepare_selected_verifier_caches(
    tasks: &[Task],
    environments: Option<&BTreeMap<PathBuf, VmEnvironment>>,
    vmm: &Path,
    runtime_image: &Path,
    gvproxy: Option<&Path>,
) -> Result<()> {
    match environments {
        Some(environments) => {
            prepare_verifier_caches(tasks, environments, vmm, runtime_image, gvproxy).await
        }
        None => Ok(()),
    }
}

async fn prepare_runtime_for_vm(vm: bool, rootfs: Option<&Path>) -> Result<PathBuf> {
    if vm || rootfs.is_some_and(Path::is_file) {
        prepare_vm_guest_runtime().await
    } else {
        Ok(PathBuf::new())
    }
}

const EMBEDDED_GUEST_TOOL_RUNTIME: &str = "/usr/local/bin/nanocodex-vm-guest";
const BLOCK_GUEST_TOOL_RUNTIME: &str = "/run/nanoeval/nanocodex-vm-guest";
const GUEST_RUNTIME_BLOCK_ID: &str = "nanoeval-runtime";
const GUEST_RUNTIME_BLOCK_DEVICE: &str = "/dev/vdb";
const GUEST_RUNTIME_MOUNT: &str = "/run/nanoeval";
const DEFAULT_VM_CACHE: &str = ".cache/vm";
const DEFAULT_KRUNFW_DIRECTORY: &str = ".cache/libkrunfw/libkrunfw";
#[cfg(target_os = "linux")]
const KRUNFW_LIBRARY_FILENAME: &str = "libkrunfw.so.5";
#[cfg(target_os = "macos")]
const KRUNFW_LIBRARY_FILENAME: &str = "libkrunfw.5.dylib";
#[cfg(target_os = "linux")]
const KRUNFW_LIBRARY_PATH_ENVIRONMENT: &str = "LD_LIBRARY_PATH";
#[cfg(target_os = "macos")]
const KRUNFW_LIBRARY_PATH_ENVIRONMENT: &str = "DYLD_LIBRARY_PATH";
#[cfg(target_arch = "aarch64")]
const VM_GUEST_TARGET: &str = "aarch64-unknown-linux-musl";
#[cfg(target_arch = "x86_64")]
const VM_GUEST_TARGET: &str = "x86_64-unknown-linux-musl";
#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("Evaluator VM guests are only supported on aarch64 and x86_64 hosts");
const VERIFIER_CACHE_VERSION: u32 = 2;
const MINIMUM_VERIFIER_CACHE_DISK_BYTES: u64 = 512 * 1024 * 1024;
const MAXIMUM_VERIFIER_CACHE_DISK_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const VERIFIER_SETUP_MARKER: &str = "# Check if we're in a valid working directory";
const VERIFIER_CACHE_BLOCK_ID: &str = "nanoeval-verifier-cache";
const VERIFIER_CACHE_BLOCK_DEVICE: &str = "/dev/vdc";
const VERIFIER_CACHE_MOUNT: &str = "/run/nanoeval-verifier-cache";
const CACHED_VERIFIER_SCRIPT: &str = "/tmp/nanoeval-verifier.sh";
const VERIFIER_CACHE_PREPARE_SCRIPT: &str = "/tmp/nanoeval-prepare-verifier.sh";
const GUEST_PUBLIC_RESOLV_CONF: &str =
    "nameserver 192.168.127.1\\nnameserver 1.1.1.1\\noptions timeout:2 attempts:5\\n";
const EVAL_IMAGE_BUILD_POLICY: EvalImageBuildPolicy = EvalImageBuildPolicy {
    prefer_ipv4: true,
    run_timeout: Duration::from_mins(60),
};
const VERIFIER_NETWORK_RETRIES: usize = 4;
const VERIFIER_NETWORK_RETRY_BASE_DELAY: Duration = Duration::from_secs(2);
const VM_GUEST_BUILD_RECORD_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EvalImageBuildPolicy {
    prefer_ipv4: bool,
    run_timeout: Duration,
}

impl EvalImageBuildPolicy {
    const fn apply(self, builder: VmImageBuilder) -> VmImageBuilder {
        let builder = builder.run_timeout(self.run_timeout);
        if self.prefer_ipv4 {
            builder.prefer_ipv4()
        } else {
            builder
        }
    }
}

#[derive(Clone)]
struct VmEnvironment {
    rootfs: PathBuf,
    workspace: String,
    environment: BTreeMap<String, String>,
    shell: String,
    verifier: Option<VerifierVmEnvironment>,
}

#[derive(Clone)]
struct VerifierVmEnvironment {
    rootfs: PathBuf,
    workspace: String,
    environment: BTreeMap<String, String>,
    shell: String,
}

#[derive(Clone, Copy)]
struct VmAttemptHost<'a> {
    runtime_image: &'a Path,
    vmm: &'a Path,
    gvproxy: Option<&'a Path>,
    retain_passed_rootfs: bool,
    web_search: bool,
}

pub(crate) async fn prepare_vm_guest_runtime() -> Result<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| eyre!("nanocodex binary crate is not inside its Cargo workspace"))?;
    let started_at = Instant::now();
    let runtime = workspace
        .join("target")
        .join(VM_GUEST_TARGET)
        .join("debug/nanocodex-vm-guest");
    let build_status = if vm_guest_runtime_is_fresh(workspace, &runtime)? {
        "hit"
    } else {
        let previous_runtime = file_metadata_snapshot(&runtime)?;
        let exit = vm_guest_build_command(workspace).status().await?;
        if !exit.success() {
            return Err(eyre!("building the VM guest runtime failed with {exit}"));
        }
        let current_runtime = file_metadata_snapshot(&runtime)?;
        let build_status = if previous_runtime.is_some() && previous_runtime == current_runtime {
            "indexed"
        } else {
            "rebuilt"
        };
        write_vm_guest_build_record(workspace, &runtime)?;
        build_status
    };
    if !runtime.is_file() {
        return Err(eyre!(
            "Cargo completed without producing {}",
            runtime.display()
        ));
    }
    let runtime_disk = GuestRuntimeDisk::prepare(&runtime, workspace.join(DEFAULT_VM_CACHE))?;
    let cache_status = match runtime_disk.status() {
        GuestRuntimeDiskStatus::Hit => "hit",
        GuestRuntimeDiskStatus::Created => "created",
    };
    info!(
        target: "nanocodex_vm",
        duration_ns = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX),
        vm_guest_build_status = build_status,
        vm_guest_target = VM_GUEST_TARGET,
        vm_guest_runtime_cache_status = cache_status,
        vm_guest_runtime_digest = runtime_disk.digest(),
        vm_guest_runtime_disk = %runtime_disk.path().display(),
        "VM guest runtime ready"
    );
    Ok(runtime_disk.path().to_path_buf())
}

fn vm_guest_build_command(workspace: &Path) -> Command {
    let mut command = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()));
    command
        .current_dir(workspace)
        .arg("build")
        .arg("--quiet")
        .arg("--target")
        .arg(VM_GUEST_TARGET)
        .arg("--package")
        .arg("nanocodex-vm")
        .arg("--bin")
        .arg("nanocodex-vm-guest")
        .arg("--no-default-features")
        .arg("--features")
        .arg("guest-runtime");
    command
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct VmGuestBuildRecord {
    version: u32,
    target: String,
    runtime_bytes: u64,
    runtime_modified_unix_ns: u64,
    input_count: usize,
    input_metadata_digest: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileMetadataSnapshot {
    bytes: u64,
    modified_unix_ns: u64,
}

fn vm_guest_runtime_is_fresh(workspace: &Path, runtime: &Path) -> Result<bool> {
    let path = vm_guest_build_record_path(workspace);
    let record = match fs::read(&path) {
        Ok(contents) => match serde_json::from_slice::<VmGuestBuildRecord>(&contents) {
            Ok(record) => record,
            Err(error) => {
                warn!(
                    target: "nanocodex_eval",
                    cache_record = %path.display(),
                    %error,
                    "ignoring invalid VM guest build record"
                );
                return Ok(false);
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(vm_guest_build_record(workspace, runtime)?.as_ref() == Some(&record))
}

fn write_vm_guest_build_record(workspace: &Path, runtime: &Path) -> Result<()> {
    let record = vm_guest_build_record(workspace, runtime)?.ok_or_else(|| {
        eyre!(
            "Cargo completed without producing {} and its dependency record",
            runtime.display()
        )
    })?;
    write_json_atomic(&vm_guest_build_record_path(workspace), &record)
}

fn vm_guest_build_record(workspace: &Path, runtime: &Path) -> Result<Option<VmGuestBuildRecord>> {
    let Some(runtime_metadata) = file_metadata_snapshot(runtime)? else {
        return Ok(None);
    };
    let dependency_path = runtime.with_extension("d");
    let dependencies = match fs::read_to_string(&dependency_path) {
        Ok(contents) => parse_cargo_dep_info(&contents)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let mut inputs = dependencies;
    inputs.extend([
        workspace.join("Cargo.toml"),
        workspace.join("Cargo.lock"),
        workspace.join(".cargo/config.toml"),
        workspace.join("crates/nanocodex-oai-api/Cargo.toml"),
        workspace.join("crates/nanocodex-tools/Cargo.toml"),
        workspace.join("crates/experimental/nanocodex-vm/Cargo.toml"),
    ]);
    for script in [
        format!("scripts/{VM_GUEST_TARGET}-linker"),
        format!("scripts/{VM_GUEST_TARGET}-ar"),
    ] {
        let path = workspace.join(script);
        if path.exists() {
            inputs.push(path);
        }
    }
    inputs.sort_unstable();
    inputs.dedup();

    let mut digest = Sha256::new();
    digest.update(b"nanocodex-vm-guest-build-inputs-v1\0");
    for input in &inputs {
        let Some(metadata) = file_metadata_snapshot(input)? else {
            return Ok(None);
        };
        digest.update(input.as_os_str().as_encoded_bytes());
        digest.update([0]);
        digest.update(metadata.bytes.to_le_bytes());
        digest.update(metadata.modified_unix_ns.to_le_bytes());
    }
    Ok(Some(VmGuestBuildRecord {
        version: VM_GUEST_BUILD_RECORD_VERSION,
        target: VM_GUEST_TARGET.to_owned(),
        runtime_bytes: runtime_metadata.bytes,
        runtime_modified_unix_ns: runtime_metadata.modified_unix_ns,
        input_count: inputs.len(),
        input_metadata_digest: hex::encode(digest.finalize()),
    }))
}

fn vm_guest_build_record_path(workspace: &Path) -> PathBuf {
    workspace
        .join(DEFAULT_VM_CACHE)
        .join("runtime-build-records")
        .join(format!("{VM_GUEST_TARGET}.json"))
}

fn file_metadata_snapshot(path: &Path) -> io::Result<Option<FileMetadataSnapshot>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?;
    Ok(Some(FileMetadataSnapshot {
        bytes: metadata.len(),
        modified_unix_ns: u64::try_from(modified.as_nanos()).map_err(io::Error::other)?,
    }))
}

fn parse_cargo_dep_info(contents: &str) -> io::Result<Vec<PathBuf>> {
    let (_, dependencies) = contents
        .split_once(": ")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid Cargo dep-info"))?;
    let mut paths = Vec::new();
    let mut current = String::new();
    let mut escaped = false;
    for character in dependencies.chars() {
        if escaped {
            if character != '\n' && character != '\r' {
                current.push(character);
            }
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() {
            if !current.is_empty() {
                paths.push(PathBuf::from(std::mem::take(&mut current)));
            }
        } else {
            current.push(character);
        }
    }
    if escaped {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Cargo dep-info ends with an escape",
        ));
    }
    if !current.is_empty() {
        paths.push(PathBuf::from(current));
    }
    if paths.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Cargo dep-info contains no dependencies",
        ));
    }
    Ok(paths)
}

#[derive(Debug, thiserror::Error)]
enum VmAttemptError {
    #[error("no VM environment was prepared for task root {0}")]
    MissingPreparedEnvironment(PathBuf),

    #[error("the agent VM session was already finished")]
    AgentSessionAlreadyFinished,

    #[error("rootfs template is not a directory: {0}")]
    InvalidRootfs(PathBuf),

    #[error("rootfs template does not contain the guest tool runtime: {0}")]
    MissingGuestRuntime(PathBuf),

    #[error("the task requires public networking but gvproxy was not prepared")]
    NetworkBackendNotPrepared,

    #[error("rootfs entry collides with attempt data: {0}")]
    Collision(PathBuf),

    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Session(#[from] VmToolSessionError),

    #[error(transparent)]
    Tools(#[from] ToolsBuildError),

    #[error(transparent)]
    ParseReward(#[from] ParseFloatError),

    #[error(transparent)]
    Ext4(#[from] arcbox_ext4::error::FormatError),

    #[error(transparent)]
    Network(#[from] GvproxyError),
}

struct VmAttempt {
    tools: Tools,
    verifier: VmVerifier,
}

struct VmVerifier {
    agent_session: Option<VmToolSession>,
    launch: VmLaunch,
    separate_launch: Option<VmLaunch>,
    cache: Option<VerifierCache>,
    attempt_cache: Option<AttemptVerifierCache>,
    retain_passed_rootfs: bool,
    _network: Option<Gvproxy>,
}

#[derive(Clone)]
struct VmLaunch {
    root: PathBuf,
    workspace: String,
    shell: String,
    runtime_image: PathBuf,
    vmm: PathBuf,
    cpus: u32,
    memory_mib: u64,
    ext4: bool,
    resolver_configuration: String,
    environment: BTreeMap<String, String>,
    network_socket: Option<PathBuf>,
}

struct VerifierCache {
    root: PathBuf,
    key: String,
    status: &'static str,
    cacheable_start: usize,
    cacheable_end: usize,
    skip_setup: bool,
    disk_bytes: u64,
}

struct AttemptVerifierCache {
    disk: PathBuf,
    skip_setup: bool,
}

fn vm_attempt(
    environment: &VmEnvironment,
    host: VmAttemptHost<'_>,
    attempt: EvalAttempt<'_>,
) -> Result<VmAttempt, VmAttemptError> {
    let span = info_span!(
        target: "nanocodex_eval",
        "vm.attempt.setup",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        eval.task.name = attempt.task().name(),
        vm.rootfs.template = %environment.rootfs.display(),
        vm.rootfs.destination = %attempt.directory().display(),
        vm.cpu.count = attempt.task().resources().cpus,
        vm.memory_mib = attempt.task().resources().memory_mb,
        status = tracing::field::Empty,
        error.message = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    );
    let started_at = Instant::now();
    let result = span.in_scope(|| vm_attempt_inner(environment, host, attempt));
    record_operation(&span, started_at, &result);
    result
}

fn vm_attempt_inner(
    environment: &VmEnvironment,
    host: VmAttemptHost<'_>,
    attempt: EvalAttempt<'_>,
) -> Result<VmAttempt, VmAttemptError> {
    let template = &environment.rootfs;
    let verifier_cache = if environment.verifier.is_some() {
        None
    } else {
        prepare_verifier_cache(template, attempt.task())?
    };
    let root = materialize_attempt_root(template, host.runtime_image, attempt.directory())?;
    let network = spawn_attempt_network(
        attempt.task().network(),
        host.gvproxy,
        &attempt.directory().join("vm").join("gvproxy.log"),
    )?;
    let launch = VmLaunch {
        root,
        workspace: environment.workspace.clone(),
        shell: environment.shell.clone(),
        runtime_image: host.runtime_image.to_path_buf(),
        vmm: host.vmm.to_path_buf(),
        cpus: attempt.task().resources().cpus.clamp(1, u32::from(u8::MAX)),
        memory_mib: attempt
            .task()
            .resources()
            .memory_mb
            .clamp(1, u64::from(u32::MAX)),
        ext4: template.is_file(),
        resolver_configuration: network
            .as_ref()
            .map_or_else(String::new, |_| GUEST_PUBLIC_RESOLV_CONF.to_owned()),
        environment: environment.environment.clone(),
        network_socket: network
            .as_ref()
            .map(|network| network.socket().to_path_buf()),
    };
    let separate_launch = prepare_separate_verifier_launch(environment, &launch, host, attempt)?;
    let verifier_directory = attempt.directory().join("verifier");
    fs::create_dir_all(&verifier_directory)?;
    let attempt_cache = verifier_cache
        .as_ref()
        .map(|cache| cache.materialize(&verifier_directory))
        .transpose()?;
    let session = launch.spawn(attempt_cache.as_ref())?;
    let vm = session.tools();
    let tools = Tools::builder()
        .without_defaults()
        .web_search(host.web_search)
        .image_generation(true)
        .working_directory(environment.workspace.clone())
        .default_shell(if template.is_file() {
            &environment.shell
        } else {
            "sh"
        })
        .tool(vm.exec_command_tool())
        .tool(vm.write_stdin_tool())
        .tool(vm.apply_patch_tool())
        .tool(vm.view_image_tool())
        .tool(UpdatePlanTool::new())
        .build()
        .map_err(VmAttemptError::from)?;
    Ok(VmAttempt {
        tools,
        verifier: VmVerifier {
            agent_session: Some(session),
            launch,
            separate_launch,
            cache: verifier_cache,
            attempt_cache,
            retain_passed_rootfs: host.retain_passed_rootfs,
            _network: network,
        },
    })
}

fn materialize_attempt_root(
    template: &Path,
    runtime_image: &Path,
    attempt_directory: &Path,
) -> Result<PathBuf, VmAttemptError> {
    if template.is_file() {
        if !runtime_image.is_file() {
            return Err(VmAttemptError::MissingGuestRuntime(
                runtime_image.to_path_buf(),
            ));
        }
        let root = attempt_directory.join("rootfs.ext4");
        reflink_or_sparse_copy(template, &root)?;
        return Ok(root);
    }

    let guest_runtime = template.join(EMBEDDED_GUEST_TOOL_RUNTIME.trim_start_matches('/'));
    if !guest_runtime.is_file() {
        return Err(VmAttemptError::MissingGuestRuntime(guest_runtime));
    }
    let span = info_span!(
        target: "nanocodex_eval",
        "vm.rootfs.materialize",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        source = %template.display(),
        destination = %attempt_directory.display(),
        status = tracing::field::Empty,
        error.message = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    );
    let started_at = Instant::now();
    let result = span.in_scope(|| materialize_rootfs(template, attempt_directory));
    record_operation(&span, started_at, &result);
    result?;
    Ok(attempt_directory.to_path_buf())
}

fn prepare_separate_verifier_launch(
    environment: &VmEnvironment,
    agent: &VmLaunch,
    host: VmAttemptHost<'_>,
    attempt: EvalAttempt<'_>,
) -> Result<Option<VmLaunch>, VmAttemptError> {
    environment
        .verifier
        .as_ref()
        .map(|verifier| {
            let root = attempt.directory().join("verifier-rootfs.ext4");
            reflink_or_sparse_copy(&verifier.rootfs, &root)?;
            Ok(VmLaunch {
                root,
                workspace: verifier.workspace.clone(),
                shell: verifier.shell.clone(),
                runtime_image: host.runtime_image.to_path_buf(),
                vmm: host.vmm.to_path_buf(),
                cpus: attempt.task().resources().cpus.clamp(1, u32::from(u8::MAX)),
                memory_mib: attempt
                    .task()
                    .resources()
                    .memory_mb
                    .clamp(1, u64::from(u32::MAX)),
                ext4: true,
                resolver_configuration: agent.resolver_configuration.clone(),
                environment: verifier.environment.clone(),
                network_socket: agent.network_socket.clone(),
            })
        })
        .transpose()
}

fn prepare_verifier_cache(
    template: &Path,
    task: &Task,
) -> Result<Option<VerifierCache>, VmAttemptError> {
    template
        .is_file()
        .then(|| VerifierCache::prepare(template, task, Path::new(DEFAULT_VM_CACHE)))
        .transpose()
        .map(Option::flatten)
}

fn spawn_attempt_network(
    policy: NetworkPolicy,
    gvproxy: Option<&Path>,
    log: &Path,
) -> Result<Option<Gvproxy>, VmAttemptError> {
    match policy {
        NetworkPolicy::Public => {
            let binary = gvproxy.ok_or(VmAttemptError::NetworkBackendNotPrepared)?;
            Gvproxy::spawn(binary, log).map(Some).map_err(Into::into)
        }
        NetworkPolicy::Disabled => Ok(None),
    }
}

impl VmLaunch {
    fn spawn(
        &self,
        verifier_cache: Option<&AttemptVerifierCache>,
    ) -> Result<VmToolSession, VmAttemptError> {
        let mut command = Command::new(&self.vmm);
        let firmware = Path::new(DEFAULT_KRUNFW_DIRECTORY);
        if firmware.join(KRUNFW_LIBRARY_FILENAME).is_file() {
            command.env(KRUNFW_LIBRARY_PATH_ENVIRONMENT, firmware.canonicalize()?);
        }
        command.args(["eval", "vm", "run-config", "--config"]);

        let network = if let Some(socket) = &self.network_socket {
            Network::gvproxy(socket)
        } else {
            Network::Disabled
        };
        let mut vm = if self.ext4 {
            VmConfig::ext4(&self.root)
        } else {
            VmConfig::new(&self.root)
        }
        .cpus(u8::try_from(self.cpus).unwrap_or(u8::MAX))
        .memory_mib(u32::try_from(self.memory_mib).unwrap_or(u32::MAX))
        .network(network);
        if self.ext4 {
            vm = vm.block_device(BlockDevice::read_only(
                GUEST_RUNTIME_BLOCK_ID,
                &self.runtime_image,
            ));
            if let Some(cache) = verifier_cache {
                vm = vm.block_device(BlockDevice::read_write(
                    VERIFIER_CACHE_BLOCK_ID,
                    &cache.disk,
                ));
            }
        }

        let mut guest = if self.ext4 {
            GuestCommand::new("/bin/sh")
                .arg("-c")
                .arg(vm_guest_bootstrap_script(
                    &self.workspace,
                    &self.resolver_configuration,
                ))
        } else {
            GuestCommand::new(EMBEDDED_GUEST_TOOL_RUNTIME).arg(&self.workspace)
        };
        for (name, value) in &self.environment {
            guest = guest.env(name, value);
        }
        VmToolSession::spawn_vm(command, vm, guest).map_err(Into::into)
    }
}

fn vm_guest_bootstrap_script(workspace: &str, resolver_configuration: &str) -> String {
    let workspace = shell_word_without_double_quotes(workspace);
    let resolver_configuration = shell_word_without_double_quotes(resolver_configuration);
    format!(
        "set -eu; rm -f /etc/resolv.conf; printf %b {resolver_configuration} > /etc/resolv.conf; \
         mkdir -p -- {workspace} /logs/verifier {GUEST_RUNTIME_MOUNT}; \
         mount -t ext4 -o ro {GUEST_RUNTIME_BLOCK_DEVICE} {GUEST_RUNTIME_MOUNT}; \
         exec {BLOCK_GUEST_TOOL_RUNTIME} {workspace}"
    )
}

fn shell_word_without_double_quotes(value: &str) -> String {
    let mut quoted = String::with_capacity(value.len().saturating_add(2));
    quoted.push('\'');
    for character in value.chars() {
        match character {
            '\'' => quoted.push_str("'\\''"),
            // libkrun cannot carry a literal double quote in an argv entry.
            // Synthesize it only after the wrapper shell starts.
            '"' => quoted.push_str("'$(printf '\\042')'"),
            character => quoted.push(character),
        }
    }
    quoted.push('\'');
    quoted
}

impl VerifierCache {
    fn prepare(template: &Path, task: &Task, cache: &Path) -> Result<Option<Self>, VmAttemptError> {
        let script = fs::read(task.verifier().script())?;
        let Some(setup) = recognized_verifier_setup(&script) else {
            info!(
                target: "nanocodex_eval",
                task_name = task.name(),
                verifier_cache_status = "unsupported",
                "canonical verifier will use the cold dependency path"
            );
            return Ok(None);
        };
        let template_identity = template
            .file_name()
            .ok_or_else(|| io::Error::other("VM root disk template has no file name"))?;
        let disk_bytes = task
            .resources()
            .storage_mb
            .saturating_mul(1024 * 1024)
            .clamp(
                MINIMUM_VERIFIER_CACHE_DISK_BYTES,
                MAXIMUM_VERIFIER_CACHE_DISK_BYTES,
            );
        let key = verifier_cache_key(
            template_identity,
            &script[setup.cacheable_start..setup.cacheable_end],
            disk_bytes,
        );
        let root = cache.join("verifiers").join(&key);
        let disk = root.join("cache.ext4");
        let status = if disk.is_file() && verifier_cache_populated(&disk)? {
            "hit"
        } else {
            "miss"
        };
        info!(
            target: "nanocodex_eval",
            task_name = task.name(),
            verifier_cache_key = key,
            verifier_cache_status = status,
            verifier_cache_path = %root.display(),
            "post-agent verifier dependency cache ready"
        );
        Ok(Some(Self {
            root,
            key,
            status,
            cacheable_start: setup.cacheable_start,
            cacheable_end: setup.cacheable_end,
            skip_setup: setup.skip_setup,
            disk_bytes,
        }))
    }

    fn materialize(
        &self,
        verifier_directory: &Path,
    ) -> Result<AttemptVerifierCache, VmAttemptError> {
        let disk = verifier_directory.join("cache.ext4");
        let hit = self.is_ready()?;
        if hit {
            reflink_or_sparse_copy(&self.root.join("cache.ext4"), &disk)?;
        } else {
            format_verifier_cache_disk(&disk, self.disk_bytes)?;
        }
        Ok(AttemptVerifierCache {
            disk,
            skip_setup: hit && self.skip_setup,
        })
    }

    fn is_ready(&self) -> io::Result<bool> {
        let disk = self.root.join("cache.ext4");
        Ok(disk.is_file() && verifier_cache_populated(&disk)?)
    }

    async fn prepare_once(
        &self,
        task: &Task,
        environment: &VmEnvironment,
        vmm: &Path,
        runtime_image: &Path,
        gvproxy: Option<&Path>,
    ) -> Result<(), VmAttemptError> {
        if self.is_ready()? {
            return Ok(());
        }
        fs::create_dir_all(&self.root)?;
        let lock_path = self.root.join(".prepare.lock");
        let lock = tokio::task::spawn_blocking(move || {
            let file = fs::OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(lock_path)?;
            file.lock_exclusive()?;
            Ok::<_, io::Error>(file)
        })
        .await
        .map_err(io::Error::other)??;
        if self.is_ready()? {
            info!(
                target: "nanocodex_eval",
                verifier_cache_key = self.key,
                "verifier cache preparation reused another process's result"
            );
            drop(lock);
            return Ok(());
        }
        let target = self.root.join("cache.ext4");
        if target.is_file() {
            fs::remove_file(&target)?;
        }
        self.populate(task, environment, vmm, runtime_image, gvproxy)
            .await?;
        drop(lock);
        Ok(())
    }

    async fn populate(
        &self,
        task: &Task,
        environment: &VmEnvironment,
        vmm: &Path,
        runtime_image: &Path,
        gvproxy: Option<&Path>,
    ) -> Result<(), VmAttemptError> {
        let temporary = tempfile::tempdir_in(&self.root)?;
        let root = materialize_attempt_root(&environment.rootfs, runtime_image, temporary.path())?;
        let network = spawn_attempt_network(
            task.network(),
            gvproxy,
            &temporary.path().join("gvproxy.log"),
        )?;
        let launch = VmLaunch {
            root,
            workspace: environment.workspace.clone(),
            shell: environment.shell.clone(),
            runtime_image: runtime_image.to_path_buf(),
            vmm: vmm.to_path_buf(),
            cpus: task.resources().cpus.clamp(1, u32::from(u8::MAX)),
            memory_mib: task.resources().memory_mb.clamp(1, u64::from(u32::MAX)),
            ext4: true,
            resolver_configuration: network
                .as_ref()
                .map_or_else(String::new, |_| GUEST_PUBLIC_RESOLV_CONF.to_owned()),
            environment: environment.environment.clone(),
            network_socket: network
                .as_ref()
                .map(|network| network.socket().to_path_buf()),
        };
        let verifier_directory = temporary.path().join("verifier");
        fs::create_dir_all(&verifier_directory)?;
        let attempt_cache = AttemptVerifierCache {
            disk: verifier_directory.join("cache.ext4"),
            skip_setup: false,
        };
        format_verifier_cache_disk(&attempt_cache.disk, self.disk_bytes)?;
        let session = launch.spawn(Some(&attempt_cache))?;
        mount_verifier_cache(&session).await?;
        let script = fs::read(task.verifier().script())?;
        session
            .write_file(
                VERIFIER_CACHE_PREPARE_SCRIPT,
                script[self.cacheable_start..self.cacheable_end].to_vec(),
                0o700,
            )
            .await?;
        let mut last_output = None;
        for retry in 0..=VERIFIER_NETWORK_RETRIES {
            restore_verifier_resolver(&session, &launch).await?;
            let output = session
                .command(
                    VmCommand::new(&launch.shell)
                        .arg(VERIFIER_CACHE_PREPARE_SCRIPT)
                        .current_directory(&launch.workspace)
                        .environment(base_guest_environment(task, &launch.workspace))
                        .timeout(task.verifier().timeout()),
                )
                .await?;
            let retryable = verifier_bootstrap_network_failed(&output);
            let succeeded = output.exit_code == 0;
            last_output = Some(output);
            if succeeded || retry == VERIFIER_NETWORK_RETRIES || !retryable {
                break;
            }
            let delay = verifier_network_retry_delay(retry);
            warn!(
                target: "nanocodex_eval",
                verifier_cache_key = self.key,
                retry = retry + 1,
                max_retries = VERIFIER_NETWORK_RETRIES,
                retry_delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                "verifier cache preparation hit a transient network failure; retrying"
            );
            tokio::time::sleep(delay).await;
        }
        let output =
            last_output.ok_or_else(|| io::Error::other("verifier cache setup did not execute"))?;
        let combined = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
        fs::write(self.root.join("prepare.log"), &combined)?;
        session.shutdown().await?;
        if output.exit_code != 0 || !verifier_cache_populated(&attempt_cache.disk)? {
            return Err(io::Error::other(format!(
                "verifier cache setup exited {}: {}",
                output.exit_code,
                String::from_utf8_lossy(&combined)
            ))
            .into());
        }
        if !self.mark_ready(&attempt_cache)? {
            return Err(io::Error::other("verifier cache setup produced no reusable cache").into());
        }
        info!(
            target: "nanocodex_eval",
            verifier_cache_key = self.key,
            "verifier cache prepared before agent execution"
        );
        Ok(())
    }

    fn mark_ready(&self, attempt: &AttemptVerifierCache) -> io::Result<bool> {
        if attempt.skip_setup || !verifier_cache_populated(&attempt.disk)? {
            return Ok(false);
        }
        fs::create_dir_all(&self.root)?;
        let target = self.root.join("cache.ext4");
        let mut identity = Sha256::new();
        identity.update(attempt.disk.as_os_str().as_encoded_bytes());
        let temporary = self
            .root
            .join(format!("cache.{}.tmp", hex::encode(identity.finalize())));
        reflink_or_sparse_copy(&attempt.disk, &temporary)?;
        match fs::hard_link(&temporary, &target) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                fs::remove_file(&temporary)?;
                return Err(error);
            }
        }
        fs::remove_file(temporary)?;
        Ok(true)
    }
}

fn verifier_cache_key(
    template_identity: &OsStr,
    cacheable_script: &[u8],
    disk_bytes: u64,
) -> String {
    let mut digest = Sha256::new();
    digest.update(VERIFIER_CACHE_VERSION.to_le_bytes());
    digest.update(VM_GUEST_TARGET.as_bytes());
    digest.update(template_identity.as_encoded_bytes());
    digest.update(cacheable_script);
    digest.update(disk_bytes.to_le_bytes());
    hex::encode(digest.finalize())
}

fn format_verifier_cache_disk(path: &Path, disk_bytes: u64) -> Result<(), VmAttemptError> {
    let mut formatter = Formatter::new(path, 4_096, disk_bytes)?;
    for directory in ["apt-archives", "apt-lists", "uv-cache", "uv-home"] {
        formatter.create(
            &format!("/{directory}"),
            make_mode(file_mode::S_IFDIR, 0o755),
            None,
            None,
            None,
            Some(0),
            Some(0),
            None,
        )?;
    }
    formatter.close()?;
    Ok(())
}

fn verifier_cache_populated(disk: &Path) -> io::Result<bool> {
    let mut reader = Reader::new(disk).map_err(io::Error::other)?;
    Ok(reader.exists("/uv-home/bin/env") && reader.exists("/uv-home/bin/uv"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecognizedVerifierSetup {
    cacheable_start: usize,
    cacheable_end: usize,
    skip_setup: bool,
}

fn recognized_verifier_setup(script: &[u8]) -> Option<RecognizedVerifierSetup> {
    let script = std::str::from_utf8(script).ok()?;
    let marker = script.find(VERIFIER_SETUP_MARKER)?;
    let setup = &script[..marker];
    let commands = setup
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let canonical = [
        "apt-get update",
        "apt-get install -y curl",
        "curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh",
        "source $HOME/.local/bin/env",
    ];
    let has_pinned_uv_bootstrap = commands
        .windows(2)
        .any(|commands| commands == &canonical[2..]);
    if !has_pinned_uv_bootstrap {
        return None;
    }
    let cacheable_start = script
        .strip_prefix("#!")
        .and_then(|script| script.find('\n'))
        .map_or(0, |offset| offset + 3);
    Some(RecognizedVerifierSetup {
        cacheable_start,
        cacheable_end: marker,
        skip_setup: commands == canonical,
    })
}

fn cached_verifier_script(script: &[u8], setup: RecognizedVerifierSetup) -> Vec<u8> {
    let mut cached = Vec::with_capacity(script.len());
    cached.extend_from_slice(&script[..setup.cacheable_start]);
    cached.extend_from_slice(b"\nsource /root/.local/bin/env\n");
    cached.extend_from_slice(&script[setup.cacheable_end..]);
    cached
}

fn verifier_bootstrap_network_failed(output: &VmCommandOutput) -> bool {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let contains = |needle: &str| stdout.contains(needle) || stderr.contains(needle);
    let dependency_runner_missing = contains("uvx: command not found")
        || contains("/root/.local/bin/env: No such file or directory");
    let dns_failed = contains("Temporary failure resolving") || contains("Could not resolve host");
    let network_failed = dns_failed
        || contains("failed to download https://github.com/astral-sh/uv/")
        || contains("The requested URL returned error: 502")
        || contains("The requested URL returned error: 503")
        || contains("The requested URL returned error: 504");
    let apt_bootstrap_failed = dns_failed
        && (contains("deb.debian.org")
            || contains("archive.ubuntu.com")
            || contains("security.ubuntu.com"));
    apt_bootstrap_failed || dependency_runner_missing && network_failed
}

impl AttemptVerifier for VmVerifier {
    fn verify<'a>(
        &'a mut self,
        task: &'a Task,
        attempt: EvalAttempt<'a>,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<AttemptVerification, Box<dyn Error + Send + Sync + 'static>>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.verify_inner(task, attempt)
                .await
                .map_err(|error| Box::new(error) as _)
        })
    }
}

impl VmVerifier {
    async fn collect_artifacts(
        session: &VmToolSession,
        task: &Task,
        launch: &VmLaunch,
    ) -> Result<Option<Vec<u8>>, VmAttemptError> {
        for collect in task.verifier().collect() {
            let output = session
                .command(
                    VmCommand::new("/bin/sh")
                        .arg("-c")
                        .arg(collect.command())
                        .current_directory(&launch.workspace)
                        .environment(base_guest_environment(task, &launch.workspace))
                        .timeout(task.verifier().timeout()),
                )
                .await?;
            if output.exit_code != 0 {
                return Err(io::Error::other(format!(
                    "verifier artifact collection exited {}: {}",
                    output.exit_code,
                    String::from_utf8_lossy(&output.stderr)
                ))
                .into());
            }
        }
        if task.artifacts().is_empty() {
            return Ok(None);
        }

        let mut command = VmCommand::new("/bin/tar")
            .arg("-C")
            .arg("/")
            .arg("-cf")
            .arg("/tmp/nanoeval-artifacts.tar")
            .arg("--");
        for artifact in task.artifacts() {
            let relative = artifact.strip_prefix("/").map_err(|_| {
                io::Error::other(format!(
                    "artifact path must be absolute: {}",
                    artifact.display()
                ))
            })?;
            if relative.as_os_str().is_empty()
                || relative
                    .components()
                    .any(|component| !matches!(component, std::path::Component::Normal(_)))
            {
                return Err(io::Error::other(format!(
                    "artifact path is not a safe guest path: {}",
                    artifact.display()
                ))
                .into());
            }
            command = command.arg(
                relative
                    .to_str()
                    .ok_or_else(|| {
                        io::Error::other(format!(
                            "artifact path is not UTF-8: {}",
                            artifact.display()
                        ))
                    })?
                    .to_owned(),
            );
        }
        let output = session
            .command(command.timeout(task.verifier().timeout()))
            .await?;
        if output.exit_code != 0 {
            return Err(io::Error::other(format!(
                "artifact archive exited {}: {}",
                output.exit_code,
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        session
            .read_file("/tmp/nanoeval-artifacts.tar")
            .await
            .map(Some)
            .map_err(Into::into)
    }

    async fn stage_artifacts(
        session: &VmToolSession,
        artifacts: Option<Vec<u8>>,
    ) -> Result<(), VmAttemptError> {
        let Some(artifacts) = artifacts else {
            return Ok(());
        };
        session
            .write_file("/tmp/nanoeval-artifacts.tar", artifacts, 0o600)
            .await?;
        let output = session
            .command(
                VmCommand::new("/bin/tar")
                    .arg("-C")
                    .arg("/")
                    .arg("-xf")
                    .arg("/tmp/nanoeval-artifacts.tar")
                    .timeout(Duration::from_mins(10)),
            )
            .await?;
        if output.exit_code != 0 {
            return Err(io::Error::other(format!(
                "artifact extraction exited {}: {}",
                output.exit_code,
                String::from_utf8_lossy(&output.stderr)
            ))
            .into());
        }
        Ok(())
    }

    async fn verify_inner(
        &mut self,
        task: &Task,
        attempt: EvalAttempt<'_>,
    ) -> Result<AttemptVerification, VmAttemptError> {
        let verifier_directory = attempt.directory().join("verifier");
        fs::create_dir_all(&verifier_directory)?;
        let (verifier_launch, verifier_session) = self.start_verifier_session(task).await?;
        let command = self.verifier_command(task, &verifier_launch, self.attempt_cache.as_ref())?;
        let (output, verifier_timed_out) = self
            .execute_verifier_with_network_retries(&verifier_session, &verifier_launch, command)
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let combined = match (stdout.is_empty(), stderr.is_empty()) {
            (_, true) => stdout.clone(),
            (true, false) => stderr.clone(),
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        fs::write(verifier_directory.join("test-stdout.txt"), combined)?;
        let reward_bytes = if verifier_timed_out {
            b"0\n".to_vec()
        } else {
            verifier_session
                .read_file("/logs/verifier/reward.txt")
                .await?
        };
        fs::write(verifier_directory.join("reward.txt"), &reward_bytes)?;
        if let Ok(ctrf) = verifier_session.read_file("/logs/verifier/ctrf.json").await {
            fs::write(verifier_directory.join("ctrf.json"), ctrf)?;
        }
        let answer_path = format!("{}/answer.txt", verifier_launch.workspace);
        if let Ok(answer) = verifier_session.read_file(answer_path).await {
            fs::write(attempt.workspace().join("answer.txt"), answer)?;
        }
        verifier_session.shutdown().await?;
        self.finish_verifier_cache()?;
        let reward = String::from_utf8_lossy(&reward_bytes)
            .trim()
            .parse::<f64>()?;
        if reward > 0.0 && !self.retain_passed_rootfs {
            self.remove_passed_root_disks();
        }
        Ok(AttemptVerification {
            result: VerifierResult {
                exit_code: output.exit_code,
                rewards: BTreeMap::from([("reward".to_owned(), reward)]),
            },
            stdout,
            stderr,
        })
    }

    async fn start_verifier_session(
        &mut self,
        task: &Task,
    ) -> Result<(VmLaunch, VmToolSession), VmAttemptError> {
        let agent_session = self
            .agent_session
            .take()
            .ok_or(VmAttemptError::AgentSessionAlreadyFinished)?;
        let launch = self
            .separate_launch
            .clone()
            .unwrap_or_else(|| self.launch.clone());
        let session = if self.separate_launch.is_some() {
            let artifacts = Self::collect_artifacts(&agent_session, task, &self.launch).await?;
            agent_session.shutdown().await?;
            let session = launch.spawn(None)?;
            Self::stage_artifacts(&session, artifacts).await?;
            session
        } else {
            let tests = task
                .verifier()
                .script()
                .parent()
                .ok_or_else(|| io::Error::other("verifier script has no parent directory"))?;
            Self::copy_directory(&agent_session, tests, tests, Path::new("/tests")).await?;
            agent_session
        };
        session
            .write_file("/logs/verifier/.nanoeval", Vec::new(), 0o600)
            .await?;
        if self.attempt_cache.is_some() {
            self.mount_verifier_cache(&session).await?;
        }
        self.stage_cached_verifier(&session, task).await?;
        Ok((launch, session))
    }

    fn finish_verifier_cache(&mut self) -> Result<(), VmAttemptError> {
        if let (Some(cache), Some(attempt_cache)) = (&self.cache, &self.attempt_cache)
            && !attempt_cache.skip_setup
        {
            if cache.mark_ready(attempt_cache)? {
                info!(
                    target: "nanocodex_eval",
                    verifier_cache_key = cache.key,
                    verifier_cache_previous_status = cache.status,
                    "post-agent verifier dependency cache committed"
                );
            } else {
                warn!(
                    target: "nanocodex_eval",
                    verifier_cache_key = cache.key,
                    "verifier dependency cache remained incomplete"
                );
            }
        }
        if let Some(attempt_cache) = self.attempt_cache.take() {
            fs::remove_file(attempt_cache.disk)?;
        }
        Ok(())
    }

    fn remove_attempt_cache(&mut self) {
        let Some(attempt_cache) = self.attempt_cache.take() else {
            return;
        };
        match fs::remove_file(&attempt_cache.disk) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warn!(
                target: "nanocodex_eval",
                verifier_cache_path = %attempt_cache.disk.display(),
                %error,
                "failed to remove disposable attempt verifier cache"
            ),
        }
    }

    fn remove_passed_root_disks(&self) {
        for launch in std::iter::once(&self.launch).chain(self.separate_launch.as_ref()) {
            if !launch.ext4 {
                continue;
            }
            match remove_passed_rootfs(&launch.root) {
                Ok(true) => info!(
                    target: "nanocodex_eval",
                    vm_rootfs_path = %launch.root.display(),
                    "removed passed attempt VM root disk"
                ),
                Ok(false) => {}
                Err(error) => warn!(
                    target: "nanocodex_eval",
                    vm_rootfs_path = %launch.root.display(),
                    %error,
                    "failed to remove passed attempt VM root disk"
                ),
            }
        }
    }

    async fn execute_verifier_command(
        session: &VmToolSession,
        command: VmCommand,
    ) -> Result<(VmCommandOutput, bool), VmAttemptError> {
        match session.command(command).await {
            Ok(output) => Ok((output, false)),
            Err(VmToolSessionError::GuestTimeout { timeout, output }) => {
                Ok((verifier_timeout_output(timeout, output), true))
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn execute_verifier_with_network_retries(
        &self,
        session: &VmToolSession,
        launch: &VmLaunch,
        command: VmCommand,
    ) -> Result<(VmCommandOutput, bool), VmAttemptError> {
        for retry in 0..=VERIFIER_NETWORK_RETRIES {
            restore_verifier_resolver(session, launch).await?;
            let result = Self::execute_verifier_command(session, command.clone()).await?;
            if result.1
                || retry == VERIFIER_NETWORK_RETRIES
                || !verifier_bootstrap_network_failed(&result.0)
            {
                return Ok(result);
            }
            let delay = verifier_network_retry_delay(retry);
            warn!(
                target: "nanocodex_eval",
                retry = retry + 1,
                max_retries = VERIFIER_NETWORK_RETRIES,
                retry_delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                "canonical verifier dependency bootstrap hit a transient network failure; retrying"
            );
            tokio::time::sleep(delay).await;
        }
        unreachable!("the verifier retry loop always returns")
    }

    async fn stage_cached_verifier(
        &self,
        session: &VmToolSession,
        task: &Task,
    ) -> Result<(), VmAttemptError> {
        if !self
            .attempt_cache
            .as_ref()
            .is_some_and(|cache| cache.skip_setup)
        {
            return Ok(());
        }
        let cache = self
            .cache
            .as_ref()
            .ok_or_else(|| io::Error::other("verifier cache metadata is missing"))?;
        let script = fs::read(task.verifier().script())?;
        let cached = cached_verifier_script(
            &script,
            RecognizedVerifierSetup {
                cacheable_start: cache.cacheable_start,
                cacheable_end: cache.cacheable_end,
                skip_setup: cache.skip_setup,
            },
        );
        session
            .write_file(CACHED_VERIFIER_SCRIPT, cached, 0o700)
            .await?;
        Ok(())
    }

    async fn mount_verifier_cache(&self, session: &VmToolSession) -> Result<(), VmAttemptError> {
        mount_verifier_cache(session).await
    }

    fn verifier_command(
        &self,
        task: &Task,
        launch: &VmLaunch,
        attempt_cache: Option<&AttemptVerifierCache>,
    ) -> Result<VmCommand, VmAttemptError> {
        let skip_setup = attempt_cache.is_some_and(|cache| cache.skip_setup);
        let mut command = if skip_setup {
            let cache = self
                .cache
                .as_ref()
                .ok_or_else(|| io::Error::other("verifier cache metadata is missing"))?;
            info!(
                target: "nanocodex_eval",
                verifier_cache_key = cache.key,
                verifier_setup_bytes_skipped = cache.cacheable_end - cache.cacheable_start,
                verifier_system_setup_bytes = cache.cacheable_start,
                "running canonical verifier with only persisted setup omitted"
            );
            VmCommand::new(verifier_shell(&launch.shell, skip_setup)).arg(CACHED_VERIFIER_SCRIPT)
        } else {
            VmCommand::new(verifier_shell(&launch.shell, skip_setup)).arg("/tests/test.sh")
        };
        command = command
            .current_directory(&launch.workspace)
            .environment(base_guest_environment(task, &launch.workspace))
            .timeout(task.verifier().timeout());
        Ok(command)
    }

    fn copy_directory<'a>(
        session: &'a VmToolSession,
        root: &'a Path,
        directory: &'a Path,
        destination: &'a Path,
    ) -> Pin<Box<dyn Future<Output = Result<(), VmAttemptError>> + Send + 'a>> {
        Box::pin(async move {
            for entry in fs::read_dir(directory)? {
                let entry = entry?;
                let path = entry.path();
                let relative = path.strip_prefix(root).map_err(io::Error::other)?;
                let guest = destination.join(relative).to_string_lossy().into_owned();
                let file_type = entry.file_type()?;
                if file_type.is_dir() {
                    Self::copy_directory(session, root, &path, destination).await?;
                } else if file_type.is_file() {
                    let mode =
                        std::os::unix::fs::PermissionsExt::mode(&entry.metadata()?.permissions());
                    session.write_file(guest, fs::read(path)?, mode).await?;
                } else {
                    return Err(VmAttemptError::Collision(path));
                }
            }
            Ok(())
        })
    }
}

impl Drop for VmVerifier {
    fn drop(&mut self) {
        self.remove_attempt_cache();
    }
}

const fn verifier_network_retry_delay(retry: usize) -> Duration {
    let exponent = if retry > 8 { 8 } else { retry };
    VERIFIER_NETWORK_RETRY_BASE_DELAY.saturating_mul(1_u32 << exponent)
}

async fn restore_verifier_resolver(
    session: &VmToolSession,
    launch: &VmLaunch,
) -> Result<(), VmAttemptError> {
    if launch.resolver_configuration.is_empty() {
        return Ok(());
    }
    let output = session
        .command(
            VmCommand::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "rm -f /etc/resolv.conf && printf '{}' > /etc/resolv.conf",
                    launch.resolver_configuration
                ))
                .timeout(Duration::from_secs(10)),
        )
        .await?;
    if output.exit_code != 0 {
        return Err(io::Error::other(format!(
            "restoring verifier DNS configuration exited {}: {}",
            output.exit_code,
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(())
}

async fn mount_verifier_cache(session: &VmToolSession) -> Result<(), VmAttemptError> {
    let output = session
        .command(
            VmCommand::new("/bin/sh")
                .arg("-c")
                .arg(format!(
                    "mkdir -p {VERIFIER_CACHE_MOUNT} /var/cache/apt/archives /var/lib/apt/lists /root/.cache/uv /root/.local && mount -t ext4 {VERIFIER_CACHE_BLOCK_DEVICE} {VERIFIER_CACHE_MOUNT} && mount --bind {VERIFIER_CACHE_MOUNT}/apt-archives /var/cache/apt/archives && mount --bind {VERIFIER_CACHE_MOUNT}/apt-lists /var/lib/apt/lists && mount --bind {VERIFIER_CACHE_MOUNT}/uv-cache /root/.cache/uv && mount --bind {VERIFIER_CACHE_MOUNT}/uv-home /root/.local"
                ))
                .timeout(Duration::from_secs(30)),
        )
        .await?;
    if output.exit_code != 0 {
        return Err(io::Error::other(format!(
            "mounting verifier cache exited {}: {}",
            output.exit_code,
            String::from_utf8_lossy(&output.stderr)
        ))
        .into());
    }
    Ok(())
}

fn remove_passed_rootfs(rootfs: &Path) -> io::Result<bool> {
    if !rootfs.is_file() {
        return Ok(false);
    }
    fs::remove_file(rootfs)?;
    Ok(true)
}

fn verifier_timeout_output(
    timeout: Duration,
    mut output: VmCommandPartialOutput,
) -> VmCommandOutput {
    output.stderr.extend_from_slice(
        format!(
            "\ncanonical verifier exceeded its {timeout:?} deadline; \
             the candidate is scored with reward 0\n"
        )
        .as_bytes(),
    );
    VmCommandOutput {
        exit_code: 124,
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

const fn verifier_shell(configured: &str, skip_setup: bool) -> &str {
    if skip_setup { "/bin/bash" } else { configured }
}

fn base_guest_environment(task: &Task, workspace: &str) -> Vec<(String, String)> {
    let mut environment = BTreeMap::from([
        (
            "PATH".to_owned(),
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned(),
        ),
        ("HOME".to_owned(), "/root".to_owned()),
        ("NANOCODEX_EVAL_WORKSPACE".to_owned(), workspace.to_owned()),
        (
            "NANOCODEX_EVAL_VERIFIER_LOGS".to_owned(),
            "/logs/verifier".to_owned(),
        ),
        // Retained tasks from the temporary Nanoeval repository still
        // consume these names.
        ("NANOEVAL_WORKSPACE".to_owned(), workspace.to_owned()),
        (
            "NANOEVAL_VERIFIER_LOGS".to_owned(),
            "/logs/verifier".to_owned(),
        ),
    ]);
    environment.extend(task.environment().clone());
    environment.extend(task.verifier().environment().clone());
    environment.into_iter().collect()
}

fn record_operation<T, E>(span: &tracing::Span, started_at: Instant, result: &Result<T, E>)
where
    E: std::fmt::Display,
{
    let duration_ns = u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX);
    span.record("duration_ns", duration_ns);
    match result {
        Ok(_) => {
            span.record("status", "completed");
            span.record("otel.status_code", "OK");
            span.in_scope(|| {
                info!(
                    target: "nanocodex_eval",
                    duration_ns,
                    status = "completed",
                    "VM attempt operation completed"
                );
            });
        }
        Err(error) => {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            span.record("error.message", tracing::field::display(error));
            span.in_scope(|| {
                info!(
                    target: "nanocodex_eval",
                    duration_ns,
                    status = "failed",
                    error = %error,
                    "VM attempt operation failed"
                );
            });
        }
    }
}

fn materialize_rootfs(source: &Path, destination: &Path) -> Result<(), VmAttemptError> {
    if !source.is_dir() {
        return Err(VmAttemptError::InvalidRootfs(source.to_path_buf()));
    }
    copy_root_entries(source, destination, true)
}

fn copy_root_entries(source: &Path, destination: &Path, root: bool) -> Result<(), VmAttemptError> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if root && matches!(entry.file_name().to_str(), Some("workspace" | "verifier")) {
            continue;
        }
        let source = entry.path();
        let target = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source)?;
        if metadata.file_type().is_symlink() {
            if target.exists() || fs::symlink_metadata(&target).is_ok() {
                return Err(VmAttemptError::Collision(target));
            }
            std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
        } else if metadata.is_dir() {
            if target.exists() && !target.is_dir() {
                return Err(VmAttemptError::Collision(target));
            }
            fs::create_dir_all(&target)?;
            copy_root_entries(&source, &target, false)?;
        } else if metadata.is_file() {
            if target.exists() {
                return Err(VmAttemptError::Collision(target));
            }
            fs::copy(source, target)?;
        } else {
            return Err(VmAttemptError::Collision(source));
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct RunReport {
    job_id: uuid::Uuid,
    job_directory: PathBuf,
    skipped: usize,
    summary: RunSummary,
    attempts: Vec<AttemptOutcome>,
}

impl RunReport {
    fn new(job: &HarborJob, mut attempts: Vec<AttemptOutcome>, skipped: usize) -> Self {
        attempts.sort_by(|left, right| left.trial_name().cmp(right.trial_name()));
        Self {
            job_id: job.id(),
            job_directory: job.directory().to_path_buf(),
            skipped,
            summary: RunSummary::from_attempts(&attempts),
            attempts,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
struct RunSummary {
    total: usize,
    passed: usize,
    failed: usize,
    refused: usize,
    errored: usize,
    known_estimated_cost_usd: Option<f64>,
    priced_attempts: usize,
}

impl RunSummary {
    fn from_attempts(attempts: &[AttemptOutcome]) -> Self {
        let mut summary = Self {
            total: attempts.len(),
            ..Self::default()
        };
        for attempt in attempts {
            match attempt {
                AttemptOutcome::Passed(result) => {
                    summary.passed += 1;
                    summary.record_estimated_cost(result.agent.cost_usd);
                }
                AttemptOutcome::Failed(result) => {
                    summary.failed += 1;
                    summary.record_estimated_cost(result.agent.cost_usd);
                }
                AttemptOutcome::Refused(_) => summary.refused += 1,
                AttemptOutcome::Errored(_) => summary.errored += 1,
            }
        }
        summary
    }

    fn record_estimated_cost(&mut self, cost_usd: Option<f64>) {
        let Some(cost_usd) = cost_usd else {
            return;
        };
        self.known_estimated_cost_usd =
            Some(self.known_estimated_cost_usd.unwrap_or_default() + cost_usd);
        self.priced_attempts += 1;
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", content = "details", rename_all = "snake_case")]
enum AttemptOutcome {
    Passed(EvalResult),
    Failed(EvalResult),
    Refused(EvalFailure),
    Errored(EvalFailure),
}

impl AttemptOutcome {
    const fn from_result(result: EvalResult) -> Self {
        match result.status {
            EvalStatus::Passed => Self::Passed(result),
            EvalStatus::Failed => Self::Failed(result),
        }
    }

    fn trial_name(&self) -> &str {
        match self {
            Self::Passed(result) | Self::Failed(result) => &result.trial_name,
            Self::Refused(failure) | Self::Errored(failure) => &failure.trial_name,
        }
    }

    fn from_failure(failure: EvalFailure) -> Self {
        if failure.kind == EvalFailureKind::AgentSafetyRefusal {
            Self::Refused(failure)
        } else {
            Self::Errored(failure)
        }
    }
}

struct Progress {
    outcomes: Vec<AttemptOutcome>,
    failed: usize,
}

impl Progress {
    fn scored_results(&self) -> Vec<EvalResult> {
        self.outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                AttemptOutcome::Passed(result) | AttemptOutcome::Failed(result) => {
                    Some(result.clone())
                }
                AttemptOutcome::Refused(_) | AttemptOutcome::Errored(_) => None,
            })
            .collect()
    }
}

async fn report_progress(
    mut events: EvalEventStream,
    mut expected_attempts: watch::Receiver<usize>,
    concurrency: usize,
    max_memory_mb: Option<u64>,
) -> Result<Progress> {
    let mut expected = *expected_attempts.borrow_and_update();
    let count = if expected == 1 { "" } else { "s" };
    if let Some(max_memory_mb) = max_memory_mb {
        eprintln!(
            "Running {expected} evaluation{count} (up to {concurrency} concurrent, \
             {max_memory_mb} MiB task-declared memory)"
        );
    } else {
        eprintln!("Running {expected} evaluation{count} (up to {concurrency} concurrent)");
    }
    let mut completed = 0;
    let mut outcomes = Vec::with_capacity(expected);
    let mut failed = 0;
    let mut expected_updates_open = true;
    while completed < expected {
        let event = if expected_updates_open {
            tokio::select! {
                update = expected_attempts.changed() => {
                    if update.is_ok() {
                        expected = *expected_attempts.borrow_and_update();
                    } else {
                        expected_updates_open = false;
                    }
                    continue;
                }
                event = events.recv() => event?,
            }
        } else {
            events.recv().await?
        }
        .ok_or_else(|| eyre!("event stream closed after {completed} of {expected} attempts"))?;
        match &event.kind {
            EvalEventKind::Completed(result) => {
                completed += 1;
                let outcome = AttemptOutcome::from_result(result.as_ref().clone());
                write_progress_line(&outcome, completed, expected);
                outcomes.push(outcome);
            }
            EvalEventKind::Failed(failure) => {
                completed += 1;
                failed += 1;
                let outcome = AttemptOutcome::from_failure(failure.as_ref().clone());
                write_progress_line(&outcome, completed, expected);
                outcomes.push(outcome);
            }
            EvalEventKind::AttemptStarted { .. }
            | EvalEventKind::Agent(_)
            | EvalEventKind::VerifierStarted
            | EvalEventKind::VerifierOutput { .. }
            | EvalEventKind::VerifierCompleted(_) => {}
        }
    }
    Ok(Progress { outcomes, failed })
}

fn write_progress_line(outcome: &AttemptOutcome, completed: usize, expected: usize) {
    match outcome {
        AttemptOutcome::Passed(result) => {
            let status = Painted::new(format!("[PASS {completed}/{expected}]")).green();
            eprintln!(
                "{status} {} ({})",
                result.trial_name,
                result_duration(result)
            );
        }
        AttemptOutcome::Failed(result) => {
            let status = Painted::new(format!("[FAIL {completed}/{expected}]")).red();
            eprintln!(
                "{status} {} ({}, reward={:.3})",
                result.trial_name,
                result_duration(result),
                result.verifier.rewards.values().sum::<f64>()
            );
        }
        AttemptOutcome::Refused(failure) => {
            let message = failure.message.lines().next().unwrap_or_default();
            let status = Painted::new(format!("[REFUSED {completed}/{expected}]")).yellow();
            eprintln!(
                "{status} {} ({}): {message}",
                failure.trial_name,
                format_milliseconds(
                    failure
                        .occurred_at
                        .signed_duration_since(failure.started_at)
                        .num_milliseconds()
                )
            );
        }
        AttemptOutcome::Errored(failure) => {
            let message = failure.message.lines().next().unwrap_or_default();
            let status = Painted::new(format!("[ERROR {completed}/{expected}]")).red();
            eprintln!(
                "{status} {} ({:?}, {}): {message}",
                failure.trial_name,
                failure.kind,
                format_milliseconds(
                    failure
                        .occurred_at
                        .signed_duration_since(failure.started_at)
                        .num_milliseconds()
                )
            );
        }
    }
}

fn result_duration(result: &EvalResult) -> String {
    format_milliseconds(
        result
            .timing
            .finished_at
            .signed_duration_since(result.timing.started_at)
            .num_milliseconds(),
    )
}

fn format_milliseconds(milliseconds: i64) -> String {
    let seconds = milliseconds / 1_000;
    let millis = milliseconds.unsigned_abs() % 1_000;
    format!("{seconds}.{millis:03}s")
}

#[cfg(test)]
mod tests {
    use std::{
        fs, future,
        path::{Path, PathBuf},
        process::Command as StdCommand,
        time::Duration,
    };

    use clap::Parser;
    use nanocodex_eval::Task;
    use nanocodex_vm::{VmCommandOutput, VmCommandPartialOutput};

    use super::{
        CACHED_VERIFIER_SCRIPT, DEFAULT_HOST_UTILIZATION_PERCENT, DEFAULT_TRIALS, HostResources,
        RetainedBuild, RetainedScheduling, Run, RunInvocation, RunMeasurements, RunSummary,
        VmRetention, cached_verifier_script, finish_or_drain, load_tasks,
        recognized_verifier_setup, remove_passed_rootfs, retained_retry_task_names,
        retained_task_durations, verifier_bootstrap_network_failed, verifier_cache_key,
        verifier_network_retry_delay, verifier_shell, verifier_timeout_output,
    };

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        eval: Run,
    }

    #[tokio::test]
    async fn injected_interrupt_closes_admission_then_waits_for_admitted_work() {
        let (release, released) = tokio::sync::oneshot::channel();
        let execution = finish_or_drain(
            async {
                released.await.unwrap();
                Ok::<_, &'static str>(17)
            },
            future::ready(Ok(())),
            9,
            || {
                release.send(()).unwrap();
                3
            },
        )
        .await
        .unwrap();

        assert_eq!(execution.result.unwrap(), 17);
        assert_eq!(execution.terminal_attempts, 3);
        assert!(execution.interrupted);
    }

    #[test]
    fn run_measurements_retain_cold_and_warm_phase_boundaries() {
        let output = tempfile::tempdir().unwrap();
        RunMeasurements {
            observability: Duration::from_nanos(1),
            task_loading: Duration::from_nanos(2),
            vm_runtime: Duration::from_nanos(3),
            vm_environments: Duration::from_nanos(4),
            evaluation_setup: Duration::from_nanos(5),
            attempts: Duration::from_nanos(6),
            harbor_finish: Duration::from_nanos(7),
            output: Duration::from_nanos(8),
            total: Duration::from_nanos(36),
        }
        .persist(output.path())
        .unwrap();

        let timing: serde_json::Value =
            serde_json::from_slice(&fs::read(output.path().join("timing.json")).unwrap()).unwrap();
        assert_eq!(timing["vm_runtime_build_ns"], 3);
        assert_eq!(timing["cold_image_and_cache_ns"], 4);
        assert_eq!(timing["attempts_wall_ns"], 6);
        assert_eq!(timing["total_wall_ns"], 36);
    }

    #[test]
    fn eval_image_builds_prefer_ipv4_and_use_a_sixty_minute_run_timeout() {
        assert_eq!(
            super::EVAL_IMAGE_BUILD_POLICY,
            super::EvalImageBuildPolicy {
                prefer_ipv4: true,
                run_timeout: Duration::from_mins(60),
            }
        );
    }

    #[test]
    fn vm_guest_build_targets_the_unified_vm_package() {
        let command = super::vm_guest_build_command(Path::new("/tmp/nanocodex-workspace"));
        let arguments = command
            .as_std()
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            arguments,
            [
                "build",
                "--quiet",
                "--target",
                super::VM_GUEST_TARGET,
                "--package",
                "nanocodex-vm",
                "--bin",
                "nanocodex-vm-guest",
                "--no-default-features",
                "--features",
                "guest-runtime",
            ]
        );
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(Path::new("/tmp/nanocodex-workspace"))
        );
    }

    #[test]
    fn guest_build_record_tracks_exact_cargo_dependencies() {
        let workspace = tempfile::tempdir().unwrap();
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config.toml",
            "crates/nanocodex-oai-api/Cargo.toml",
            "crates/nanocodex-tools/Cargo.toml",
            "crates/experimental/nanocodex-vm/Cargo.toml",
        ] {
            let path = workspace.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "fixture").unwrap();
        }
        let source = workspace
            .path()
            .join("crates/experimental/nanocodex-vm/src/tools/guest.rs");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fs::write(&source, "first guest source").unwrap();
        let runtime = workspace.path().join("target/guest/debug/guest");
        fs::create_dir_all(runtime.parent().unwrap()).unwrap();
        fs::write(&runtime, "guest binary").unwrap();
        fs::write(
            runtime.with_extension("d"),
            format!("{}: {}\n", runtime.display(), source.display()),
        )
        .unwrap();

        super::write_vm_guest_build_record(workspace.path(), &runtime).unwrap();
        assert!(super::vm_guest_runtime_is_fresh(workspace.path(), &runtime).unwrap());

        fs::write(source, "changed guest source with a different size").unwrap();
        assert!(!super::vm_guest_runtime_is_fresh(workspace.path(), &runtime).unwrap());
    }

    #[test]
    fn cargo_dep_info_parser_preserves_escaped_paths() {
        let paths = super::parse_cargo_dep_info(
            "/tmp/guest: /tmp/plain.rs /tmp/with\\ space.rs /tmp/back\\\\slash.rs\n",
        )
        .unwrap();

        assert_eq!(
            paths,
            [
                PathBuf::from("/tmp/plain.rs"),
                PathBuf::from("/tmp/with space.rs"),
                PathBuf::from("/tmp/back\\slash.rs"),
            ]
        );
    }

    #[test]
    fn vm_bootstrap_preserves_shell_words_without_libkrun_quotes() {
        let workspace = "/workspace with 'single' and \"double\"";
        let quoted = super::shell_word_without_double_quotes(workspace);
        let script = format!("printf %s {quoted}");
        assert!(!script.contains('"'));
        let output = StdCommand::new("/bin/sh")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, workspace.as_bytes());

        let bootstrap = super::vm_guest_bootstrap_script(workspace, "nameserver 192.168.127.1\\n");
        assert!(!bootstrap.contains('"'));
        assert!(bootstrap.contains(&quoted));
    }

    #[test]
    fn accepts_repeated_tasks_with_per_task_trials() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--task",
            "tasks/second",
            "--trials",
            "5",
            "--concurrency",
            "10",
            "--max-memory-mb",
            "24576",
            "--vm",
        ])
        .unwrap();

        assert_eq!(
            cli.eval.tasks,
            [PathBuf::from("tasks/first"), PathBuf::from("tasks/second")]
        );
        assert_eq!(cli.eval.trials, 5);
        assert_eq!(cli.eval.concurrency, Some(10));
        assert_eq!(cli.eval.max_memory_mb, Some(24_576));
        assert_eq!(cli.eval.host_utilization, DEFAULT_HOST_UTILIZATION_PERCENT);
        assert!(cli.eval.vm);
        assert!(!cli.eval.vm_retention.unwrap_or_default().retains_passes());
        assert!(cli.eval.suites.is_empty());
    }

    #[test]
    fn defaults_to_five_independent_trials_per_task() {
        let cli = TestCli::try_parse_from(["nanoeval", "--task", "tasks/first"]).unwrap();

        let resolved = cli.eval.resolve_run().unwrap();

        assert_eq!(cli.eval.trials, DEFAULT_TRIALS);
        assert_eq!(resolved.trials, DEFAULT_TRIALS);
        assert!(!resolved.web_search);
    }

    #[test]
    fn web_search_is_an_explicit_eval_capability() {
        let cli =
            TestCli::try_parse_from(["nanoeval", "--task", "tasks/first", "--web-search"]).unwrap();

        let resolved = cli.eval.resolve_run().unwrap();

        assert!(resolved.web_search);
    }

    #[test]
    fn cost_summary_distinguishes_known_and_unpriced_attempts() {
        let mut summary = RunSummary {
            total: 3,
            ..RunSummary::default()
        };

        summary.record_estimated_cost(Some(0.125));
        summary.record_estimated_cost(None);
        summary.record_estimated_cost(Some(0.375));

        assert_eq!(summary.known_estimated_cost_usd, Some(0.5));
        assert_eq!(summary.priced_attempts, 2);
    }

    #[test]
    fn host_defaults_use_the_configured_share_of_cpu_and_memory() {
        let host = HostResources {
            logical_cpus: 10,
            physical_memory_bytes: Some(32 * 1024 * 1024 * 1024),
        };

        let defaults = host.scheduling_defaults(80);

        assert_eq!(defaults.concurrency, 8);
        assert_eq!(defaults.max_memory_mb, Some(26_214));
    }

    #[test]
    fn host_defaults_keep_at_least_one_execution_slot() {
        let host = HostResources {
            logical_cpus: 1,
            physical_memory_bytes: None,
        };

        let defaults = host.scheduling_defaults(1);

        assert_eq!(defaults.concurrency, 1);
        assert_eq!(defaults.max_memory_mb, None);
    }

    #[test]
    fn explicit_scheduler_limits_disable_automatic_resolution() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--concurrency",
            "3",
            "--max-memory-mb",
            "4096",
        ])
        .unwrap();

        let resolved = cli.eval.resolve_run().unwrap();

        assert_eq!(resolved.concurrency, 3);
        assert_eq!(resolved.max_memory_mb, Some(4_096));
        assert_eq!(resolved.automatic_scheduling, None);
    }

    #[test]
    fn resumed_workload_allows_scheduler_changes_only() {
        let retained = RunInvocation {
            version: super::INVOCATION_VERSION,
            nanocodex_build: RetainedBuild {
                version: "test".to_owned(),
                git_sha: "0123456789abcdef".to_owned(),
                built_at: "2026-07-28T00:00:00Z".to_owned(),
                executable_sha256: "abc123".to_owned(),
            },
            model: "gpt-5.6-sol".to_owned(),
            pricing_revision: "test-pricing-v1".to_owned(),
            tool_profile: "microvm_workspace".to_owned(),
            seed: None,
            scheduling: RetainedScheduling {
                policy: super::SCHEDULING_POLICY.to_owned(),
                automatic_utilization_percent: Some(80),
                concurrency_source: "automatic".to_owned(),
                memory_source: "automatic".to_owned(),
            },
            trials: 5,
            concurrency: 16,
            max_memory_mb: Some(49_152),
            vm: true,
            vm_rootfs: None,
            vm_retention: VmRetention::Failures,
            thinking: "xhigh".to_owned(),
            web_search: false,
            rerun_from: None,
        };
        let mut resumed = retained.clone();
        resumed.concurrency = 30;
        resumed.max_memory_mb = Some(58_000);

        assert!(retained.same_workload(&resumed));

        resumed.thinking = "high".to_owned();
        assert!(!retained.same_workload(&resumed));
    }

    #[test]
    fn passed_vm_retention_is_explicit() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--task",
            "tasks/first",
            "--vm",
            "--vm-retention",
            "all",
        ])
        .unwrap();

        assert!(cli.eval.vm_retention.unwrap().retains_passes());
    }

    #[test]
    fn rerun_is_a_task_source_with_foundry_style_name_filters() {
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--rerun",
            "webserver",
            "--rerun-from",
            "job-id",
            "--match-task",
            "torch-.*",
            "--match-task",
            "mteb",
            "--include-errored",
            "--list",
        ])
        .unwrap();

        assert!(cli.eval.retry.rerun);
        assert_eq!(cli.eval.retry.rerun_from, Some(PathBuf::from("job-id")));
        assert_eq!(cli.eval.retry.names, ["webserver"]);
        assert_eq!(cli.eval.retry.match_task, ["torch-.*", "mteb"]);
        assert!(cli.eval.retry.statuses.include_errored);
        assert!(cli.eval.retry.list);
        assert!(cli.eval.tasks.is_empty());
        assert!(cli.eval.suites.is_empty());
    }

    #[test]
    fn positional_rerun_names_are_literal_substrings() {
        let cli = TestCli::try_parse_from(["nanoeval", "--rerun", "task.+", "--list"]).unwrap();
        let matcher = super::retry_matcher(&cli.eval.retry).unwrap().unwrap();

        assert!(matcher.is_match("terminal-bench/task.+example"));
        assert!(!matcher.is_match("terminal-bench/taskXYZexample"));
    }

    #[test]
    fn passed_rootfs_cleanup_removes_only_a_disk_file() {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = directory.path().join("rootfs.ext4");
        fs::write(&rootfs, b"guest disk").unwrap();

        assert!(remove_passed_rootfs(&rootfs).unwrap());
        assert!(!rootfs.exists());
        assert!(!remove_passed_rootfs(directory.path()).unwrap());
    }

    #[test]
    fn suite_loads_immediate_tasks_in_name_order() {
        let suite = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tasks");
        let cli = TestCli::try_parse_from([
            "nanoeval",
            "--suite",
            suite.to_str().unwrap(),
            "--concurrency",
            "3",
        ])
        .unwrap();
        let tasks = load_tasks(cli.eval.tasks, cli.eval.suites).unwrap();
        let names = tasks.iter().map(Task::name).collect::<Vec<_>>();

        assert_eq!(
            names,
            [
                "nanoeval/extract-todos",
                "nanoeval/uppercase-message",
                "nanoeval/write-greeting"
            ]
        );
    }

    #[test]
    fn retained_task_duration_uses_the_median_completed_trial() {
        let output = tempfile::tempdir().unwrap();
        let job = output.path().join("job");
        for (trial, finished_at) in [
            ("first", "2026-07-23T00:00:10Z"),
            ("second", "2026-07-23T00:00:30Z"),
            ("third", "2026-07-23T00:00:20Z"),
        ] {
            let directory = job.join(trial);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"terminal-bench/example","started_at":"2026-07-23T00:00:00Z","finished_at":"{finished_at}"}}"#
                ),
            )
            .unwrap();
        }

        let estimates = retained_task_durations(output.path()).unwrap();
        assert_eq!(
            estimates["terminal-bench/example"],
            std::time::Duration::from_secs(20)
        );
    }

    #[test]
    fn retry_selection_distinguishes_scores_refusals_and_errors() {
        let job = tempfile::tempdir().unwrap();
        for (trial, result) in [
            (
                "passed",
                r#"{"task_name":"terminal-bench/passed","verifier_result":{"rewards":{"reward":1.0}},"exception_info":null}"#,
            ),
            (
                "partially-failed",
                r#"{"task_name":"terminal-bench/partially-failed","verifier_result":{"rewards":{"first":1.0,"second":0.0}},"exception_info":null}"#,
            ),
            (
                "failed",
                r#"{"task_name":"terminal-bench/torch-failed","verifier_result":{"rewards":{"reward":0.0}},"exception_info":null}"#,
            ),
            (
                "refused",
                r#"{"task_name":"terminal-bench/refused","verifier_result":null,"exception_info":{"exception_type":"AgentSafetyRefusalError"}}"#,
            ),
            (
                "errored",
                r#"{"task_name":"terminal-bench/errored","verifier_result":null,"exception_info":{"exception_type":"VerifierError"}}"#,
            ),
        ] {
            let directory = job.path().join(trial);
            fs::create_dir(&directory).unwrap();
            fs::write(directory.join("result.json"), result).unwrap();
        }

        let failed = retained_retry_task_names(job.path(), false, false, None).unwrap();
        assert_eq!(
            failed.task_names,
            [
                "terminal-bench/partially-failed".to_owned(),
                "terminal-bench/torch-failed".to_owned()
            ]
            .into()
        );

        let matcher = regex::RegexSet::new(["torch|errored"]).unwrap();
        let selected = retained_retry_task_names(job.path(), true, true, Some(&matcher)).unwrap();
        assert_eq!(
            selected.task_names,
            [
                "terminal-bench/errored".to_owned(),
                "terminal-bench/torch-failed".to_owned()
            ]
            .into()
        );
    }

    #[test]
    fn retry_selection_uses_pass_at_k_across_trials() {
        let job = tempfile::tempdir().unwrap();
        for (trial, task, verifier_result, exception_info) in [
            (
                "eventual-pass-failed",
                "terminal-bench/eventual-pass",
                r#"{"rewards":{"reward":0.0}}"#,
                "null",
            ),
            (
                "eventual-pass-passed",
                "terminal-bench/eventual-pass",
                r#"{"rewards":{"reward":1.0}}"#,
                "null",
            ),
            (
                "scored-failure",
                "terminal-bench/scored-failure",
                r#"{"rewards":{"reward":0.0}}"#,
                "null",
            ),
            (
                "scored-failure-error",
                "terminal-bench/scored-failure",
                "null",
                r#"{"exception_type":"AgentTimeoutError"}"#,
            ),
        ] {
            let directory = job.path().join(trial);
            fs::create_dir(&directory).unwrap();
            fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"{task}","verifier_result":{verifier_result},"exception_info":{exception_info}}}"#
                ),
            )
            .unwrap();
        }

        let queue = retained_retry_task_names(job.path(), false, false, None).unwrap();

        assert_eq!(
            queue.task_names,
            ["terminal-bench/scored-failure".to_owned()].into()
        );
    }

    #[test]
    fn retry_lineage_overlays_only_tasks_present_in_the_child_job() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("base");
        let child = root.path().join("child");
        for (job, trial, task, reward) in [
            (&base, "first", "terminal-bench/first", 0.0),
            (&base, "second", "terminal-bench/second", 0.0),
            (&child, "first-retry", "terminal-bench/first", 1.0),
        ] {
            let directory = job.join(trial);
            fs::create_dir_all(&directory).unwrap();
            fs::write(
                directory.join("result.json"),
                format!(
                    r#"{{"task_name":"{task}","verifier_result":{{"rewards":{{"reward":{reward}}}}},"exception_info":null}}"#
                ),
            )
            .unwrap();
        }
        super::write_json_atomic(
            &child.join(super::INVOCATION_FILE),
            &super::RunInvocation {
                version: super::INVOCATION_VERSION,
                nanocodex_build: super::RetainedBuild {
                    version: "test".to_owned(),
                    git_sha: "0123456789abcdef".to_owned(),
                    built_at: "2026-07-28T00:00:00Z".to_owned(),
                    executable_sha256: "abc123".to_owned(),
                },
                model: "gpt-5.6-sol".to_owned(),
                pricing_revision: "test-pricing-v1".to_owned(),
                tool_profile: "native_workspace".to_owned(),
                seed: None,
                scheduling: super::RetainedScheduling {
                    policy: super::SCHEDULING_POLICY.to_owned(),
                    automatic_utilization_percent: None,
                    concurrency_source: "configured".to_owned(),
                    memory_source: "configured".to_owned(),
                },
                trials: 1,
                concurrency: 1,
                max_memory_mb: None,
                vm: false,
                vm_rootfs: None,
                vm_retention: super::VmRetention::Failures,
                thinking: "low".to_owned(),
                web_search: false,
                rerun_from: Some(base.canonicalize().unwrap()),
            },
        )
        .unwrap();

        let queue = retained_retry_task_names(&child, false, false, None).unwrap();

        assert_eq!(queue.lineage.len(), 2);
        assert_eq!(
            queue.task_names,
            ["terminal-bench/second".to_owned()].into()
        );
    }

    #[test]
    fn legacy_last_run_marker_remains_readable() {
        let root = tempfile::tempdir().unwrap();
        let job = root.path().join("job");
        fs::create_dir(&job).unwrap();
        fs::write(job.join("result.json"), "{}").unwrap();
        let marker = root.path().join(".nanoeval/last-run.json");
        super::write_json_atomic(&marker, &super::LastRun { job: job.clone() }).unwrap();

        let resolved = super::completed_job_from_last_run(None, [marker.as_path()]).unwrap();

        assert_eq!(resolved, job.canonicalize().unwrap());
    }

    #[test]
    fn cold_verifier_uses_the_prepared_environment_shell() {
        assert_eq!(verifier_shell("sh", false), "sh");
        assert_eq!(verifier_shell("bash", false), "bash");
        assert_eq!(verifier_shell("sh", true), "/bin/bash");
    }

    #[test]
    fn requires_at_least_one_task() {
        let Err(error) = TestCli::try_parse_from(["nanoeval"]) else {
            panic!("a task should be required");
        };
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn cached_verifier_omits_the_complete_pinned_uv_bootstrap() {
        assert!(CACHED_VERIFIER_SCRIPT.starts_with("/tmp/"));
        let supported = br"#!/bin/bash
# Install curl
apt-get update
apt-get install -y curl
# Install uv
curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
uvx pytest
";
        let setup = recognized_verifier_setup(supported).unwrap();
        assert!(setup.skip_setup);
        assert_eq!(&supported[..setup.cacheable_start], b"#!/bin/bash\n");
        let omitted = &supported[setup.cacheable_start..setup.cacheable_end];
        assert!(omitted.windows(7).any(|window| window == b"apt-get"));
        assert!(omitted.windows(9).any(|window| window == b"astral.sh"));
        assert!(omitted.windows(7).any(|window| window == b"source "));
        assert!(!omitted.windows(4).any(|window| window == b"uvx "));
        let transformed = cached_verifier_script(supported, setup);
        let transformed = std::str::from_utf8(&transformed).unwrap();
        assert!(transformed.starts_with("#!/bin/bash\n"));
        assert!(!transformed.contains("apt-get"));
        assert!(transformed.contains("source /root/.local/bin/env"));
        assert!(!transformed.contains("astral.sh"));
        assert!(transformed.contains("uvx pytest"));

        assert!(recognized_verifier_setup(b"pip install pytest\npytest").is_none());
        assert!(
            recognized_verifier_setup(
                br"apt-get update
apt-get install -y curl
curl -LsSf https://astral.sh/uv/latest/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
"
            )
            .is_none()
        );
        let custom_setup = recognized_verifier_setup(
            br"#!/bin/bash
apt-get update
apt-get install -y curl git libgl1
curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
",
        )
        .unwrap();
        assert!(!custom_setup.skip_setup);

        let stateful_setup = recognized_verifier_setup(
            br"#!/bin/bash
apt-get update
apt-get install -y curl
touch /root/extra-state
curl -LsSf https://astral.sh/uv/0.9.5/install.sh | sh
source $HOME/.local/bin/env
# Check if we're in a valid working directory
",
        )
        .unwrap();
        assert!(!stateful_setup.skip_setup);

        let key = verifier_cache_key(
            std::ffi::OsStr::new("rootfs.ext4"),
            omitted,
            512 * 1024 * 1024,
        );
        let different_verifier_body = supported
            .strip_suffix(b"uvx pytest\n")
            .unwrap()
            .iter()
            .copied()
            .chain(b"uvx python -m unittest\n".iter().copied())
            .collect::<Vec<_>>();
        let different_setup = recognized_verifier_setup(&different_verifier_body).unwrap();
        assert_eq!(
            key,
            verifier_cache_key(
                std::ffi::OsStr::new("rootfs.ext4"),
                &different_verifier_body
                    [different_setup.cacheable_start..different_setup.cacheable_end],
                512 * 1024 * 1024,
            )
        );
    }

    #[test]
    fn retries_only_dependency_bootstrap_network_failures() {
        let dns_failure = VmCommandOutput {
            exit_code: 0,
            stdout: b"curl: (6) Could not resolve host: astral.sh\n\
                /tests/test.sh: line 19: uvx: command not found\n"
                .to_vec(),
            stderr: Vec::new(),
        };
        assert!(verifier_bootstrap_network_failed(&dns_failure));

        let gateway_failure = VmCommandOutput {
            exit_code: 0,
            stdout: b"failed to download https://github.com/astral-sh/uv/releases/download/uv\n\
                curl: (22) The requested URL returned error: 504\n\
                /tests/test.sh: line 19: uvx: command not found\n"
                .to_vec(),
            stderr: Vec::new(),
        };
        assert!(verifier_bootstrap_network_failed(&gateway_failure));

        let apt_dns_failure = VmCommandOutput {
            exit_code: 100,
            stdout: Vec::new(),
            stderr: b"Temporary failure resolving 'deb.debian.org'\n".to_vec(),
        };
        assert!(verifier_bootstrap_network_failed(&apt_dns_failure));

        let genuine_test_failure = VmCommandOutput {
            exit_code: 0,
            stdout: b"FAILED test_outputs.py::test_data_matches\n\
                AssertionError: result.txt contains unexpected value\n"
                .to_vec(),
            stderr: Vec::new(),
        };
        assert!(!verifier_bootstrap_network_failed(&genuine_test_failure));

        let task_owned_download_failure = VmCommandOutput {
            exit_code: 0,
            stdout: b"Could not resolve host: github.com\nFAILED test_outputs.py\n".to_vec(),
            stderr: Vec::new(),
        };
        assert!(!verifier_bootstrap_network_failed(
            &task_owned_download_failure
        ));
        assert_eq!(
            (0..=4)
                .map(verifier_network_retry_delay)
                .collect::<Vec<_>>(),
            [2, 4, 8, 16, 32].map(std::time::Duration::from_secs)
        );
    }

    #[test]
    fn verifier_timeout_preserves_partial_output_bytes() {
        let output = verifier_timeout_output(
            Duration::from_secs(17),
            VmCommandPartialOutput {
                stdout: vec![0, 0xff, b'\n'],
                stderr: vec![0x80, b'\n'],
            },
        );

        assert_eq!(output.exit_code, 124);
        assert_eq!(output.stdout, [0, 0xff, b'\n']);
        assert_eq!(
            output.stderr,
            [
                &[0x80, b'\n'][..],
                b"\ncanonical verifier exceeded its 17s deadline; \
                  the candidate is scored with reward 0\n",
            ]
            .concat()
        );
    }
}
