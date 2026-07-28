//! Harbor-compatible artifacts for [`crate`].
//!
//! The evaluator's typed result is authoritative. This module subscribes to its
//! optional event stream and durably projects each attempt into Harbor job,
//! trial, trajectory, verifier, and ATIF files.
//!
//! # Record a job
//!
//! ```no_run
//! use nanocodex_agent::{Nanocodex, OpenAi};
//! use nanocodex_eval::{Evaluator, Task, harbor::Harbor};
//!
//! # async fn evaluate() -> Result<(), Box<dyn std::error::Error>> {
//! let agent = Nanocodex::builder(OpenAi::new(std::env::var("OPENAI_API_KEY")?)?).instructions(
//!     "Work in the provided workspace, complete the task, and verify it.",
//! );
//! let (evaluator, events) = Evaluator::builder(agent)
//!     .output_directory(".nanocodex/evals")
//!     .build()?;
//! let recorder = Harbor::new(&evaluator)?.record(events.subscribe())?;
//! let result = evaluator.task(Task::load("tasks/write-greeting")?).await?;
//! let job = recorder.finish(vec![result]).await?;
//! println!("{}", job.directory().display());
//! # Ok(())
//! # }
//! ```

#![deny(missing_docs, rustdoc::broken_intra_doc_links)]

mod checksum;
mod published;

pub use published::{
    PublishedAgent, PublishedAgentDetails, PublishedAgentInfo, PublishedAttempt, PublishedAttempts,
    PublishedError, PublishedModelInfo, PublishedObservation, PublishedObservationResult,
    PublishedQuery, PublishedResults, PublishedResultsBuilder, PublishedStep, PublishedStepId,
    PublishedTask, PublishedToolCall, PublishedTrajectory, PublishedTrial,
};

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufWriter, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

use crate::{
    AgentMetadata, AggregateDataset, AtifBuilder, AtifTrajectory, AttemptFact,
    AttemptFactArtifacts, EvalEnvironment, EvalEventKind, EvalEventStream, EvalEventStreamError,
    EvalFailure, EvalResult, Evaluator, LatencyBreakdown, PhaseTiming, Task, TaskLoadError,
    durable::scan_manifest_trials,
    sweep::{RunCoordinate, RunManifest},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::{sync::oneshot, task::JoinHandle};
use url::Url;
use uuid::Uuid;

use checksum::directory_hash;

#[derive(Debug, thiserror::Error)]
/// An error produced while recording or publishing Harbor-compatible artifacts.
pub enum HarborError {
    /// A filesystem operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A Harbor JSON document could not be encoded or decoded.
    #[error(transparent)]
    Json(#[from] serde_json::Error),

    /// A task package changed or became unreadable before artifact projection.
    #[error(transparent)]
    TaskPackage(#[from] TaskLoadError),

    /// A task directory contained no packageable files.
    #[error("task directory is empty: {0}")]
    EmptyTask(PathBuf),

    /// Following a symbolic link would make task packaging cyclic.
    #[error("task directory contains a cyclic symbolic link: {0}")]
    CyclicTaskDirectory(PathBuf),

    /// A trial path could not be represented as a `file:` URL.
    #[error("trial directory cannot be represented as a file URL: {0}")]
    InvalidTrialPath(PathBuf),

    /// A retained terminal result did not belong to its finite run.
    #[error("invalid durable evaluation trial: {0}")]
    InvalidDurableTrial(String),

    /// A terminal result was about to be committed before one of its artifacts.
    #[error("terminal evaluation result prerequisite is missing: {0}")]
    MissingTerminalPrerequisite(PathBuf),

    /// The evaluator event subscription lagged or otherwise failed.
    #[error(transparent)]
    EventStream(#[from] EvalEventStreamError),

    /// An event referenced an attempt before its start event.
    #[error("received events for attempt {0} before attempt.started")]
    MissingAttempt(Uuid),

    /// More than one start event was received for an attempt.
    #[error("received duplicate attempt.started for attempt {0}")]
    DuplicateAttempt(Uuid),

    /// The recorder task stopped before it could be finalized.
    #[error("Harbor recorder stopped before finish")]
    RecorderStopped,

    /// The evaluator event stream ended before the requested batch completed.
    #[error("Evaluator event stream closed before Harbor recording finished")]
    EventStreamClosed,

    /// The background recorder task failed.
    #[error("Harbor recorder task failed: {0}")]
    Join(#[from] tokio::task::JoinError),

    /// Recording was started without an active Tokio runtime.
    #[error("Harbor recording requires an active Tokio runtime: {0}")]
    Runtime(#[from] tokio::runtime::TryCurrentError),
}

/// Explicit Harbor compatibility adapter for one evaluation job.
pub struct Harbor {
    artifacts: HarborArtifacts,
}

/// Active, streaming Harbor projection of an independent event subscription.
pub struct HarborRecorder {
    finish: Option<oneshot::Sender<FinishRequest>>,
    task: Option<JoinHandle<Result<HarborJob, HarborError>>>,
}

#[derive(Clone, Debug)]
/// A durably committed Harbor-compatible evaluation job.
pub struct HarborJob {
    id: Uuid,
    directory: PathBuf,
}

impl Harbor {
    /// Attaches the adapter to a reusable evaluator and its artifact directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the evaluator directory cannot be initialized with
    /// Harbor job metadata.
    pub fn new(eval: &Evaluator) -> Result<Self, HarborError> {
        Ok(Self {
            artifacts: HarborArtifacts::attach(eval)?,
        })
    }

    /// Starts consuming one independent event subscription immediately.
    ///
    /// # Errors
    ///
    /// Returns an error when called without an active Tokio runtime.
    pub fn record(self, events: EvalEventStream) -> Result<HarborRecorder, HarborError> {
        let (finish, finish_receiver) = oneshot::channel();
        let task = tokio::runtime::Handle::try_current()?.spawn(record(
            self.artifacts,
            events,
            finish_receiver,
        ));
        Ok(HarborRecorder {
            finish: Some(finish),
            task: Some(task),
        })
    }
}

impl HarborRecorder {
    /// Waits until every supplied result's terminal event has been recorded,
    /// then commits the final Harbor job result.
    ///
    /// # Errors
    ///
    /// Returns an error on event lag, malformed event payloads, filesystem
    /// failures, or premature recorder termination.
    pub async fn finish(mut self, results: Vec<EvalResult>) -> Result<HarborJob, HarborError> {
        self.finish
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .send(FinishRequest::Results(results))
            .map_err(|_| HarborError::RecorderStopped)?;
        self.task
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .await?
    }

    /// Finishes after the requested number of completed or errored attempts.
    ///
    /// This is the batch boundary used when individual attempts may fail while
    /// the evaluator continues running unrelated work.
    ///
    /// # Errors
    ///
    /// Returns an error on event lag, malformed event payloads, filesystem
    /// failures, premature recorder termination, or a mismatched attempt count.
    pub async fn finish_all(mut self, attempts: usize) -> Result<HarborJob, HarborError> {
        self.finish
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .send(FinishRequest::TerminalCount(attempts))
            .map_err(|_| HarborError::RecorderStopped)?;
        self.task
            .take()
            .ok_or(HarborError::RecorderStopped)?
            .await?
    }
}

impl Drop for HarborRecorder {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl HarborJob {
    /// Returns the stable job identifier.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }

    /// Returns the directory containing the committed Harbor artifacts.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Reconstructs the stable aggregate dataset from every durable trial.
    ///
    /// This includes trials completed by an earlier process and skipped when a
    /// finite run resumes.
    ///
    /// # Errors
    ///
    /// Returns an error when retained JSON cannot be read or a retained sweep
    /// trial does not match its original run coordinate.
    pub fn aggregate_dataset(&self) -> Result<AggregateDataset, HarborError> {
        let manifest =
            HarborArtifacts::read_json_if_exists::<RunManifest>(&self.directory.join("run.json"))?;
        let mut attempts =
            HarborArtifacts::durable_trials(&self.directory, self.id, manifest.as_ref())?
                .into_iter()
                .map(|trial| {
                    let (configuration, repetition) = match trial.coordinate.as_ref() {
                        Some(coordinate) => (
                            coordinate.agent().as_str().to_owned(),
                            coordinate.repetition(),
                        ),
                        None => (
                            "default".to_owned(),
                            fallback_repetition(&trial.result.trial_name),
                        ),
                    };
                    Ok(trial.attempt_fact(configuration, repetition))
                })
                .collect::<Result<Vec<_>, HarborError>>()?;
        attempts.sort_by(|left, right| {
            (
                left.task_name.as_str(),
                left.configuration.as_str(),
                left.repetition,
                left.attempt_id,
            )
                .cmp(&(
                    right.task_name.as_str(),
                    right.configuration.as_str(),
                    right.repetition,
                    right.attempt_id,
                ))
        });
        Ok(AggregateDataset::new(attempts))
    }
}

struct AttemptRecording {
    events: BufWriter<File>,
    atif: AtifBuilder,
}

enum FinishRequest {
    Results(Vec<EvalResult>),
    TerminalCount(usize),
}

fn finished_attempt_count(
    request: Option<&FinishRequest>,
    completed: &HashSet<Uuid>,
) -> Option<usize> {
    match request? {
        FinishRequest::Results(results)
            if results
                .iter()
                .all(|result| completed.contains(&result.attempt_id)) =>
        {
            Some(results.len())
        }
        FinishRequest::TerminalCount(expected) if completed.len() == *expected => Some(*expected),
        FinishRequest::Results(_) | FinishRequest::TerminalCount(_) => None,
    }
}

async fn record(
    artifacts: HarborArtifacts,
    mut events: EvalEventStream,
    mut finish: oneshot::Receiver<FinishRequest>,
) -> Result<HarborJob, HarborError> {
    let mut attempts = HashMap::<Uuid, AttemptRecording>::new();
    let mut completed = HashSet::<Uuid>::new();
    let mut finish_request = None::<FinishRequest>;

    loop {
        if let Some(n_total_trials) = finished_attempt_count(finish_request.as_ref(), &completed) {
            artifacts.write_job(n_total_trials)?;
            return Ok(HarborJob {
                id: artifacts.job_id,
                directory: artifacts.root.clone(),
            });
        }

        tokio::select! {
            requested = &mut finish, if finish_request.is_none() => {
                finish_request = Some(requested.map_err(|_| HarborError::RecorderStopped)?);
            }
            event = events.recv() => {
                let event = event?.ok_or(HarborError::EventStreamClosed)?;
                match &event.kind {
                    EvalEventKind::AttemptStarted { prompt, .. } => {
                        let writer = artifacts.write_input(
                            event.attempt_id,
                            &event.trial_name,
                            prompt,
                        )?;
                        if attempts.insert(event.attempt_id, AttemptRecording {
                            events: writer,
                            atif: AtifBuilder::default(),
                        }).is_some() {
                            return Err(HarborError::DuplicateAttempt(event.attempt_id));
                        }
                    }
                    EvalEventKind::Agent(agent_event) => {
                        let attempt = attempts
                            .get_mut(&event.attempt_id)
                            .ok_or(HarborError::MissingAttempt(event.attempt_id))?;
                        serde_json::to_writer(&mut attempt.events, agent_event)?;
                        attempt.events.write_all(b"\n")?;
                        attempt.events.flush()?;
                        attempt.atif.apply(agent_event)?;
                    }
                    EvalEventKind::Completed(result) => {
                        let mut attempt = attempts
                            .remove(&event.attempt_id)
                            .ok_or(HarborError::MissingAttempt(event.attempt_id))?;
                        attempt.events.flush()?;
                        attempt.events.get_ref().sync_all()?;
                        let result = result.as_ref().clone();
                        let trajectory = attempt.atif.finish(result.task(), &result.agent);
                        artifacts.write_trial(&result, &trajectory)?;
                        completed.insert(result.attempt_id);
                        artifacts.write_job(completed.len())?;
                    }
                    EvalEventKind::Failed(failure) => {
                        let trajectory = if let Some(mut attempt) = attempts.remove(&event.attempt_id) {
                            attempt.events.flush()?;
                            attempt.events.get_ref().sync_all()?;
                            attempt.atif.finish_failure(failure.task())
                        } else {
                            let mut events = artifacts.write_input(
                                event.attempt_id,
                                &event.trial_name,
                                failure.task().prompt(),
                            )?;
                            events.flush()?;
                            events.get_ref().sync_all()?;
                            AtifBuilder::default().finish_failure(failure.task())
                        };
                        let failure = failure.as_ref().clone();
                        artifacts.write_failure(&failure, &trajectory)?;
                        completed.insert(failure.attempt_id);
                        artifacts.write_job(completed.len())?;
                    }
                    EvalEventKind::VerifierStarted
                    | EvalEventKind::VerifierOutput { .. }
                    | EvalEventKind::VerifierCompleted(_) => {}
                }
            }
        }
    }
}

struct HarborArtifacts {
    job_id: Uuid,
    started_at: DateTime<Utc>,
    root: PathBuf,
    jobs_dir: PathBuf,
    max_concurrency: usize,
    environment: EvalEnvironment,
    planned_attempts: Option<usize>,
    manifest: Option<RunManifest>,
    baseline: Option<HarborJobResult>,
    recorded_trials: Mutex<Vec<HarborRecordedTrial>>,
}

impl HarborArtifacts {
    fn attach(eval: &Evaluator) -> Result<Self, HarborError> {
        let root = eval.directory().to_path_buf();
        let manifest = Self::read_json_if_exists::<RunManifest>(&root.join("run.json"))?;
        let baseline = Self::read_json_if_exists(&root.join("result.json"))?;
        let recorded_trials = Self::recorded_trials(&root, eval.id(), manifest.as_ref())?;
        let artifacts = Self {
            job_id: eval.id(),
            started_at: eval.started_at(),
            root,
            jobs_dir: eval.parent_directory().to_path_buf(),
            max_concurrency: eval.max_concurrency(),
            environment: eval.attempt_environment(),
            planned_attempts: eval.planned_attempts(),
            manifest,
            baseline,
            recorded_trials: Mutex::new(recorded_trials),
        };
        Self::write_file(&artifacts.root.join("job.log"), [])?;
        artifacts.write_job_metadata()?;
        artifacts.write_job(artifacts.planned_attempts.unwrap_or(0))?;
        Ok(artifacts)
    }

    fn recorded_trials(
        root: &Path,
        job_id: Uuid,
        manifest: Option<&RunManifest>,
    ) -> Result<Vec<HarborRecordedTrial>, HarborError> {
        let mut recorded = Vec::new();
        for trial in Self::durable_result_paths(root, job_id, manifest)? {
            let Some(lock) =
                Self::read_json_if_exists::<HarborTrialLock>(&trial.directory.join("lock.json"))?
            else {
                return Err(HarborError::InvalidDurableTrial(format!(
                    "durable trial {} has no per-trial lock",
                    trial.directory.display()
                )));
            };
            recorded.push(Self::recorded_trial(lock));
        }
        Ok(recorded)
    }

    fn recorded_trial(lock: HarborTrialLock) -> HarborRecordedTrial {
        HarborRecordedTrial {
            task: HarborTaskConfig {
                path: lock.task.path.clone(),
                source: lock.task.source.clone(),
            },
            agent: lock.agent.clone(),
            lock,
        }
    }

    fn write_input(
        &self,
        attempt_id: Uuid,
        trial_name: &str,
        prompt: &str,
    ) -> Result<BufWriter<File>, HarborError> {
        let root = self.root.join(trial_name);
        let agent = root.join("agent");
        fs::create_dir_all(&agent)?;
        let input = HarborInput {
            protocol_version: 1,
            request_id: Some(attempt_id.to_string()),
            kind: "input",
            payload: HarborInputPayload {
                instruction: prompt,
            },
        };
        let mut bytes = serde_json::to_vec(&input)?;
        bytes.push(b'\n');
        Self::write_file(&agent.join("input.jsonl"), bytes)?;
        Ok(BufWriter::new(File::create(agent.join("events.jsonl"))?))
    }

    fn write_trial(
        &self,
        result: &EvalResult,
        trajectory: &AtifTrajectory,
    ) -> Result<(), HarborError> {
        let task = result.task();
        task.validate_package()?;
        let root = &result.artifacts.directory;
        let agent = root.join("agent");
        let input_path = agent.join("input.jsonl");
        let events_path = agent.join("events.jsonl");
        let trajectory_path = agent.join("trajectory.json");
        let stderr_path = agent.join("stderr.log");
        let config_path = root.join("config.json");
        let manifest_path = root.join("artifacts/manifest.json");
        let trial_log_path = root.join("trial.log");
        let lock_path = root.join("lock.json");
        let result_path = root.join("result.json");
        let task_path = task.root().to_path_buf();
        let task_checksum = directory_hash(task.root())?;
        let task_digest = task.content_digest();
        let config = HarborTrialConfig {
            task: HarborTaskConfig {
                path: task_path.clone(),
                source: Some("nanocodex/local".to_owned()),
            },
            trial_name: &result.trial_name,
            trials_dir: &self.root,
            agent: harbor_agent_config(&result.agent.model, &result.agent.effort),
            environment: HarborEnvironmentConfig::from(result.environment),
            verifier: HarborVerifierConfig::native(),
            artifacts: Vec::new(),
            extra_instruction_paths: Vec::new(),
            job_id: self.job_id,
        };
        Self::write_json(&config_path, &config)?;
        Self::write_json(&trajectory_path, trajectory)?;
        Self::write_json(&manifest_path, &Vec::<HarborArtifactManifestEntry>::new())?;

        let trial_uri = Url::from_directory_path(root)
            .map_err(|()| HarborError::InvalidTrialPath(root.clone()))?
            .to_string();
        let trial_result = HarborTrialResult {
            id: result.attempt_id,
            task_name: &result.task_name,
            trial_name: &result.trial_name,
            trial_uri,
            task_id: HarborTaskId { path: task_path },
            source: "nanocodex/local",
            task_checksum: task_checksum.clone(),
            config,
            agent_info: HarborAgentInfo {
                name: "nanocodex",
                version: env!("CARGO_PKG_VERSION"),
                model_info: HarborModelInfo {
                    name: &result.agent.model,
                    provider: "openai",
                },
            },
            agent_result: Some(HarborAgentResult {
                n_input_tokens: result.agent.usage.input_tokens,
                n_cache_tokens: result.agent.usage.cached_input_tokens,
                n_output_tokens: result.agent.usage.output_tokens,
                cost_usd: result.agent.cost_usd,
                rollout_details: None,
                metadata: &result.agent.metadata,
            }),
            verifier_result: Some(HarborVerifierResult {
                rewards: &result.verifier.rewards,
            }),
            started_at: result.timing.started_at,
            finished_at: result.timing.finished_at,
            queue_wait: Some(&result.timing.queue_wait),
            environment_setup: Some(&result.timing.environment_setup),
            environment_readiness: Some(&result.timing.environment_readiness),
            agent_setup: Some(&result.timing.agent_setup),
            agent_execution: Some(&result.timing.agent_execution),
            verifier: Some(&result.timing.verifier),
            exception_info: None,
            step_results: None,
        };
        Self::write_file(&trial_log_path, [])?;
        Self::write_file(&stderr_path, [])?;

        let lock = HarborTrialLock::new(
            task,
            &result.agent.model,
            &result.agent.effort,
            &task_checksum,
            task_digest,
            result.environment,
        );
        Self::write_json(&lock_path, &lock)?;
        Self::write_terminal_json(
            &result_path,
            &trial_result,
            &[
                input_path.as_path(),
                events_path.as_path(),
                config_path.as_path(),
                trajectory_path.as_path(),
                manifest_path.as_path(),
                trial_log_path.as_path(),
                stderr_path.as_path(),
                result.artifacts.verifier_output.as_path(),
                lock_path.as_path(),
            ],
            &self.root,
        )?;
        {
            let mut recorded = self
                .recorded_trials
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorded.push(HarborRecordedTrial {
                task: HarborTaskConfig {
                    path: task.root().to_path_buf(),
                    source: Some("nanocodex/local".to_owned()),
                },
                agent: harbor_agent_config(&result.agent.model, &result.agent.effort),
                lock,
            });
        }
        self.write_job_metadata()
    }

    fn write_failure(
        &self,
        failure: &EvalFailure,
        trajectory: &AtifTrajectory,
    ) -> Result<(), HarborError> {
        let task = failure.task();
        task.validate_package()?;
        let root = &failure.artifacts.directory;
        let agent = root.join("agent");
        let input_path = agent.join("input.jsonl");
        let events_path = agent.join("events.jsonl");
        let trajectory_path = agent.join("trajectory.json");
        let stderr_path = agent.join("stderr.log");
        let config_path = root.join("config.json");
        let manifest_path = root.join("artifacts/manifest.json");
        let trial_log_path = root.join("trial.log");
        let lock_path = root.join("lock.json");
        let result_path = root.join("result.json");
        let task_path = task.root().to_path_buf();
        let task_checksum = directory_hash(task.root())?;
        let task_digest = task.content_digest();
        let model = trajectory.agent.model_name.as_str();
        let effort = trajectory
            .steps
            .iter()
            .find_map(|step| step.reasoning_effort.as_deref())
            .unwrap_or(&failure.effort);
        let config = HarborTrialConfig {
            task: HarborTaskConfig {
                path: task_path.clone(),
                source: Some("nanocodex/local".to_owned()),
            },
            trial_name: &failure.trial_name,
            trials_dir: &self.root,
            agent: harbor_agent_config(model, effort),
            environment: HarborEnvironmentConfig::from(failure.environment),
            verifier: HarborVerifierConfig::native(),
            artifacts: Vec::new(),
            extra_instruction_paths: Vec::new(),
            job_id: self.job_id,
        };
        Self::write_json(&config_path, &config)?;
        Self::write_json(&trajectory_path, trajectory)?;
        Self::write_json(&manifest_path, &Vec::<HarborArtifactManifestEntry>::new())?;

        let trial_uri = Url::from_directory_path(root)
            .map_err(|()| HarborError::InvalidTrialPath(root.clone()))?
            .to_string();
        let trial_result = HarborTrialResult {
            id: failure.attempt_id,
            task_name: &failure.task_name,
            trial_name: &failure.trial_name,
            trial_uri,
            task_id: HarborTaskId { path: task_path },
            source: "nanocodex/local",
            task_checksum: task_checksum.clone(),
            config,
            agent_info: HarborAgentInfo {
                name: "nanocodex",
                version: env!("CARGO_PKG_VERSION"),
                model_info: HarborModelInfo {
                    name: model,
                    provider: "openai",
                },
            },
            agent_result: None,
            verifier_result: None,
            started_at: failure.started_at,
            finished_at: failure.occurred_at,
            queue_wait: None,
            environment_setup: None,
            environment_readiness: None,
            agent_setup: None,
            agent_execution: None,
            verifier: None,
            exception_info: Some(HarborExceptionInfo {
                exception_type: failure.kind.harbor_exception_type(),
                exception_message: &failure.message,
                exception_traceback: &failure.traceback,
                occurred_at: failure.occurred_at,
            }),
            step_results: None,
        };
        Self::write_file(&trial_log_path, failure.traceback.as_bytes())?;
        Self::write_file(&stderr_path, failure.traceback.as_bytes())?;

        let lock = HarborTrialLock::new(
            task,
            model,
            effort,
            &task_checksum,
            task_digest,
            failure.environment,
        );
        Self::write_json(&lock_path, &lock)?;
        Self::write_terminal_json(
            &result_path,
            &trial_result,
            &[
                input_path.as_path(),
                events_path.as_path(),
                config_path.as_path(),
                trajectory_path.as_path(),
                manifest_path.as_path(),
                trial_log_path.as_path(),
                stderr_path.as_path(),
                lock_path.as_path(),
            ],
            &self.root,
        )?;
        {
            let mut recorded = self
                .recorded_trials
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            recorded.push(HarborRecordedTrial {
                task: HarborTaskConfig {
                    path: task.root().to_path_buf(),
                    source: Some("nanocodex/local".to_owned()),
                },
                agent: harbor_agent_config(model, effort),
                lock,
            });
        }
        self.write_job_metadata()
    }

    fn durable_result_paths(
        root: &Path,
        job_id: Uuid,
        manifest: Option<&RunManifest>,
    ) -> Result<Vec<DurableResultPath>, HarborError> {
        if let Some(manifest) = manifest {
            return scan_manifest_trials(root, job_id, manifest)
                .map(|trials| {
                    trials
                        .into_iter()
                        .map(|trial| DurableResultPath {
                            directory: trial.directory().to_path_buf(),
                            result_path: trial.result_path().to_path_buf(),
                            coordinate: Some(trial.coordinate().clone()),
                        })
                        .collect()
                })
                .map_err(|error| HarborError::InvalidDurableTrial(error.to_string()));
        }

        let mut trials = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                let result_path = entry.path().join("result.json");
                if result_path.is_file() {
                    trials.push(DurableResultPath {
                        directory: entry.path(),
                        result_path,
                        coordinate: None,
                    });
                }
            }
        }
        trials.sort_unstable_by(|left, right| left.result_path.cmp(&right.result_path));
        Ok(trials)
    }

    fn durable_trials(
        root: &Path,
        job_id: Uuid,
        manifest: Option<&RunManifest>,
    ) -> Result<Vec<DurableHarborTrial>, HarborError> {
        Self::durable_result_paths(root, job_id, manifest)?
            .into_iter()
            .map(|trial| {
                let result = serde_json::from_slice(&fs::read(&trial.result_path)?)?;
                Ok(DurableHarborTrial {
                    directory: trial.directory,
                    result,
                    coordinate: trial.coordinate,
                })
            })
            .collect()
    }

    fn write_job(&self, n_total_trials: usize) -> Result<(), HarborError> {
        let now = Utc::now();
        let trials = Self::durable_trials(&self.root, self.job_id, self.manifest.as_ref())?;
        let mut stats = HarborJobStats::from_trials(&trials);
        for (eval_key, pass_at_k) in compute_harbor_pass_at_k(&trials) {
            stats.evals.entry(eval_key).or_default().pass_at_k = pass_at_k;
        }
        let baseline_total = if self.manifest.is_some() {
            0
        } else {
            self.baseline
                .as_ref()
                .map_or(0, |baseline| baseline.n_total_trials)
        };
        let n_total_trials = n_total_trials
            .max(self.planned_attempts.unwrap_or(0))
            .max(baseline_total)
            .max(stats.n_completed_trials);
        stats.n_pending_trials = n_total_trials.saturating_sub(stats.n_completed_trials);
        let job = HarborJobResult {
            id: self.job_id,
            started_at: self
                .baseline
                .as_ref()
                .map_or(self.started_at, |baseline| baseline.started_at),
            updated_at: now,
            finished_at: (n_total_trials > 0 && stats.n_completed_trials == n_total_trials)
                .then_some(now),
            n_total_trials,
            stats,
        };
        Self::write_json(&self.root.join("result.json"), &job)
    }

    fn write_job_metadata(&self) -> Result<(), HarborError> {
        let recorded = self
            .recorded_trials
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut tasks = Vec::new();
        let mut agents = Vec::new();
        for trial in recorded.iter() {
            if !tasks
                .iter()
                .any(|task: &HarborTaskConfig| task.path == trial.task.path)
            {
                tasks.push(trial.task.clone());
            }
            if !agents.iter().any(|agent: &HarborAgentConfig| {
                agent.name == trial.agent.name && agent.model_name == trial.agent.model_name
            }) {
                agents.push(trial.agent.clone());
            }
        }
        Self::write_json(
            &self.root.join("config.json"),
            &HarborJobConfig {
                job_name: self.job_id.to_string(),
                jobs_dir: self.jobs_dir.clone(),
                n_concurrent_trials: self.max_concurrency,
                quiet: true,
                environment: HarborEnvironmentConfig::from(self.environment),
                verifier: HarborVerifierConfig::native(),
                agents,
                tasks,
            },
        )?;
        Self::write_json(
            &self.root.join("lock.json"),
            &HarborJobLock {
                schema_version: 2,
                created_at: self.started_at,
                harbor: HarborLockInfo {},
                n_concurrent_trials: self.max_concurrency,
                retry: HarborRetryConfig::default(),
                trials: recorded.iter().map(|trial| trial.lock.clone()).collect(),
            },
        )
    }

    fn write_json(path: &Path, value: &impl Serialize) -> Result<(), HarborError> {
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        Self::atomic_write(path, bytes)
    }

    fn write_terminal_json(
        path: &Path,
        value: &impl Serialize,
        prerequisites: &[&Path],
        job_root: &Path,
    ) -> Result<(), HarborError> {
        Self::write_terminal_json_with_sync(path, value, prerequisites, job_root, sync_directory)
    }

    fn write_terminal_json_with_sync<F>(
        path: &Path,
        value: &impl Serialize,
        prerequisites: &[&Path],
        job_root: &Path,
        mut sync_directory: F,
    ) -> Result<(), HarborError>
    where
        F: FnMut(&Path) -> std::io::Result<()>,
    {
        for prerequisite in prerequisites {
            Self::sync_terminal_prerequisite_with(prerequisite, &mut sync_directory)?;
        }
        let mut bytes = serde_json::to_vec_pretty(value)?;
        bytes.push(b'\n');
        Self::atomic_write_with_sync(path, bytes, &mut sync_directory)?;
        sync_directory(job_root)?;
        if let Some(output_root) = job_root.parent() {
            sync_directory(output_root)?;
        }
        Ok(())
    }

    fn sync_terminal_prerequisite_with<F>(
        path: &Path,
        sync_directory: &mut F,
    ) -> Result<(), HarborError>
    where
        F: FnMut(&Path) -> std::io::Result<()>,
    {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(HarborError::MissingTerminalPrerequisite(path.to_path_buf()));
            }
            Err(error) => return Err(error.into()),
        };
        if !file.metadata()?.is_file() {
            return Err(HarborError::MissingTerminalPrerequisite(path.to_path_buf()));
        }
        file.sync_all()?;
        if let Some(parent) = path.parent() {
            sync_directory(parent)?;
        }
        Ok(())
    }

    fn read_json_if_exists<T>(path: &Path) -> Result<Option<T>, HarborError>
    where
        T: for<'de> Deserialize<'de>,
    {
        match fs::read(path) {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn atomic_write(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), HarborError> {
        let mut sync = sync_directory;
        Self::atomic_write_with_sync(path, bytes, &mut sync)
    }

    fn atomic_write_with_sync<F>(
        path: &Path,
        bytes: impl AsRef<[u8]>,
        sync_directory: &mut F,
    ) -> Result<(), HarborError>
    where
        F: FnMut(&Path) -> std::io::Result<()>,
    {
        require_durable_directory_sync()?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(bytes.as_ref())?;
        temporary.as_file().sync_all()?;
        temporary.persist(path).map_err(|error| error.error)?;
        sync_directory(parent)?;
        Ok(())
    }

    fn write_file(path: &Path, bytes: impl AsRef<[u8]>) -> Result<(), HarborError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, bytes)?;
        Ok(())
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!(
            "durable Harbor artifact commits require directory fsync support: {}",
            path.display()
        ),
    ))
}

#[cfg(unix)]
const fn require_durable_directory_sync() -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(unix))]
fn require_durable_directory_sync() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "durable Harbor artifact commits require directory fsync support",
    ))
}

fn harbor_agent_config(model: &str, effort: &str) -> HarborAgentConfig {
    HarborAgentConfig {
        name: "nanocodex".to_owned(),
        model_name: format!("openai/{model}"),
        kwargs: HarborAgentKwargs {
            effort: effort.to_owned(),
        },
    }
}

fn harbor_float_key(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.1}")
    } else {
        value.to_string()
    }
}

#[derive(Serialize)]
struct HarborInput<'a> {
    protocol_version: u32,
    request_id: Option<String>,
    #[serde(rename = "type")]
    kind: &'static str,
    payload: HarborInputPayload<'a>,
}

#[derive(Serialize)]
struct HarborInputPayload<'a> {
    instruction: &'a str,
}

struct HarborRecordedTrial {
    task: HarborTaskConfig,
    agent: HarborAgentConfig,
    lock: HarborTrialLock,
}

#[derive(Serialize)]
struct HarborJobConfig {
    job_name: String,
    jobs_dir: PathBuf,
    n_concurrent_trials: usize,
    quiet: bool,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
    agents: Vec<HarborAgentConfig>,
    tasks: Vec<HarborTaskConfig>,
}

#[derive(Deserialize, Serialize)]
struct HarborJobLock {
    schema_version: u32,
    created_at: DateTime<Utc>,
    harbor: HarborLockInfo,
    n_concurrent_trials: usize,
    retry: HarborRetryConfig,
    trials: Vec<HarborTrialLock>,
}

#[derive(Deserialize, Serialize)]
struct HarborLockInfo {}

#[derive(Deserialize, Serialize)]
struct HarborRetryConfig {
    max_retries: u32,
    exclude_exceptions: Vec<String>,
    wait_multiplier: f64,
    min_wait_sec: f64,
    max_wait_sec: f64,
}

impl Default for HarborRetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 0,
            exclude_exceptions: [
                "AgentTimeoutError",
                "VerifierTimeoutError",
                "RewardFileNotFoundError",
                "RewardFileEmptyError",
                "VerifierOutputParseError",
                "ApiUsageLimitError",
                "AgentSafetyRefusalError",
                "AgentAuthenticationError",
                "ModelNotFoundError",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            wait_multiplier: 1.0,
            min_wait_sec: 1.0,
            max_wait_sec: 60.0,
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborTrialLock {
    schema_version: u32,
    task: HarborTaskLock,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nanocodex: Option<NanocodexTrialLock>,
    install_only: bool,
    timeout_multiplier: f64,
    agent: HarborAgentConfig,
    skills: Vec<HarborAgentSkillLock>,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
}

impl HarborTrialLock {
    fn new(
        task: &Task,
        model: &str,
        effort: &str,
        harbor_digest: &str,
        materialization_digest: &str,
        environment: EvalEnvironment,
    ) -> Self {
        Self {
            schema_version: 1,
            task: HarborTaskLock {
                name: task
                    .name()
                    .rsplit('/')
                    .next()
                    .unwrap_or(task.name())
                    .to_owned(),
                kind: HarborTaskLockKind::Local,
                digest: format!("sha256:{harbor_digest}"),
                source: Some("nanocodex/local".to_owned()),
                path: task.root().to_path_buf(),
            },
            nanocodex: Some(NanocodexTrialLock {
                materialization_digest: format!("sha256:{materialization_digest}"),
            }),
            install_only: false,
            timeout_multiplier: 1.0,
            agent: HarborAgentConfig {
                name: "nanocodex".to_owned(),
                model_name: format!("openai/{model}"),
                kwargs: HarborAgentKwargs {
                    effort: effort.to_owned(),
                },
            },
            skills: Vec::new(),
            environment: HarborEnvironmentConfig::from(environment),
            verifier: HarborVerifierConfig::native(),
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
struct NanocodexTrialLock {
    materialization_digest: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborTaskLock {
    name: String,
    #[serde(rename = "type")]
    kind: HarborTaskLockKind,
    digest: String,
    source: Option<String>,
    path: PathBuf,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum HarborTaskLockKind {
    Local,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborAgentSkillLock {}

#[derive(Serialize)]
struct HarborTrialConfig<'a> {
    task: HarborTaskConfig,
    trial_name: &'a str,
    trials_dir: &'a Path,
    agent: HarborAgentConfig,
    environment: HarborEnvironmentConfig,
    verifier: HarborVerifierConfig,
    artifacts: Vec<String>,
    extra_instruction_paths: Vec<PathBuf>,
    job_id: Uuid,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborTaskConfig {
    path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborAgentConfig {
    name: String,
    model_name: String,
    kwargs: HarborAgentKwargs,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborAgentKwargs {
    effort: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborEnvironmentConfig {
    #[serde(rename = "type")]
    environment_type: Option<HarborEnvironmentType>,
    import_path: String,
    delete: bool,
    cpu_enforcement_policy: ResourceMode,
    memory_enforcement_policy: ResourceMode,
    kwargs: NativeEnvironmentKwargs,
}

impl From<EvalEnvironment> for HarborEnvironmentConfig {
    fn from(environment: EvalEnvironment) -> Self {
        let (import_path, backend) = match environment {
            EvalEnvironment::Native => ("nanocodex_eval.native:NativeEnvironment", "native"),
            EvalEnvironment::MicroVm => ("nanocodex_vm:VmEnvironment", "microvm"),
        };
        Self {
            environment_type: None,
            import_path: import_path.to_owned(),
            delete: false,
            cpu_enforcement_policy: ResourceMode::Ignore,
            memory_enforcement_policy: ResourceMode::Ignore,
            kwargs: NativeEnvironmentKwargs {
                backend: backend.to_owned(),
            },
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum HarborEnvironmentType {}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum ResourceMode {
    Ignore,
}

#[derive(Clone, Deserialize, Serialize)]
struct NativeEnvironmentKwargs {
    backend: String,
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborVerifierConfig {
    import_path: String,
}

impl HarborVerifierConfig {
    fn native() -> Self {
        Self {
            import_path: "nanocodex_eval.native:Verifier".to_owned(),
        }
    }
}

#[derive(Serialize)]
struct HarborTrialResult<'a> {
    id: Uuid,
    task_name: &'a str,
    trial_name: &'a str,
    trial_uri: String,
    task_id: HarborTaskId,
    source: &'static str,
    task_checksum: String,
    config: HarborTrialConfig<'a>,
    agent_info: HarborAgentInfo<'a>,
    agent_result: Option<HarborAgentResult<'a>>,
    verifier_result: Option<HarborVerifierResult<'a>>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    queue_wait: Option<&'a PhaseTiming>,
    environment_setup: Option<&'a PhaseTiming>,
    environment_readiness: Option<&'a PhaseTiming>,
    agent_setup: Option<&'a PhaseTiming>,
    agent_execution: Option<&'a PhaseTiming>,
    verifier: Option<&'a PhaseTiming>,
    exception_info: Option<HarborExceptionInfo<'a>>,
    step_results: Option<Vec<HarborStepResult>>,
}

#[derive(Serialize)]
struct HarborExceptionInfo<'a> {
    exception_type: &'a str,
    exception_message: &'a str,
    exception_traceback: &'a str,
    occurred_at: DateTime<Utc>,
}

#[derive(Serialize)]
struct HarborStepResult {}

#[derive(Serialize)]
struct HarborTaskId {
    path: PathBuf,
}

#[derive(Serialize)]
struct HarborAgentInfo<'a> {
    name: &'static str,
    version: &'static str,
    model_info: HarborModelInfo<'a>,
}

#[derive(Serialize)]
struct HarborModelInfo<'a> {
    name: &'a str,
    provider: &'static str,
}

#[derive(Serialize)]
struct HarborAgentResult<'a> {
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    cost_usd: Option<f64>,
    rollout_details: Option<Vec<HarborRolloutDetail>>,
    metadata: &'a AgentMetadata,
}

#[derive(Serialize)]
struct HarborRolloutDetail {}

#[derive(Serialize)]
struct HarborVerifierResult<'a> {
    rewards: &'a BTreeMap<String, f64>,
}

#[derive(Serialize)]
struct HarborArtifactManifestEntry {}

#[derive(Clone, Deserialize, Serialize)]
struct HarborJobResult {
    id: Uuid,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    n_total_trials: usize,
    stats: HarborJobStats,
}

struct DurableHarborTrial {
    directory: PathBuf,
    result: RetainedHarborTrialResult,
    coordinate: Option<RunCoordinate>,
}

struct DurableResultPath {
    directory: PathBuf,
    result_path: PathBuf,
    coordinate: Option<RunCoordinate>,
}

#[derive(Deserialize)]
struct RetainedHarborTrialResult {
    id: Uuid,
    task_name: String,
    trial_name: String,
    source: Option<String>,
    config: RetainedHarborTrialConfig,
    agent_info: RetainedHarborAgentInfo,
    agent_result: Option<RetainedHarborAgentResult>,
    verifier_result: Option<RetainedHarborVerifierResult>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    queue_wait: Option<RetainedPhaseTiming>,
    environment_readiness: Option<RetainedPhaseTiming>,
    agent_setup: Option<RetainedPhaseTiming>,
    agent_execution: Option<RetainedPhaseTiming>,
    verifier: Option<RetainedPhaseTiming>,
    exception_info: Option<RetainedHarborExceptionInfo>,
}

#[derive(Deserialize)]
struct RetainedHarborTrialConfig {
    environment: RetainedHarborEnvironment,
}

#[derive(Deserialize)]
struct RetainedHarborEnvironment {
    kwargs: RetainedHarborEnvironmentKwargs,
}

#[derive(Deserialize)]
struct RetainedHarborEnvironmentKwargs {
    backend: String,
}

#[derive(Deserialize)]
struct RetainedHarborAgentInfo {
    name: String,
    model_info: RetainedHarborModelInfo,
}

#[derive(Deserialize)]
struct RetainedHarborModelInfo {
    name: String,
}

#[derive(Deserialize)]
struct RetainedHarborAgentResult {
    #[serde(default)]
    n_input_tokens: u64,
    #[serde(default)]
    n_cache_tokens: u64,
    #[serde(default)]
    n_output_tokens: u64,
    cost_usd: Option<f64>,
    #[serde(default)]
    metadata: RetainedAgentMetadata,
}

#[derive(Default, Deserialize)]
struct RetainedAgentMetadata {
    #[serde(default)]
    model_duration_ns: u64,
    #[serde(default)]
    tool_work_duration_ns: u64,
    #[serde(default)]
    tool_wall_duration_ns: u64,
}

#[derive(Deserialize)]
struct RetainedHarborVerifierResult {
    rewards: BTreeMap<String, f64>,
}

#[derive(Deserialize)]
struct RetainedHarborExceptionInfo {
    exception_type: String,
}

#[derive(Deserialize)]
struct RetainedPhaseTiming {
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
}

impl DurableHarborTrial {
    fn eval_key(&self) -> String {
        let source = self.result.source.as_deref().unwrap_or("adhoc");
        format!(
            "{}__{}__{source}",
            self.result.agent_info.name, self.result.agent_info.model_info.name
        )
    }

    fn attempt_fact(self, configuration: String, repetition: u16) -> AttemptFact {
        let agent = self.result.agent_result.as_ref();
        let metadata = agent.map(|agent| &agent.metadata);
        let passed = self
            .result
            .verifier_result
            .as_ref()
            .is_some_and(|verifier| verifier.rewards.values().all(|reward| *reward > 0.0))
            && self.result.exception_info.is_none();
        AttemptFact {
            attempt_id: self.result.id,
            task_name: self.result.task_name,
            configuration,
            repetition,
            passed,
            cost_usd: agent.and_then(|agent| agent.cost_usd),
            latency: LatencyBreakdown {
                queue_wait_ns: retained_phase_duration_ns(self.result.queue_wait.as_ref()),
                vm_bootstrap_ns: if self.result.config.environment.kwargs.backend == "microvm" {
                    retained_phase_duration_ns(self.result.environment_readiness.as_ref())
                } else {
                    0
                },
                agent_setup_ns: retained_phase_duration_ns(self.result.agent_setup.as_ref()),
                agent_execution_ns: retained_phase_duration_ns(
                    self.result.agent_execution.as_ref(),
                ),
                model_ns: metadata.map_or(0, |metadata| metadata.model_duration_ns),
                tool_work_ns: metadata.map_or(0, |metadata| metadata.tool_work_duration_ns),
                tool_wall_ns: metadata.map_or(0, |metadata| metadata.tool_wall_duration_ns),
                verifier_ns: retained_phase_duration_ns(self.result.verifier.as_ref()),
                total_ns: retained_duration_ns(self.result.started_at, self.result.finished_at),
                ..LatencyBreakdown::default()
            },
            artifacts: AttemptFactArtifacts {
                trajectory: self.directory.join("agent/trajectory.json"),
                verifier_output: self.directory.join("verifier/test-stdout.txt"),
                directory: self.directory,
            },
        }
    }
}

fn fallback_repetition(trial_name: &str) -> u16 {
    trial_name
        .rsplit("__")
        .nth(1)
        .and_then(|trial| trial.parse().ok())
        .unwrap_or(1)
}

fn retained_phase_duration_ns(timing: Option<&RetainedPhaseTiming>) -> u64 {
    timing.map_or(0, |timing| {
        retained_duration_ns(timing.started_at, timing.finished_at)
    })
}

fn retained_duration_ns(started_at: DateTime<Utc>, finished_at: DateTime<Utc>) -> u64 {
    u64::try_from(
        finished_at
            .signed_duration_since(started_at)
            .num_nanoseconds()
            .unwrap_or_default()
            .max(0),
    )
    .unwrap_or(u64::MAX)
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct HarborJobStats {
    n_completed_trials: usize,
    n_errored_trials: usize,
    n_running_trials: usize,
    n_pending_trials: usize,
    n_cancelled_trials: usize,
    n_retries: usize,
    evals: BTreeMap<String, HarborAgentDatasetStats>,
    n_input_tokens: u64,
    n_cache_tokens: u64,
    n_output_tokens: u64,
    cost_usd: Option<f64>,
}

impl HarborJobStats {
    fn from_trials(trials: &[DurableHarborTrial]) -> Self {
        let mut stats = Self {
            n_completed_trials: trials.len(),
            ..Self::default()
        };
        for trial in trials {
            if let Some(agent) = &trial.result.agent_result {
                stats.n_input_tokens = stats.n_input_tokens.saturating_add(agent.n_input_tokens);
                stats.n_cache_tokens = stats.n_cache_tokens.saturating_add(agent.n_cache_tokens);
                stats.n_output_tokens = stats.n_output_tokens.saturating_add(agent.n_output_tokens);
                if let Some(cost) = agent.cost_usd {
                    stats.cost_usd = Some(stats.cost_usd.unwrap_or_default() + cost);
                }
            }

            let eval = stats.evals.entry(trial.eval_key()).or_default();
            if let Some(exception) = &trial.result.exception_info {
                stats.n_errored_trials = stats.n_errored_trials.saturating_add(1);
                eval.n_errors = eval.n_errors.saturating_add(1);
                eval.exception_stats
                    .entry(exception.exception_type.clone())
                    .or_default()
                    .push(trial.result.trial_name.clone());
            } else {
                eval.n_trials = eval.n_trials.saturating_add(1);
                if let Some(verifier) = &trial.result.verifier_result {
                    for (name, reward) in &verifier.rewards {
                        eval.reward_stats
                            .entry(name.clone())
                            .or_default()
                            .entry(harbor_float_key(*reward))
                            .or_default()
                            .push(trial.result.trial_name.clone());
                    }
                }
            }
        }
        stats
    }
}

#[derive(Clone, Default, Deserialize, Serialize)]
struct HarborAgentDatasetStats {
    n_trials: usize,
    n_errors: usize,
    metrics: Vec<HarborMetric>,
    pass_at_k: BTreeMap<usize, f64>,
    reward_stats: BTreeMap<String, BTreeMap<String, Vec<String>>>,
    exception_stats: BTreeMap<String, Vec<String>>,
}

fn compute_harbor_pass_at_k(
    trials: &[DurableHarborTrial],
) -> BTreeMap<String, BTreeMap<usize, f64>> {
    let mut groups = BTreeMap::<String, Option<BTreeMap<String, Vec<u8>>>>::new();
    for trial in trials {
        let success = match &trial.result.verifier_result {
            None => Some(0),
            Some(verifier) if verifier.rewards.len() == 1 => {
                match verifier
                    .rewards
                    .values()
                    .next()
                    .map(|reward| reward.to_bits())
                {
                    Some(bits) if bits == 0.0_f64.to_bits() => Some(0),
                    Some(bits) if bits == 1.0_f64.to_bits() => Some(1),
                    Some(_) | None => None,
                }
            }
            Some(_) => None,
        };
        let group = groups
            .entry(trial.eval_key())
            .or_insert_with(|| Some(BTreeMap::new()));
        match (group.as_mut(), success) {
            (Some(tasks), Some(success)) => {
                tasks
                    .entry(trial.result.task_name.clone())
                    .or_default()
                    .push(success);
            }
            (_, None) => *group = None,
            (None, Some(_)) => {}
        }
    }

    groups
        .into_iter()
        .filter_map(|(eval_key, tasks)| {
            compute_pass_at_k_for_tasks(&tasks?).map(|pass_at_k| (eval_key, pass_at_k))
        })
        .collect()
}

fn compute_pass_at_k_for_tasks(tasks: &BTreeMap<String, Vec<u8>>) -> Option<BTreeMap<usize, f64>> {
    let min_trials = tasks.values().map(Vec::len).min()?;
    let task_count = u32::try_from(tasks.len()).ok()?;
    let mut pass_at_k = BTreeMap::new();
    for k in eligible_k_values(min_trials) {
        let k_u32 = u32::try_from(k).ok()?;
        let mut estimate = 0.0;
        for successes in tasks.values() {
            let n = u32::try_from(successes.len()).ok()?;
            let correct = successes.iter().map(|success| u32::from(*success)).sum();
            estimate += pass_at_k_for_task(n, correct, k_u32);
        }
        pass_at_k.insert(k, estimate / f64::from(task_count));
    }
    Some(pass_at_k)
}

fn eligible_k_values(max_k: usize) -> Vec<usize> {
    let mut values = std::collections::BTreeSet::new();
    let mut k = 2;
    while k <= max_k {
        values.insert(k);
        k *= 2;
    }
    let mut k = 5;
    while k <= max_k {
        values.insert(k);
        k += 5;
    }
    values.into_iter().collect()
}

fn pass_at_k_for_task(n: u32, correct: u32, k: u32) -> f64 {
    if n - correct < k {
        return 1.0;
    }
    let failure_probability = (0..k).fold(1.0, |product, i| {
        product * f64::from(n - correct - i) / f64::from(n - i)
    });
    1.0 - failure_probability
}

#[derive(Clone, Deserialize, Serialize)]
struct HarborMetric {}

#[cfg(all(test, unix))]
mod tests {
    use std::{collections::BTreeMap, fs, path::Path};

    use crate::{AtifTrajectory, EvalEnvironment, Evaluator, Sweep, Task};
    use chrono::Utc;
    use nanocodex_agent::{Nanocodex, OpenAi};
    use serde::Deserialize;
    use serde_json::json;
    use tempfile::tempdir;
    use uuid::Uuid;

    use super::{
        Harbor, HarborArtifacts, HarborError, HarborJob, compute_pass_at_k_for_tasks,
        pass_at_k_for_task,
    };

    #[derive(Deserialize)]
    struct TrialResult {
        exception_info: Option<ExceptionInfo>,
    }

    #[derive(Deserialize)]
    struct ExceptionInfo {
        exception_type: String,
        exception_message: String,
    }

    #[derive(Deserialize)]
    struct JobResult {
        n_total_trials: usize,
        stats: JobStats,
    }

    #[derive(Deserialize)]
    struct JobStats {
        #[serde(rename = "n_completed_trials")]
        completed: usize,
        #[serde(rename = "n_errored_trials")]
        errored: usize,
        #[serde(rename = "n_pending_trials")]
        pending: usize,
    }

    #[test]
    fn terminal_result_is_committed_only_after_artifacts_and_lock() {
        let output = tempdir().unwrap();
        let job = output.path().join("job");
        let trial = job.join("trial");
        let artifact = trial.join("agent/trajectory.json");
        let lock = trial.join("lock.json");
        let result = trial.join("result.json");
        HarborArtifacts::write_file(&artifact, b"{}\n").unwrap();

        let error = HarborArtifacts::write_terminal_json(
            &result,
            &json!({"status": "completed"}),
            &[artifact.as_path(), lock.as_path()],
            &job,
        )
        .unwrap_err();

        assert!(matches!(error, HarborError::MissingTerminalPrerequisite(path) if path == lock));
        assert!(!result.exists());

        HarborArtifacts::write_file(&lock, b"{}\n").unwrap();
        let mut directory_syncs = Vec::new();
        HarborArtifacts::write_terminal_json_with_sync(
            &result,
            &json!({"status": "completed"}),
            &[artifact.as_path(), lock.as_path()],
            &job,
            |directory| {
                directory_syncs.push((directory.to_path_buf(), result.exists()));
                Ok(())
            },
        )
        .unwrap();

        let retained: serde_json::Value =
            serde_json::from_slice(&fs::read(result).unwrap()).unwrap();
        assert_eq!(retained["status"], "completed");
        let terminal_trial_sync = directory_syncs
            .iter()
            .position(|(directory, result_exists)| directory == &trial && *result_exists)
            .unwrap();
        let terminal_job_sync = directory_syncs
            .iter()
            .position(|(directory, result_exists)| directory == &job && *result_exists)
            .unwrap();
        assert_eq!(
            directory_syncs.last(),
            Some(&(output.path().to_path_buf(), true))
        );
        assert!(terminal_trial_sync < terminal_job_sync);
        assert!(terminal_job_sync < directory_syncs.len() - 1);
    }

    #[test]
    fn atomic_write_replaces_target_without_leaving_temporary_files() {
        let output = tempdir().unwrap();
        let target = output.path().join("result.json");

        HarborArtifacts::atomic_write(&target, b"first\n").unwrap();
        HarborArtifacts::atomic_write(&target, b"second\n").unwrap();

        assert_eq!(fs::read(&target).unwrap(), b"second\n");
        let entries = fs::read_dir(output.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(entries, [target]);
    }

    #[test]
    fn attach_rebuilds_stale_job_stats_from_durable_trials() {
        let output = tempdir().unwrap();
        let task = write_greeting_task();
        let sweep = Sweep::builder()
            .task(task.clone())
            .trials(2)
            .agent(
                "default",
                Nanocodex::builder(OpenAi::new("test-key").unwrap()),
            )
            .unwrap()
            .build()
            .unwrap();
        let (eval, _) = Evaluator::builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
            .output_directory(output.path())
            .fresh_run(&sweep)
            .build()
            .unwrap();

        Harbor::new(&eval).unwrap();
        let stale: JobResult =
            serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(stale.stats.completed, 0);
        write_retained_trial(eval.directory(), eval.id(), &task, "default", 1, Some(1.0));

        Harbor::new(&eval).unwrap();
        let rebuilt: serde_json::Value =
            serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(rebuilt["n_total_trials"], 2);
        assert_eq!(rebuilt["stats"]["n_completed_trials"], 1);
        assert_eq!(rebuilt["stats"]["n_pending_trials"], 1);
        assert_eq!(rebuilt["stats"]["n_input_tokens"], 10);
        assert_eq!(rebuilt["stats"]["n_cache_tokens"], 4);
        assert_eq!(rebuilt["stats"]["n_output_tokens"], 3);
        assert_eq!(rebuilt["stats"]["cost_usd"], 0.25);
        assert_eq!(
            rebuilt["stats"]["evals"]["nanocodex__gpt-test__nanocodex/local"]["n_trials"],
            1
        );
        assert_eq!(
            rebuilt["stats"]["evals"]["nanocodex__gpt-test__nanocodex/local"]["reward_stats"]
                ["reward"]["1.0"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let rebuilt_lock: serde_json::Value =
            serde_json::from_slice(&fs::read(eval.directory().join("lock.json")).unwrap()).unwrap();
        assert_eq!(rebuilt_lock["trials"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn aggregate_reconstructs_every_durable_trial_with_sweep_provenance() {
        let output = tempdir().unwrap();
        let task = write_greeting_task();
        let sweep = Sweep::builder()
            .task(task.clone())
            .trials(2)
            .agent(
                "recipe__variant",
                Nanocodex::builder(OpenAi::new("test-key").unwrap()),
            )
            .unwrap()
            .build()
            .unwrap();
        let (eval, _) = Evaluator::builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
            .output_directory(output.path())
            .fresh_run(&sweep)
            .build()
            .unwrap();
        let first = write_retained_trial(
            eval.directory(),
            eval.id(),
            &task,
            "recipe__variant",
            1,
            Some(1.0),
        );
        let second = write_retained_trial(
            eval.directory(),
            eval.id(),
            &task,
            "recipe__variant",
            2,
            None,
        );
        let job = HarborJob {
            id: eval.id(),
            directory: eval.directory().to_path_buf(),
        };

        let aggregate = job.aggregate_dataset().unwrap();

        assert_eq!(aggregate.attempts.len(), 2);
        assert_eq!(aggregate.attempts[0].attempt_id, first);
        assert_eq!(aggregate.attempts[0].configuration, "recipe__variant");
        assert_eq!(aggregate.attempts[0].repetition, 1);
        assert!(aggregate.attempts[0].passed);
        assert_eq!(aggregate.attempts[0].cost_usd, Some(0.25));
        assert_eq!(aggregate.attempts[1].attempt_id, second);
        assert_eq!(aggregate.attempts[1].configuration, "recipe__variant");
        assert_eq!(aggregate.attempts[1].repetition, 2);
        assert!(!aggregate.attempts[1].passed);
        assert_eq!(aggregate.attempts[1].cost_usd, None);
        assert_eq!(aggregate.configurations.len(), 1);
        assert_eq!(aggregate.configurations[0].success.samples, 2);
        assert_eq!(aggregate.configurations[0].success.successes, 1);
    }

    #[test]
    fn pass_at_k_matches_harbors_unbiased_estimator() {
        assert!((pass_at_k_for_task(5, 2, 2) - 0.7).abs() < f64::EPSILON);

        let tasks = BTreeMap::from([
            ("sometimes".to_owned(), vec![1, 0, 0, 0, 0]),
            ("always".to_owned(), vec![1, 1, 1, 1, 1]),
        ]);
        let pass_at_k = compute_pass_at_k_for_tasks(&tasks).unwrap();

        assert_eq!(pass_at_k.keys().copied().collect::<Vec<_>>(), [2, 4, 5]);
        assert!((pass_at_k[&2] - 0.7).abs() < f64::EPSILON);
        assert!((pass_at_k[&4] - 0.9).abs() < f64::EPSILON);
        assert!((pass_at_k[&5] - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn trial_lock_keeps_harbors_hash_separate_from_internal_materialization_identity() {
        let task = write_greeting_task();
        let harbor_digest = super::directory_hash(task.root()).unwrap();
        let lock = super::HarborTrialLock::new(
            &task,
            "gpt-test",
            "high",
            &harbor_digest,
            task.content_digest(),
            EvalEnvironment::Native,
        );

        let mut retained = serde_json::to_value(&lock).unwrap();
        assert_eq!(
            retained["task"]["digest"],
            format!("sha256:{harbor_digest}")
        );
        assert_eq!(
            retained["nanocodex"]["materialization_digest"],
            format!("sha256:{}", task.content_digest())
        );
        assert_ne!(
            retained["task"]["digest"],
            retained["nanocodex"]["materialization_digest"]
        );
        serde_json::from_value::<super::HarborTrialLock>(retained.clone()).unwrap();

        retained.as_object_mut().unwrap().remove("nanocodex");
        let legacy = serde_json::from_value::<super::HarborTrialLock>(retained).unwrap();
        assert!(legacy.nanocodex.is_none());
    }

    #[test]
    fn finite_job_records_pending_trials_before_execution() {
        let output = tempdir().unwrap();
        let task = Task::load(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../tasks/write-greeting"),
        )
        .unwrap();
        let sweep = Sweep::builder()
            .task(task)
            .trials(2)
            .agent(
                "default",
                Nanocodex::builder(OpenAi::new("test-key").unwrap()),
            )
            .unwrap()
            .build()
            .unwrap();
        let (eval, _) = Evaluator::builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
            .output_directory(output.path())
            .fresh_run(&sweep)
            .build()
            .unwrap();

        Harbor::new(&eval).unwrap();
        let result: JobResult =
            serde_json::from_slice(&fs::read(eval.directory().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(result.n_total_trials, 2);
        assert_eq!(result.stats.completed, 0);
        assert_eq!(result.stats.pending, 2);
    }

    #[test]
    fn job_config_records_microvm_backend_before_execution() {
        let output = tempdir().unwrap();
        let (eval, _) = Evaluator::builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
            .output_directory(output.path())
            .attempt_environment(EvalEnvironment::MicroVm)
            .build()
            .unwrap();

        Harbor::new(&eval).unwrap();
        let config: serde_json::Value =
            serde_json::from_slice(&fs::read(eval.directory().join("config.json")).unwrap())
                .unwrap();
        assert_eq!(
            config["environment"]["import_path"],
            "nanocodex_vm:VmEnvironment"
        );
        assert_eq!(config["environment"]["kwargs"]["backend"], "microvm");
    }

    #[tokio::test]
    async fn records_an_errored_attempt_as_a_harbor_trial() {
        let task_root = tempdir().unwrap();
        fs::create_dir(task_root.path().join("tests")).unwrap();
        fs::create_dir(task_root.path().join("environment")).unwrap();
        fs::write(
            task_root.path().join("task.toml"),
            r#"
schema_version = "1.1"
[task]
name = "terminal-bench/errored"
description = "Errored Harbor fixture"
[metadata]
custom_docker_compose = true
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "example/errored:latest"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
        )
        .unwrap();
        fs::write(task_root.path().join("instruction.md"), "do the work\n").unwrap();
        fs::write(task_root.path().join("tests/test.sh"), "exit 0\n").unwrap();
        let task = Task::load(task_root.path()).unwrap();
        let output = tempdir().unwrap();
        let (eval, events) = Evaluator::builder(Nanocodex::builder(OpenAi::new("test").unwrap()))
            .output_directory(output.path())
            .build()
            .unwrap();
        let recorder = Harbor::new(&eval)
            .unwrap()
            .record(events.subscribe())
            .unwrap();

        assert!(eval.task(task).await.is_err());
        let job = recorder.finish_all(1).await.unwrap();
        let trial = fs::read_dir(job.directory())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .unwrap()
            .path();
        let result: TrialResult =
            serde_json::from_slice(&fs::read(trial.join("result.json")).unwrap()).unwrap();
        let exception = result.exception_info.unwrap();
        assert_eq!(exception.exception_type, "EnvironmentError");
        assert!(
            exception
                .exception_message
                .contains("custom Docker Compose")
        );
        serde_json::from_slice::<AtifTrajectory>(
            &fs::read(trial.join("agent/trajectory.json")).unwrap(),
        )
        .unwrap();

        let result: JobResult =
            serde_json::from_slice(&fs::read(job.directory().join("result.json")).unwrap())
                .unwrap();
        assert_eq!(result.n_total_trials, 1);
        assert_eq!(result.stats.completed, 1);
        assert_eq!(result.stats.errored, 1);
        assert_eq!(result.stats.pending, 0);
    }

    fn write_greeting_task() -> Task {
        Task::load(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../../tasks/write-greeting"),
        )
        .unwrap()
    }

    fn write_retained_trial(
        job: &Path,
        job_id: Uuid,
        task: &Task,
        configuration: &str,
        repetition: u16,
        reward: Option<f64>,
    ) -> Uuid {
        let id = Uuid::now_v7();
        let compact_id = id.simple().to_string();
        let trial_name = format!(
            "write-greeting__{configuration}__{repetition:03}__{}",
            &compact_id[..8]
        );
        let directory = job.join(&trial_name);
        fs::create_dir_all(directory.join("agent")).unwrap();
        fs::create_dir_all(directory.join("verifier")).unwrap();
        fs::write(directory.join("agent/trajectory.json"), "{}\n").unwrap();
        fs::write(directory.join("verifier/test-stdout.txt"), "fixture\n").unwrap();

        let started_at = Utc::now();
        let finished_at = started_at + chrono::Duration::milliseconds(10);
        let phase = json!({
            "started_at": started_at,
            "finished_at": finished_at,
        });
        let agent_result = reward.map(|_| {
            json!({
                "n_input_tokens": 10,
                "n_cache_tokens": 4,
                "n_output_tokens": 3,
                "cost_usd": 0.25,
                "metadata": {
                    "model_duration_ns": 5,
                    "tool_work_duration_ns": 6,
                    "tool_wall_duration_ns": 7,
                },
            })
        });
        let verifier_result = reward.map(|reward| {
            json!({
                "rewards": {
                    "reward": reward,
                },
            })
        });
        let exception_info = reward.is_none().then(|| {
            json!({
                "exception_type": "AgentError",
            })
        });
        let timing = reward.map(|_| phase);
        let result = json!({
            "id": id,
            "task_name": "nanoeval/write-greeting",
            "trial_name": trial_name,
            "task_id": {
                "path": task.root(),
            },
            "source": "nanocodex/local",
            "config": {
                "task": {
                    "path": task.root(),
                },
                "trial_name": trial_name,
                "trials_dir": job,
                "job_id": job_id,
                "environment": {
                    "kwargs": {
                        "backend": "native",
                    },
                },
            },
            "agent_info": {
                "name": "nanocodex",
                "model_info": {
                    "name": "gpt-test",
                },
            },
            "agent_result": agent_result,
            "verifier_result": verifier_result,
            "started_at": started_at,
            "finished_at": finished_at,
            "queue_wait": timing,
            "environment_readiness": timing,
            "agent_setup": timing,
            "agent_execution": timing,
            "verifier": timing,
            "exception_info": exception_info,
        });
        super::HarborArtifacts::write_json(
            &directory.join("lock.json"),
            &super::HarborTrialLock::new(
                task,
                "gpt-test",
                "high",
                &super::directory_hash(task.root()).unwrap(),
                task.content_digest(),
                EvalEnvironment::Native,
            ),
        )
        .unwrap();
        fs::write(
            directory.join("result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        id
    }
}

#[cfg(all(test, not(unix)))]
mod unsupported_platform_tests {
    use tempfile::tempdir;

    use super::{HarborArtifacts, HarborError};

    #[test]
    fn atomic_artifacts_fail_closed_without_durable_directory_sync() {
        let output = tempdir().unwrap();
        let target = output.path().join("result.json");

        let error = HarborArtifacts::atomic_write(&target, b"terminal\n").unwrap_err();

        assert!(
            matches!(error, HarborError::Io(error) if error.kind() == std::io::ErrorKind::Unsupported)
        );
        assert!(!target.exists());
    }
}
