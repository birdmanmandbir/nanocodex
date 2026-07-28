use std::{
    error::Error,
    ffi::OsString,
    fmt, fs,
    future::Future,
    io,
    num::ParseFloatError,
    path::{Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use futures_util::{StreamExt, stream};
use nanocodex_agent::{
    Nanocodex, NanocodexBuilder, NanocodexError,
    events::{AgentEvent, AgentEventKind, AgentEvents},
    session::SessionId,
    transport::ResponsesError,
};
use nanocodex_oai_api::MODEL;
use serde::Deserialize;
use tokio::{
    sync::{Notify, broadcast},
    time::timeout,
};
use tracing::{Instrument, Span, info, info_span};
use uuid::Uuid;

use crate::{
    AgentId, AgentMetadata, AgentResult, BillingCompleteness, CleanupPhase, EvalArtifacts,
    EvalAttemptOutcome, EvalCleanup, EvalEnvironment, EvalEvent, EvalEventKind, EvalEvents,
    EvalFailure, EvalFailureKind, EvalFailureTiming, EvalOutcome, EvalResult, EvalStatus,
    EvalTiming, PhaseTiming, Sweep, SweepAttemptResult, SweepResults, Task, TaskLoadError,
    VerifierResult,
    job::EvalJob,
    native::{NativeAttempt, VerifierExecution},
};

const EVENT_CAPACITY: usize = 16_384;
// One warmup plus three typical four-call attempts stays below the provider's
// approximate 15-request-per-minute routing guidance for a cache key.
const PROMPT_CACHE_COHORT_SIZE: u64 = 3;

/// A reusable evaluation recipe. Every task call creates an independent agent
/// session and disposable workspace.
#[derive(Clone)]
pub struct Evaluator {
    inner: Arc<EvaluatorInner>,
}

/// Deliberate evaluator policy configured before running tasks.
pub struct EvaluatorBuilder {
    nanocodex: NanocodexBuilder,
    output_directory: PathBuf,
    max_concurrency: usize,
    max_memory_mb: Option<u64>,
    attempt_environment: EvalEnvironment,
    attempt_agent: Option<AttemptAgentFactory>,
    finite_run: Option<FiniteRun>,
}

struct EvaluatorInner {
    nanocodex: NanocodexBuilder,
    job: EvalJob,
    planned_attempts: Option<usize>,
    admission: Arc<AdmissionController>,
    max_concurrency: usize,
    max_memory_mb: Option<u64>,
    attempt_environment: EvalEnvironment,
    next_prompt_cache_attempt: AtomicU64,
    events: broadcast::Sender<Arc<EvalEvent>>,
    attempt_agent: Option<AttemptAgentFactory>,
}

struct AdmissionController {
    max_concurrency: usize,
    max_memory_mb: Option<u64>,
    state: Mutex<AdmissionState>,
    changed: Notify,
}

#[derive(Default)]
struct AdmissionState {
    running: usize,
    memory_mb: u64,
    admitted: usize,
    draining: bool,
}

struct AdmissionPermit {
    controller: Arc<AdmissionController>,
    memory_mb: u64,
}

struct FiniteRun {
    manifest: crate::sweep::RunManifest,
    mode: FiniteRunMode,
}

#[derive(Clone, Copy)]
enum FiniteRunMode {
    Fresh,
    Resume,
}

type AttemptError = Box<dyn Error + Send + Sync + 'static>;
type AttemptAgentFactory = Arc<
    dyn for<'a> Fn(EvalAttempt<'a>, NanocodexBuilder) -> Result<AttemptAgent, AttemptError>
        + Send
        + Sync
        + 'static,
>;

type AttemptVerifierFuture<'a> = Pin<
    Box<dyn Future<Output = Result<AttemptVerification, AttemptVerificationFailure>> + Send + 'a>,
>;
type AttemptVerifierCleanupFuture<'a> = Pin<Box<dyn Future<Output = CleanupPhase> + Send + 'a>>;
type AttemptReadinessFuture =
    Pin<Box<dyn Future<Output = Result<(), AttemptError>> + Send + 'static>>;

/// The Nanocodex configuration and resources owned by one attempt.
pub struct AttemptAgent {
    nanocodex: NanocodexBuilder,
    readiness: Option<AttemptReadinessFuture>,
    verifier: Option<Box<dyn AttemptVerifier>>,
}

/// A verifier that runs against the same retained environment as the agent.
pub trait AttemptVerifier: Send {
    /// Verifies one completed agent attempt.
    ///
    /// The returned future may borrow the verifier, task, and attempt for its
    /// complete execution. Failures are retained as typed evaluation errors.
    fn verify<'a>(
        &'a mut self,
        task: &'a Task,
        attempt: EvalAttempt<'a>,
    ) -> AttemptVerifierFuture<'a>;

    /// Explicitly joins verifier-owned resources when verification will not run.
    ///
    /// Implementations that own processes, VMs, mounts, or other asynchronous
    /// resources must override this method. The evaluator awaits it on every
    /// post-construction abort path.
    fn shutdown(&mut self) -> AttemptVerifierCleanupFuture<'_> {
        Box::pin(async { CleanupPhase::not_required() })
    }
}

/// A verifier's primary semantic error plus independently retained cleanup.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct AttemptVerificationFailure {
    #[source]
    error: AttemptError,
    /// Cleanup health observed after the primary verification failure.
    pub cleanup: CleanupPhase,
}

impl AttemptVerificationFailure {
    /// Retains a verifier error and the cleanup attempted after it.
    pub fn new(error: impl Error + Send + Sync + 'static, cleanup: CleanupPhase) -> Self {
        Self {
            error: Box::new(error),
            cleanup,
        }
    }

    fn into_parts(self) -> (AttemptError, CleanupPhase) {
        (self.error, self.cleanup)
    }
}

/// Complete typed output returned by an attempt-owned verifier.
pub struct AttemptVerification {
    /// Process-equivalent exit status and named rewards.
    pub result: VerifierResult,
    /// Complete captured verifier standard output.
    pub stdout: String,
    /// Complete captured verifier standard error.
    pub stderr: String,
    /// Attempt-owned verifier cleanup health and timing.
    pub cleanup: CleanupPhase,
}

struct AttemptInput {
    task: Task,
    nanocodex: NanocodexBuilder,
    coordinate: Option<SweepCoordinate>,
    queued_at: DateTime<Utc>,
}

struct AttemptOutput {
    outcome: EvalAttemptOutcome,
    coordinate: Option<SweepCoordinate>,
}

#[derive(Clone)]
struct SweepCoordinate {
    agent: AgentId,
    trial: u16,
}

/// Immutable paths and task metadata available while configuring one attempt.
#[derive(Clone, Copy)]
pub struct EvalAttempt<'a> {
    task: &'a Task,
    directory: &'a Path,
    workspace: &'a Path,
}

/// Failure to configure, execute, verify, or durably retain an attempt.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// Configured concurrency was zero.
    #[error("maximum concurrency must be greater than zero")]
    InvalidConcurrency,

    /// Configured aggregate memory was zero.
    #[error("maximum task memory must be greater than zero")]
    InvalidMemory,

    /// The evaluator stopped admitting new attempts while draining.
    #[error("evaluation is draining and no longer admits new attempts")]
    Draining,

    /// A task requires behavior unavailable in the native backend.
    #[error("task {task} cannot run with the native backend: {reason}")]
    UnsupportedNativeTask {
        /// Stable task name.
        task: String,
        /// Unsupported task requirement.
        reason: &'static str,
    },

    /// Filesystem or process I/O failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// A loaded task package changed before an attempt could use it.
    #[error(transparent)]
    TaskPackage(#[from] TaskLoadError),

    /// Retained evaluation output would mutate a hashed task package.
    #[error("evaluation output {output} must not be nested in task package {task}")]
    OutputOverlapsTask {
        /// Prospective canonical output parent.
        output: PathBuf,
        /// Canonical task package root.
        task: PathBuf,
    },

    /// Agent setup or execution failed.
    #[error("Nanocodex failed: {0}")]
    Nanocodex(#[from] NanocodexError),

    /// Agent execution completed but explicit resource cleanup failed.
    #[error("Nanocodex cleanup failed: {0}")]
    AgentCleanup(#[source] NanocodexError),

    /// The attempt backend factory failed.
    #[error("failed to configure attempt agent: {0}")]
    AttemptAgent(#[source] AttemptError),

    /// An attempt-owned verifier failed.
    #[error("attempt verifier failed: {0}")]
    AttemptVerifier(#[source] AttemptError),

    /// Agent execution exceeded the task deadline.
    #[error("agent exceeded its {0:?} timeout")]
    AgentTimeout(Duration),

    /// Verifier execution exceeded the task deadline.
    #[error("verifier exceeded its {0:?} timeout")]
    VerifierTimeout(Duration),

    /// The agent firehose ended without a terminal event.
    #[error("agent event stream closed before a terminal event")]
    AgentEventsClosed,

    /// Typed artifact JSON could not be encoded or decoded.
    #[error("failed to encode or decode JSON: {0}")]
    Json(#[from] serde_json::Error),

    /// An existing job is bound to a different sweep manifest.
    #[error("evaluation job is already bound to a different run: {0}")]
    RunConflict(PathBuf),

    /// A resumable job uses an incompatible task digest algorithm.
    #[error(
        "evaluation job {path} uses task digest schema `{found}`; expected `{expected}`; \
         start an explicit fresh run to cross this recovery boundary"
    )]
    RunDigestSchemaIncompatible {
        /// Retained job whose task identity cannot be compared safely.
        path: PathBuf,
        /// Retained schema label.
        found: String,
        /// Current schema label.
        expected: String,
    },

    /// A retained terminal result did not belong to its finite run.
    #[error("invalid durable evaluation trial: {0}")]
    InvalidDurableTrial(String),

    /// Another process still owns the matching resumable job.
    #[error("matching incomplete evaluation job is already active: {0}")]
    RunActive(PathBuf),

    /// A verifier emitted an invalid numeric reward.
    #[error("invalid verifier reward: {0}")]
    ParseReward(#[from] ParseFloatError),

    /// Internal sweep execution lost its stable coordinate.
    #[error("sweep execution lost its task-agent-trial coordinates")]
    MissingSweepCoordinate,
}

impl Evaluator {
    /// Starts an evaluator builder from a reusable Nanocodex recipe.
    ///
    /// Every attempt receives an independent session and workspace. The recipe
    /// automatically shares only its immutable prompt-cache warmup.
    #[must_use]
    pub fn builder(nanocodex: NanocodexBuilder) -> EvaluatorBuilder {
        EvaluatorBuilder {
            nanocodex: nanocodex.shared_prompt_cache(),
            output_directory: PathBuf::from(".nanocodex/evals"),
            max_concurrency: 1,
            max_memory_mb: None,
            attempt_environment: EvalEnvironment::Native,
            attempt_agent: None,
            finite_run: None,
        }
    }

    /// Runs one independent attempt.
    ///
    /// # Errors
    ///
    /// Returns an operational error when the attempt cannot be admitted.
    /// Accepted setup, agent, and verifier failures are returned as typed
    /// [`EvalAttemptOutcome::Unscored`] values.
    pub async fn task(&self, task: Task) -> Result<EvalAttemptOutcome, EvalError> {
        let queued_at = Utc::now();
        let _permit = self
            .inner
            .admission
            .acquire(task.resources().memory_mb)
            .await
            .ok_or(EvalError::Draining)?;
        self.run_task(AttemptInput {
            task,
            nanocodex: self.inner.nanocodex.clone(),
            coordinate: None,
            queued_at,
        })
        .await
        .map(|output| output.outcome)
    }

    /// Runs `count` fresh attempts of the same immutable task.
    ///
    /// Results preserve attempt order even when work completes out of order.
    ///
    /// # Errors
    ///
    /// Returns an operational error when the batch cannot be scheduled or
    /// retained. Attempt failures remain in their original positions.
    pub async fn task_n(
        &self,
        task: Task,
        count: usize,
    ) -> Result<Vec<EvalAttemptOutcome>, EvalError> {
        self.tasks(std::iter::repeat_n(task, count).collect()).await
    }

    /// Runs one independent attempt for every task in `tasks`.
    ///
    /// # Errors
    ///
    /// Returns an operational error when the batch cannot be scheduled or
    /// retained. Attempt failures remain in their original positions.
    pub async fn tasks(&self, tasks: Vec<Task>) -> Result<Vec<EvalAttemptOutcome>, EvalError> {
        let inputs = tasks
            .into_iter()
            .map(|task| AttemptInput {
                task,
                nanocodex: self.inner.nanocodex.clone(),
                coordinate: None,
                queued_at: Utc::now(),
            })
            .collect();
        Ok(self
            .run_tasks(inputs)
            .await?
            .into_iter()
            .map(|output| output.outcome)
            .collect())
    }

    /// Runs `count` fresh attempts for every task in `tasks`.
    ///
    /// Results are grouped in input task order and then trial order.
    ///
    /// # Errors
    ///
    /// Returns an operational error when the batch cannot be scheduled or
    /// retained. Attempt failures remain in their original positions.
    pub async fn tasks_n(
        &self,
        tasks: Vec<Task>,
        count: usize,
    ) -> Result<Vec<EvalAttemptOutcome>, EvalError> {
        self.tasks(
            tasks
                .into_iter()
                .flat_map(|task| std::iter::repeat_n(task, count))
                .collect(),
        )
        .await
    }

    /// Runs an advanced finite task-by-agent-by-trial sweep.
    ///
    /// # Errors
    ///
    /// Returns an operational error when run binding or durable recovery fails.
    /// Every accepted task × agent × trial coordinate is returned, including
    /// unscored attempts.
    pub async fn sweep(&self, sweep: Sweep) -> Result<SweepResults, EvalError> {
        let manifest = sweep.manifest();
        self.inner.job.bind_run(&manifest)?;
        let completed = self.inner.job.completed_coordinates(&manifest)?;
        let mut skipped = 0;
        let mut inputs = Vec::new();
        for attempt in sweep.attempts() {
            if completed.contains(&attempt.coordinate()) {
                skipped += 1;
                continue;
            }
            inputs.push(AttemptInput {
                task: attempt.task().clone(),
                nanocodex: attempt.nanocodex().clone(),
                coordinate: Some(SweepCoordinate {
                    agent: attempt.agent_id().clone(),
                    trial: attempt.trial(),
                }),
                queued_at: Utc::now(),
            });
        }
        let attempts = self
            .run_tasks(inputs)
            .await?
            .into_iter()
            .map(|output| {
                let coordinate = output.coordinate.ok_or(EvalError::MissingSweepCoordinate)?;
                Ok(SweepAttemptResult::new(
                    coordinate.agent,
                    coordinate.trial,
                    output.outcome,
                ))
            })
            .collect::<Result<Vec<_>, EvalError>>()?;
        Ok(SweepResults::new(attempts, skipped))
    }

    /// Returns how many attempts in `sweep` do not yet have a durable terminal
    /// result in this evaluator's job directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the job is bound to another sweep or its retained
    /// artifacts cannot be inspected.
    pub fn remaining_attempts(&self, sweep: &Sweep) -> Result<usize, EvalError> {
        let manifest = sweep.manifest();
        self.inner.job.bind_run(&manifest)?;
        let completed = self.inner.job.completed_coordinates(&manifest)?;
        let mut remaining = 0;
        for attempt in sweep.attempts() {
            if !completed.contains(&attempt.coordinate()) {
                remaining += 1;
            }
        }
        Ok(remaining)
    }

    async fn run_tasks(&self, tasks: Vec<AttemptInput>) -> Result<Vec<AttemptOutput>, EvalError> {
        let scheduling_window = tasks
            .len()
            .min(self.inner.max_concurrency.saturating_mul(4))
            .max(1);
        let evaluator = self.clone();
        let mut completed = stream::iter(tasks.into_iter().enumerate())
            .map(move |(index, input)| {
                let evaluator = evaluator.clone();
                async move {
                    let _permit = evaluator
                        .inner
                        .admission
                        .acquire(input.task.resources().memory_mb)
                        .await?;
                    let result = evaluator.run_task(input).await;
                    Some((index, result))
                }
            })
            .buffer_unordered(scheduling_window);
        let mut results = Vec::new();
        while let Some(output) = completed.next().await {
            let Some((index, result)) = output else {
                continue;
            };
            results.push((index, result?));
        }
        results.sort_unstable_by_key(|(index, _)| *index);
        Ok(results.into_iter().map(|(_, result)| result).collect())
    }

    /// Returns the stable identifier shared by this evaluator's attempts.
    #[must_use]
    pub fn id(&self) -> Uuid {
        self.inner.job.id()
    }

    /// Returns when this evaluator was built.
    #[must_use]
    pub fn started_at(&self) -> DateTime<Utc> {
        self.inner.job.started_at()
    }

    /// Returns the directory containing this evaluator's attempt artifacts.
    #[must_use]
    pub fn directory(&self) -> &std::path::Path {
        self.inner.job.directory()
    }

    /// Returns the parent directory containing evaluation jobs.
    #[must_use]
    pub fn parent_directory(&self) -> &std::path::Path {
        self.inner.job.parent_directory()
    }

    /// Returns whether this evaluator reopened an incomplete matching job.
    #[must_use]
    pub fn resumed(&self) -> bool {
        self.inner.job.resumed()
    }

    /// Returns the finite attempt count fixed by `resume_incomplete`, when set.
    #[must_use]
    pub fn planned_attempts(&self) -> Option<usize> {
        self.inner.planned_attempts
    }

    /// Returns the maximum number of concurrently executing attempts.
    #[must_use]
    pub fn max_concurrency(&self) -> usize {
        self.inner.max_concurrency
    }

    /// Returns the optional ceiling on concurrently admitted task-declared
    /// memory.
    #[must_use]
    pub fn max_memory_mb(&self) -> Option<u64> {
        self.inner.max_memory_mb
    }

    /// Stops admission of attempts that have not started and returns the number
    /// admitted since this evaluator was built.
    ///
    /// Attempts that already hold admission continue normally. Repeated calls
    /// are idempotent and return the same final admitted count.
    pub fn begin_drain(&self) -> usize {
        self.inner.admission.begin_drain()
    }

    /// Returns the execution environment selected for every attempt.
    #[must_use]
    pub fn attempt_environment(&self) -> EvalEnvironment {
        self.inner.attempt_environment
    }

    async fn run_task(&self, input: AttemptInput) -> Result<AttemptOutput, EvalError> {
        let AttemptInput {
            task,
            nanocodex,
            coordinate,
            queued_at,
        } = input;
        let session_id = SessionId::new();
        let attempt_id = session_id.as_uuid();
        let prompt_cache_cohort = self
            .inner
            .next_prompt_cache_attempt
            .fetch_add(1, Ordering::Relaxed)
            / PROMPT_CACHE_COHORT_SIZE;
        let trial_name = trial_name(&task, attempt_id, coordinate.as_ref());
        let admitted_at = Utc::now();
        let queue_wait = PhaseTiming {
            started_at: queued_at,
            finished_at: admitted_at,
        };
        let started_at = queued_at;
        let mut emitter =
            AttemptEmitter::new(self, session_id, prompt_cache_cohort, &task, &trial_name);
        let span = attempt_span(
            self,
            &task,
            attempt_id,
            &trial_name,
            prompt_cache_cohort,
            coordinate.as_ref(),
        );
        record_content(&span, "task.prompt", task.prompt());
        let trace_started = Instant::now();
        let result = self
            .run_task_inner(
                task.clone(),
                nanocodex,
                attempt_id,
                trial_name.clone(),
                queue_wait.clone(),
                &mut emitter,
            )
            .instrument(span.clone())
            .await;
        record_attempt_result(&span, trace_started, &result);
        let outcome = match result {
            Ok(result) => EvalAttemptOutcome::Scored(result),
            Err(failure) => {
                let failure = attempt_failure(
                    self, attempt_id, task, trial_name, started_at, queue_wait, &failure,
                );
                emitter.emit(EvalEventKind::Failed(Box::new(failure.clone())));
                EvalAttemptOutcome::Unscored(failure)
            }
        };
        Ok(AttemptOutput {
            outcome,
            coordinate,
        })
    }

    async fn run_task_inner(
        &self,
        task: Task,
        nanocodex: NanocodexBuilder,
        attempt_id: Uuid,
        trial_name: String,
        queue_wait: PhaseTiming,
        emitter: &mut AttemptEmitter<'_>,
    ) -> Result<EvalResult, AttemptRunFailure> {
        reject_output_overlap(self.inner.job.parent_directory(), task.root())
            .map_err(AttemptRunFailure::new)?;
        task.validate_package()
            .map_err(|error| AttemptRunFailure::new(EvalError::TaskPackage(error)))?;
        let attempt = {
            let span = info_span!(
                target: "nanocodex_eval",
                "eval.environment.setup",
                otel.kind = "internal",
                otel.status_code = tracing::field::Empty,
                eval.task.name = task.name(),
                eval.trial.name = trial_name.as_str(),
                output.directory = %self.inner.job.directory().display(),
                status = tracing::field::Empty,
                error.message = tracing::field::Empty,
                duration_ns = tracing::field::Empty,
            );
            let trace_started = Instant::now();
            let result = span.in_scope(|| {
                validate_attempt_environment(&task, self.inner.attempt_agent.is_some())?;
                NativeAttempt::prepare(self.inner.job.directory(), &trial_name, &task)
            });
            record_span_result(&span, trace_started, &result);
            result.map_err(AttemptRunFailure::new)?
        };
        emitter.emit(EvalEventKind::AttemptStarted {
            prompt: task.prompt().to_owned(),
            workspace: attempt.paths.workspace.clone(),
        });
        let mut agent = self
            .execute_agent(emitter, &task, &attempt, nanocodex)
            .await
            .map_err(|failure| AttemptRunFailure::from_agent(&attempt, failure))?;

        if let Err(error) = task.validate_package() {
            let verifier_cleanup = shutdown_attempt_verifier(&mut agent.verifier).await;
            return Err(AttemptRunFailure::after_agent(
                &attempt,
                &agent,
                EvalError::TaskPackage(error),
                verifier_cleanup,
            ));
        }
        emitter.emit(EvalEventKind::VerifierStarted);
        let verifier = match self
            .execute_verifier(&task, &attempt, agent.verifier.take())
            .await
        {
            Ok(verifier) => verifier,
            Err(failure) => {
                return Err(AttemptRunFailure::after_verifier_failure(
                    &attempt, &agent, failure,
                ));
            }
        };
        task.validate_package().map_err(|error| {
            AttemptRunFailure::after_verifier(
                &attempt,
                &agent,
                &verifier,
                EvalError::TaskPackage(error),
            )
        })?;
        emitter.emit(EvalEventKind::VerifierOutput {
            stdout: verifier.stdout.clone(),
            stderr: verifier.stderr.clone(),
        });
        emitter.emit(EvalEventKind::VerifierCompleted(verifier.result.clone()));

        let status = verifier_status(&verifier.result);
        let result = EvalResult {
            attempt_id,
            task_name: task.name().to_owned(),
            trial_name,
            status,
            outcome: match status {
                EvalStatus::Passed => EvalOutcome::Passed,
                EvalStatus::Failed => EvalOutcome::VerifierFailed,
            },
            environment: self.inner.attempt_environment,
            agent: agent.result,
            verifier: verifier.result,
            timing: EvalTiming {
                started_at: queue_wait.started_at,
                finished_at: Utc::now(),
                queue_wait,
                environment_setup: attempt.setup_timing.clone(),
                environment_readiness: agent.readiness_timing,
                agent_setup: agent.setup_timing,
                agent_execution: agent.execution_timing,
                verifier: verifier.timing,
            },
            cleanup: EvalCleanup {
                agent: agent.cleanup,
                verifier: verifier.cleanup,
            },
            artifacts: EvalArtifacts {
                directory: attempt.paths.root.clone(),
                workspace: attempt.paths.workspace.clone(),
                verifier_output: attempt.paths.verifier_output.clone(),
            },
            task,
        };
        emitter.emit(EvalEventKind::Completed(Box::new(result.clone())));
        Ok(result)
    }

    async fn execute_verifier(
        &self,
        task: &Task,
        attempt: &NativeAttempt,
        verifier: Option<Box<dyn AttemptVerifier>>,
    ) -> Result<VerifierExecution, VerifierExecutionFailure> {
        let span = info_span!(
            target: "nanocodex_eval",
            "eval.verifier",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            eval.task.name = task.name(),
            verifier.script = %task.verifier().script().display(),
            verifier.timeout_ms = duration_ms(task.verifier().timeout()),
            process.exit.code = tracing::field::Empty,
            verifier.reward.total = tracing::field::Empty,
            verifier.passed = tracing::field::Empty,
            verifier.stdout.bytes = tracing::field::Empty,
            verifier.stderr.bytes = tracing::field::Empty,
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        let trace_started = Instant::now();
        let result = async {
            if let Some(mut verifier) = verifier {
                let started_at = Utc::now();
                let execution = match verifier
                    .verify(
                        task,
                        EvalAttempt {
                            task,
                            directory: &attempt.paths.root,
                            workspace: &attempt.paths.workspace,
                        },
                    )
                    .await
                {
                    Ok(execution) => execution,
                    Err(failure) => {
                        let (error, cleanup) = failure.into_parts();
                        let finished_at = cleanup
                            .timing
                            .as_ref()
                            .map_or_else(Utc::now, |timing| timing.started_at);
                        return Err(VerifierExecutionFailure {
                            error: EvalError::AttemptVerifier(error),
                            cleanup,
                            timing: Some(PhaseTiming {
                                started_at,
                                finished_at,
                            }),
                        });
                    }
                };
                Ok(VerifierExecution {
                    result: execution.result,
                    timing: PhaseTiming {
                        started_at,
                        finished_at: execution
                            .cleanup
                            .timing
                            .as_ref()
                            .map_or_else(Utc::now, |timing| timing.started_at),
                    },
                    stdout: execution.stdout,
                    stderr: execution.stderr,
                    cleanup: execution.cleanup,
                })
            } else {
                let started_at = Utc::now();
                attempt
                    .verify(task)
                    .await
                    .map_err(|error| VerifierExecutionFailure {
                        error,
                        cleanup: CleanupPhase::not_required(),
                        timing: Some(PhaseTiming::finished(started_at)),
                    })
            }
        }
        .instrument(span.clone())
        .await;
        if let Ok(verifier) = &result {
            let passed = verifier.result.rewards.values().all(|reward| *reward > 0.0);
            span.record("process.exit.code", verifier.result.exit_code);
            span.record(
                "verifier.reward.total",
                verifier.result.rewards.values().sum::<f64>(),
            );
            span.record("verifier.passed", passed);
            span.record("verifier.stdout.bytes", verifier.stdout.len());
            span.record("verifier.stderr.bytes", verifier.stderr.len());
            record_content(&span, "verifier.stdout", &verifier.stdout);
            record_content(&span, "verifier.stderr", &verifier.stderr);
        }
        record_span_result(&span, trace_started, &result);
        result
    }

    async fn execute_agent(
        &self,
        emitter: &mut AttemptEmitter<'_>,
        task: &Task,
        attempt: &NativeAttempt,
        nanocodex: NanocodexBuilder,
    ) -> Result<AgentExecution, AgentExecutionFailure> {
        let AgentSetup {
            agent,
            mut events,
            mut verifier,
            readiness_timing,
            timing: setup_timing,
        } = self.setup_agent(emitter, task, attempt, nanocodex).await?;
        let execution_started = Utc::now();
        let span = info_span!(
            target: "nanocodex_eval",
            "eval.agent.execution",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            eval.task.name = task.name(),
            eval.attempt.id = %emitter.attempt_id,
            agent.timeout_ms = duration_ms(task.agent_timeout()),
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        let trace_started = Instant::now();
        let result = async {
            let turn = agent.prompt(task.prompt()).await?;
            let control = turn.control();
            let mut observation = AgentObservation::default();
            let event_result = timeout(
                task.agent_timeout(),
                receive_agent_terminal(&mut events, emitter, &mut observation),
            )
            .await;
            match event_result {
                Ok(Ok(terminal)) => {
                    let (primary, final_message) = match turn.result().await {
                        Ok(result) => (None, result.into_final_message()),
                        Err(error) => (
                            Some(EvalError::Nanocodex(error)),
                            observation.final_message.clone(),
                        ),
                    };
                    let completeness = observation.billing_completeness();
                    let result = AgentResult::from_terminal(final_message, &terminal, completeness);
                    match (primary, result) {
                        (Some(primary), Ok(result)) => Ok(AgentTurnOutcome {
                            primary: Some(primary),
                            result: Some(result),
                        }),
                        (Some(primary), Err(parse_error)) => {
                            tracing::warn!(
                                target: "nanocodex_eval",
                                error = %parse_error,
                                primary_error = %primary,
                                "failed to decode partial terminal agent metrics"
                            );
                            Ok(AgentTurnOutcome {
                                primary: Some(primary),
                                result: None,
                            })
                        }
                        (None, Ok(result)) => Ok(AgentTurnOutcome {
                            primary: None,
                            result: Some(result),
                        }),
                        (None, Err(error)) => Err(error),
                    }
                }
                Ok(Err(error)) => Err(error),
                Err(_) => {
                    let completeness = observation.billing_completeness();
                    let primary = EvalError::AgentTimeout(task.agent_timeout());
                    let _ = control.cancel().await;
                    let result = receive_agent_terminal(&mut events, emitter, &mut observation)
                        .await
                        .and_then(|terminal| {
                            AgentResult::from_terminal(
                                observation.final_message,
                                &terminal,
                                completeness,
                            )
                        });
                    match result {
                        Ok(result) => Ok(AgentTurnOutcome {
                            primary: Some(primary),
                            result: Some(result),
                        }),
                        Err(error) => {
                            tracing::warn!(
                                target: "nanocodex_eval",
                                error = %error,
                                primary_error = %primary,
                                "failed to retain terminal metrics after agent timeout"
                            );
                            Ok(AgentTurnOutcome {
                                primary: Some(primary),
                                result: None,
                            })
                        }
                    }
                }
            }
        };
        let result = result.instrument(span.clone()).await;
        record_span_result(&span, trace_started, &result);
        let execution_timing = PhaseTiming::finished(execution_started);
        let cleanup_started = Utc::now();
        let (cleanup, cleanup_error) = match agent.shutdown().await {
            Ok(()) => (CleanupPhase::completed(cleanup_started), None),
            Err(error) => {
                let cleanup = CleanupPhase::failed(cleanup_started, &error);
                (cleanup, Some(error))
            }
        };
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => AgentTurnOutcome {
                primary: Some(error),
                result: None,
            },
        };
        let primary = outcome
            .primary
            .or_else(|| cleanup_error.map(EvalError::AgentCleanup));
        if let Some(error) = primary {
            let verifier_cleanup = shutdown_attempt_verifier(&mut verifier).await;
            return Err(AgentExecutionFailure {
                error,
                result: outcome.result,
                cleanup,
                verifier_cleanup,
                readiness_timing: Some(readiness_timing),
                setup_timing: Some(setup_timing),
                execution_timing: Some(execution_timing),
            });
        }
        let Some(result) = outcome.result else {
            let verifier_cleanup = shutdown_attempt_verifier(&mut verifier).await;
            return Err(AgentExecutionFailure {
                error: EvalError::AgentEventsClosed,
                result: None,
                cleanup,
                verifier_cleanup,
                readiness_timing: Some(readiness_timing),
                setup_timing: Some(setup_timing),
                execution_timing: Some(execution_timing),
            });
        };
        Ok(AgentExecution {
            result,
            verifier,
            readiness_timing,
            setup_timing,
            execution_timing,
            cleanup,
        })
    }

    async fn setup_agent(
        &self,
        emitter: &AttemptEmitter<'_>,
        task: &Task,
        attempt: &NativeAttempt,
        nanocodex: NanocodexBuilder,
    ) -> Result<AgentSetup, AgentExecutionFailure> {
        let readiness_started = Utc::now();
        let span = info_span!(
            target: "nanocodex_eval",
            "eval.agent.setup",
            otel.kind = "internal",
            otel.status_code = tracing::field::Empty,
            eval.task.name = task.name(),
            eval.attempt.id = %emitter.attempt_id,
            workspace = %attempt.paths.workspace.display(),
            status = tracing::field::Empty,
            error.message = tracing::field::Empty,
            duration_ns = tracing::field::Empty,
        );
        let trace_started = Instant::now();
        let result = async {
            let builder = nanocodex
                .workspace(&attempt.paths.workspace)
                .session_id(emitter.session_id)
                .prompt_cache_key(format!(
                    "nanoeval:{}:{:x}",
                    self.id().simple(),
                    emitter.prompt_cache_cohort
                ));
            let configured = if let Some(factory) = &self.inner.attempt_agent {
                match factory(
                    EvalAttempt {
                        task,
                        directory: &attempt.paths.root,
                        workspace: &attempt.paths.workspace,
                    },
                    builder,
                ) {
                    Ok(configured) => configured,
                    Err(error) => {
                        return Err(AgentExecutionFailure::setup(
                            EvalError::AttemptAgent(error),
                            CleanupPhase::not_required(),
                            None,
                        ));
                    }
                }
            } else {
                AttemptAgent::new(builder)
            };
            let (builder, readiness, mut verifier) = configured.into_parts();
            if let Some(readiness) = readiness
                && let Err(error) = readiness.await
            {
                let verifier_cleanup = shutdown_attempt_verifier(&mut verifier).await;
                return Err(AgentExecutionFailure::setup(
                    EvalError::AttemptAgent(error),
                    verifier_cleanup,
                    None,
                ));
            }
            let readiness_timing = PhaseTiming::finished(readiness_started);
            let setup_started = Utc::now();
            match builder.build() {
                Ok((agent, events)) => Ok(AgentSetup {
                    agent,
                    events,
                    verifier,
                    readiness_timing,
                    timing: PhaseTiming::finished(setup_started),
                }),
                Err(error) => {
                    let verifier_cleanup = shutdown_attempt_verifier(&mut verifier).await;
                    Err(AgentExecutionFailure::setup(
                        EvalError::Nanocodex(error),
                        verifier_cleanup,
                        Some(readiness_timing),
                    ))
                }
            }
        }
        .instrument(span.clone())
        .await;
        record_span_result(&span, trace_started, &result);
        result
    }
}

async fn shutdown_attempt_verifier(
    verifier: &mut Option<Box<dyn AttemptVerifier>>,
) -> CleanupPhase {
    let Some(mut verifier) = verifier.take() else {
        return CleanupPhase::not_required();
    };
    verifier.shutdown().await
}

struct AgentExecution {
    result: AgentResult,
    verifier: Option<Box<dyn AttemptVerifier>>,
    readiness_timing: PhaseTiming,
    setup_timing: PhaseTiming,
    execution_timing: PhaseTiming,
    cleanup: CleanupPhase,
}

#[derive(Debug)]
struct AgentExecutionFailure {
    error: EvalError,
    result: Option<AgentResult>,
    cleanup: CleanupPhase,
    verifier_cleanup: CleanupPhase,
    readiness_timing: Option<PhaseTiming>,
    setup_timing: Option<PhaseTiming>,
    execution_timing: Option<PhaseTiming>,
}

impl AgentExecutionFailure {
    const fn setup(
        error: EvalError,
        verifier_cleanup: CleanupPhase,
        readiness_timing: Option<PhaseTiming>,
    ) -> Self {
        Self {
            error,
            result: None,
            cleanup: CleanupPhase::not_required(),
            verifier_cleanup,
            readiness_timing,
            setup_timing: None,
            execution_timing: None,
        }
    }
}

impl fmt::Display for AgentExecutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl Error for AgentExecutionFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

#[derive(Debug)]
struct VerifierExecutionFailure {
    error: EvalError,
    cleanup: CleanupPhase,
    timing: Option<PhaseTiming>,
}

impl fmt::Display for VerifierExecutionFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl Error for VerifierExecutionFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.error)
    }
}

struct AgentTurnOutcome {
    primary: Option<EvalError>,
    result: Option<AgentResult>,
}

#[derive(Default)]
struct AgentObservation {
    billable_in_flight: u32,
    billing_unknown: bool,
    final_message: String,
}

struct AttemptRunFailure {
    error: EvalError,
    agent: Option<AgentResult>,
    verifier: Option<VerifierResult>,
    cleanup: EvalCleanup,
    environment_setup: Option<PhaseTiming>,
    environment_readiness: Option<PhaseTiming>,
    agent_setup: Option<PhaseTiming>,
    agent_execution: Option<PhaseTiming>,
    verifier_timing: Option<PhaseTiming>,
}

impl AttemptRunFailure {
    fn new(error: EvalError) -> Self {
        Self {
            error,
            agent: None,
            verifier: None,
            cleanup: EvalCleanup::default(),
            environment_setup: None,
            environment_readiness: None,
            agent_setup: None,
            agent_execution: None,
            verifier_timing: None,
        }
    }

    fn from_agent(attempt: &NativeAttempt, failure: AgentExecutionFailure) -> Self {
        Self {
            error: failure.error,
            agent: failure.result,
            verifier: None,
            cleanup: EvalCleanup {
                agent: failure.cleanup,
                verifier: failure.verifier_cleanup,
            },
            environment_setup: Some(attempt.setup_timing.clone()),
            environment_readiness: failure.readiness_timing,
            agent_setup: failure.setup_timing,
            agent_execution: failure.execution_timing,
            verifier_timing: None,
        }
    }

    fn after_agent(
        attempt: &NativeAttempt,
        agent: &AgentExecution,
        error: EvalError,
        verifier_cleanup: CleanupPhase,
    ) -> Self {
        Self {
            error,
            agent: Some(agent.result.clone()),
            verifier: None,
            cleanup: EvalCleanup {
                agent: agent.cleanup.clone(),
                verifier: verifier_cleanup,
            },
            environment_setup: Some(attempt.setup_timing.clone()),
            environment_readiness: Some(agent.readiness_timing.clone()),
            agent_setup: Some(agent.setup_timing.clone()),
            agent_execution: Some(agent.execution_timing.clone()),
            verifier_timing: None,
        }
    }

    fn after_verifier_failure(
        attempt: &NativeAttempt,
        agent: &AgentExecution,
        failure: VerifierExecutionFailure,
    ) -> Self {
        Self {
            error: failure.error,
            agent: Some(agent.result.clone()),
            verifier: None,
            cleanup: EvalCleanup {
                agent: agent.cleanup.clone(),
                verifier: failure.cleanup,
            },
            environment_setup: Some(attempt.setup_timing.clone()),
            environment_readiness: Some(agent.readiness_timing.clone()),
            agent_setup: Some(agent.setup_timing.clone()),
            agent_execution: Some(agent.execution_timing.clone()),
            verifier_timing: failure.timing,
        }
    }

    fn after_verifier(
        attempt: &NativeAttempt,
        agent: &AgentExecution,
        verifier: &VerifierExecution,
        error: EvalError,
    ) -> Self {
        Self {
            error,
            agent: Some(agent.result.clone()),
            verifier: Some(verifier.result.clone()),
            cleanup: EvalCleanup {
                agent: agent.cleanup.clone(),
                verifier: verifier.cleanup.clone(),
            },
            environment_setup: Some(attempt.setup_timing.clone()),
            environment_readiness: Some(agent.readiness_timing.clone()),
            agent_setup: Some(agent.setup_timing.clone()),
            agent_execution: Some(agent.execution_timing.clone()),
            verifier_timing: Some(verifier.timing.clone()),
        }
    }
}

async fn receive_agent_terminal(
    events: &mut AgentEvents,
    emitter: &mut AttemptEmitter<'_>,
    observation: &mut AgentObservation,
) -> Result<AgentEvent, EvalError> {
    loop {
        let event = events.recv().await.ok_or(EvalError::AgentEventsClosed)?;
        observation.observe(&event)?;
        let terminal = event.kind.is_terminal();
        emitter.emit(EvalEventKind::Agent(event.clone()));
        if terminal {
            return Ok(event);
        }
    }
}

impl AgentObservation {
    const fn billing_completeness(&self) -> BillingCompleteness {
        if self.billable_in_flight == 0 && !self.billing_unknown {
            BillingCompleteness::Complete
        } else {
            BillingCompleteness::Unknown
        }
    }

    fn observe(&mut self, event: &AgentEvent) -> Result<(), EvalError> {
        self.observe_lifecycle(event.kind);
        if event.kind == AgentEventKind::AssistantMessage {
            let message: nanocodex_agent::events::AssistantMessage = event.decode_payload()?;
            self.final_message = message.text;
        }
        Ok(())
    }

    const fn observe_lifecycle(&mut self, kind: AgentEventKind) {
        match kind {
            AgentEventKind::ModelWarmupStarted
            | AgentEventKind::ModelCallStarted
            | AgentEventKind::ModelCompactionStarted => {
                self.billable_in_flight = self.billable_in_flight.saturating_add(1);
            }
            AgentEventKind::ModelWarmupCompleted
            | AgentEventKind::ModelCallCompleted
            | AgentEventKind::ModelCompactionCompleted => {
                self.billable_in_flight = self.billable_in_flight.saturating_sub(1);
            }
            AgentEventKind::ModelWarmupFailed
            | AgentEventKind::ModelCallFailed
            | AgentEventKind::ModelCompactionFailed => {
                self.billable_in_flight = self.billable_in_flight.saturating_sub(1);
                self.billing_unknown = true;
            }
            _ => {}
        }
    }
}

struct AgentSetup {
    agent: Nanocodex,
    events: AgentEvents,
    verifier: Option<Box<dyn AttemptVerifier>>,
    readiness_timing: PhaseTiming,
    timing: PhaseTiming,
}

impl EvaluatorBuilder {
    /// Sets the parent under which this evaluator creates one UUID-named
    /// artifact directory.
    #[must_use]
    pub fn output_directory(mut self, directory: impl Into<PathBuf>) -> Self {
        self.output_directory = directory.into();
        self
    }

    /// Reopens the newest incomplete job whose durable run manifest matches
    /// `sweep`, or creates a new job when none exists.
    #[must_use]
    pub fn resume_incomplete(mut self, sweep: &Sweep) -> Self {
        self.finite_run = Some(FiniteRun {
            manifest: sweep.manifest(),
            mode: FiniteRunMode::Resume,
        });
        self
    }

    /// Creates a new job already bound to `sweep`, even when a matching
    /// incomplete job exists.
    #[must_use]
    pub fn fresh_run(mut self, sweep: &Sweep) -> Self {
        self.finite_run = Some(FiniteRun {
            manifest: sweep.manifest(),
            mode: FiniteRunMode::Fresh,
        });
        self
    }

    /// Sets the maximum number of attempts allowed to execute concurrently.
    ///
    /// The default is one. [`Self::build`] rejects zero.
    #[must_use]
    pub const fn max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Bounds the sum of task-declared memory for concurrently running
    /// attempts. A task whose own declaration exceeds the ceiling runs alone.
    #[must_use]
    pub const fn max_memory_mb(mut self, max_memory_mb: u64) -> Self {
        self.max_memory_mb = Some(max_memory_mb);
        self
    }

    /// Records the execution environment used by the configured attempt
    /// backend in results and durable Harbor artifacts.
    #[must_use]
    pub const fn attempt_environment(mut self, environment: EvalEnvironment) -> Self {
        self.attempt_environment = environment;
        self
    }

    /// Configures the fresh Nanocodex builder for each attempt.
    ///
    /// The factory runs after the disposable workspace is populated and before
    /// the agent is built. This is the boundary for attempt-owned resources
    /// such as a retained VM tool session and its guest-visible workspace.
    #[must_use]
    pub fn attempt_agent<F, E>(mut self, factory: F) -> Self
    where
        F: for<'a> Fn(EvalAttempt<'a>, NanocodexBuilder) -> Result<AttemptAgent, E>
            + Send
            + Sync
            + 'static,
        E: Error + Send + Sync + 'static,
    {
        self.attempt_agent = Some(Arc::new(move |attempt, builder| {
            factory(attempt, builder).map_err(|error| Box::new(error) as AttemptError)
        }));
        self
    }

    /// Builds a reusable evaluator and a source of independent event streams.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid concurrency or an unavailable output path.
    pub fn build(self) -> Result<(Evaluator, EvalEvents), EvalError> {
        if self.max_concurrency == 0 {
            return Err(EvalError::InvalidConcurrency);
        }
        if self.max_memory_mb == Some(0) {
            return Err(EvalError::InvalidMemory);
        }
        if let Some(run) = &self.finite_run {
            let output = prospective_canonical_directory(&self.output_directory)?;
            for task in run.manifest.task_roots() {
                reject_output_overlap(&output, task)?;
            }
        }
        let planned_attempts = self
            .finite_run
            .as_ref()
            .map(|run| run.manifest.attempt_count());
        let job = match &self.finite_run {
            Some(run) => {
                let job = match run.mode {
                    FiniteRunMode::Fresh => EvalJob::create(&self.output_directory)?,
                    FiniteRunMode::Resume => {
                        EvalJob::resume_or_create(&self.output_directory, &run.manifest)?
                    }
                };
                job.bind_run(&run.manifest)?;
                job
            }
            None => EvalJob::create(&self.output_directory)?,
        };
        let (event_sender, _) = broadcast::channel(EVENT_CAPACITY);
        Ok((
            Evaluator {
                inner: Arc::new(EvaluatorInner {
                    nanocodex: self.nanocodex,
                    job,
                    planned_attempts,
                    admission: Arc::new(AdmissionController::new(
                        self.max_concurrency,
                        self.max_memory_mb,
                    )),
                    max_concurrency: self.max_concurrency,
                    max_memory_mb: self.max_memory_mb,
                    attempt_environment: self.attempt_environment,
                    next_prompt_cache_attempt: AtomicU64::new(0),
                    events: event_sender.clone(),
                    attempt_agent: self.attempt_agent,
                }),
            },
            EvalEvents::new(event_sender),
        ))
    }
}

impl AdmissionController {
    fn new(max_concurrency: usize, max_memory_mb: Option<u64>) -> Self {
        Self {
            max_concurrency,
            max_memory_mb,
            state: Mutex::new(AdmissionState::default()),
            changed: Notify::new(),
        }
    }

    async fn acquire(self: &Arc<Self>, requested_memory_mb: u64) -> Option<AdmissionPermit> {
        let memory_mb = self
            .max_memory_mb
            .map_or(0, |limit| requested_memory_mb.min(limit));
        loop {
            let changed = self.changed.notified();
            {
                let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
                if state.draining {
                    return None;
                }
                let concurrency_available = state.running < self.max_concurrency;
                let memory_available = self.max_memory_mb.is_none_or(|limit| {
                    state
                        .memory_mb
                        .checked_add(memory_mb)
                        .is_some_and(|total| total <= limit)
                });
                if concurrency_available && memory_available {
                    state.running += 1;
                    state.memory_mb += memory_mb;
                    state.admitted = state.admitted.saturating_add(1);
                    return Some(AdmissionPermit {
                        controller: Arc::clone(self),
                        memory_mb,
                    });
                }
            }
            changed.await;
        }
    }

    fn begin_drain(&self) -> usize {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.draining = true;
        let admitted = state.admitted;
        drop(state);
        self.changed.notify_waiters();
        admitted
    }
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        let mut state = self
            .controller
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.running = state.running.saturating_sub(1);
        state.memory_mb = state.memory_mb.saturating_sub(self.memory_mb);
        drop(state);
        self.controller.changed.notify_waiters();
    }
}

impl AttemptAgent {
    /// Uses `nanocodex` for one attempt with the default native verifier.
    #[must_use]
    pub fn new(nanocodex: NanocodexBuilder) -> Self {
        Self {
            nanocodex,
            readiness: None,
            verifier: None,
        }
    }

    /// Installs asynchronous environment readiness work that must complete
    /// before the agent is built or any model request is sent.
    ///
    /// VM adapters use this to wait for a typed guest handshake. A readiness
    /// failure aborts the attempt as an environment error without spending a
    /// model request.
    #[must_use]
    pub fn ready<F, E>(mut self, readiness: F) -> Self
    where
        F: Future<Output = Result<(), E>> + Send + 'static,
        E: Error + Send + Sync + 'static,
    {
        self.readiness = Some(Box::pin(async move {
            readiness
                .await
                .map_err(|error| Box::new(error) as AttemptError)
        }));
        self
    }

    /// Installs the verifier that owns this attempt's environment backend.
    #[must_use]
    pub fn verifier(mut self, verifier: impl AttemptVerifier + 'static) -> Self {
        self.verifier = Some(Box::new(verifier));
        self
    }

    fn into_parts(
        self,
    ) -> (
        NanocodexBuilder,
        Option<AttemptReadinessFuture>,
        Option<Box<dyn AttemptVerifier>>,
    ) {
        (self.nanocodex, self.readiness, self.verifier)
    }
}

impl EvalAttempt<'_> {
    /// Returns the immutable task definition.
    #[must_use]
    pub const fn task(&self) -> &Task {
        self.task
    }

    /// Returns the retained attempt root.
    #[must_use]
    pub const fn directory(&self) -> &Path {
        self.directory
    }

    /// Returns the workspace path presented to the agent.
    #[must_use]
    pub const fn workspace(&self) -> &Path {
        self.workspace
    }
}

struct AttemptEmitter<'a> {
    eval: &'a Evaluator,
    attempt_id: Uuid,
    session_id: SessionId,
    prompt_cache_cohort: u64,
    task_name: String,
    trial_name: String,
    sequence: u64,
}

impl<'a> AttemptEmitter<'a> {
    fn new(
        eval: &'a Evaluator,
        session_id: SessionId,
        prompt_cache_cohort: u64,
        task: &Task,
        trial_name: &str,
    ) -> Self {
        Self {
            eval,
            attempt_id: session_id.as_uuid(),
            session_id,
            prompt_cache_cohort,
            task_name: task.name().to_owned(),
            trial_name: trial_name.to_owned(),
            sequence: 0,
        }
    }

    fn emit(&mut self, kind: EvalEventKind) {
        self.sequence += 1;
        let _ = self.eval.inner.events.send(Arc::new(EvalEvent {
            run_id: self.eval.inner.job.id(),
            attempt_id: self.attempt_id,
            task_name: self.task_name.clone(),
            trial_name: self.trial_name.clone(),
            sequence: self.sequence,
            kind,
        }));
    }
}

#[derive(Deserialize)]
struct ResponsesApiErrorEnvelope {
    error: Option<ResponsesApiError>,
    response: Option<ResponsesApiErrorResponse>,
}

#[derive(Deserialize)]
struct ResponsesApiErrorResponse {
    error: Option<ResponsesApiError>,
}

#[derive(Deserialize)]
struct ResponsesApiError {
    code: Option<String>,
}

fn attempt_failure(
    eval: &Evaluator,
    attempt_id: Uuid,
    task: Task,
    trial_name: String,
    started_at: DateTime<Utc>,
    queue_wait: PhaseTiming,
    failure: &AttemptRunFailure,
) -> EvalFailure {
    let root = eval.directory().join(&trial_name);
    let model = failure
        .agent
        .as_ref()
        .map_or_else(|| MODEL.to_owned(), |agent| agent.model.clone());
    let effort = failure
        .agent
        .as_ref()
        .map_or_else(|| "unknown".to_owned(), |agent| agent.effort.clone());
    EvalFailure {
        attempt_id,
        task_name: task.name().to_owned(),
        trial_name,
        kind: failure_kind(&failure.error),
        outcome: failure_outcome(&failure.error),
        message: failure.error.to_string(),
        traceback: error_traceback(&failure.error),
        model,
        effort,
        environment: eval.attempt_environment(),
        started_at,
        occurred_at: Utc::now(),
        timing: EvalFailureTiming {
            queue_wait,
            environment_setup: failure.environment_setup.clone(),
            environment_readiness: failure.environment_readiness.clone(),
            agent_setup: failure.agent_setup.clone(),
            agent_execution: failure.agent_execution.clone(),
            verifier: failure.verifier_timing.clone(),
        },
        agent: failure.agent.clone(),
        verifier: failure.verifier.clone(),
        cleanup: failure.cleanup.clone(),
        artifacts: EvalArtifacts {
            workspace: root.join("workspace"),
            verifier_output: root.join("verifier/test-stdout.txt"),
            directory: root,
        },
        task,
    }
}

fn failure_outcome(error: &EvalError) -> EvalOutcome {
    match error {
        EvalError::Nanocodex(error) if is_safety_refusal(error) => EvalOutcome::SafetyRefusal,
        EvalError::AgentTimeout(_) => EvalOutcome::AgentTimeout,
        _ => EvalOutcome::InfrastructureError,
    }
}

fn failure_kind(error: &EvalError) -> EvalFailureKind {
    match error {
        EvalError::Nanocodex(error) if is_safety_refusal(error) => {
            EvalFailureKind::AgentSafetyRefusal
        }
        EvalError::Nanocodex(error)
            if error
                .responses_error()
                .is_some_and(|error| error.class() == "authorization") =>
        {
            EvalFailureKind::AgentAuthentication
        }
        EvalError::AgentTimeout(_) => EvalFailureKind::AgentTimeout,
        EvalError::VerifierTimeout(_) => EvalFailureKind::VerifierTimeout,
        EvalError::AgentCleanup(_) => EvalFailureKind::Cleanup,
        EvalError::Nanocodex(_) | EvalError::AgentEventsClosed => EvalFailureKind::Agent,
        EvalError::AttemptVerifier(_) | EvalError::ParseReward(_) => EvalFailureKind::Verifier,
        EvalError::UnsupportedNativeTask { .. }
        | EvalError::TaskPackage(_)
        | EvalError::OutputOverlapsTask { .. }
        | EvalError::AttemptAgent(_) => EvalFailureKind::Environment,
        EvalError::InvalidConcurrency
        | EvalError::InvalidMemory
        | EvalError::Draining
        | EvalError::InvalidDurableTrial(_)
        | EvalError::Io(_)
        | EvalError::Json(_)
        | EvalError::RunConflict(_)
        | EvalError::RunDigestSchemaIncompatible { .. }
        | EvalError::RunActive(_)
        | EvalError::MissingSweepCoordinate => EvalFailureKind::Internal,
    }
}

fn reject_output_overlap(output: &Path, task: &Path) -> Result<(), EvalError> {
    if output.starts_with(task) || output_aliases_task_package(output, task)? {
        return Err(EvalError::OutputOverlapsTask {
            output: output.to_path_buf(),
            task: task.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn output_aliases_task_package(output: &Path, task: &Path) -> io::Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    const PACKAGE_DIRECTORIES: [&str; 4] = ["environment", "tests", "solution", "steps"];

    // The output ancestry is application-owned and non-adversarial. Comparing
    // existing directory identities also catches accidental bind-mount aliases;
    // it is not intended to defend against a concurrent hostile path swap.
    let mut identities = std::collections::HashSet::<(u64, u64)>::new();
    let mut pending = vec![task.to_path_buf()];
    pending.extend(PACKAGE_DIRECTORIES.into_iter().map(|name| task.join(name)));
    while let Some(directory) = pending.pop() {
        let metadata = match fs::symlink_metadata(&directory) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "task package symlinks are unsupported while checking output ancestry: {}",
                    directory.display()
                ),
            ));
        }
        if !metadata.is_dir() || !identities.insert((metadata.dev(), metadata.ino())) {
            continue;
        }
        if directory != task {
            for entry in fs::read_dir(&directory)? {
                let entry = entry?;
                let metadata = fs::symlink_metadata(entry.path())?;
                if metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "task package symlinks are unsupported while checking output ancestry: {}",
                            entry.path().display()
                        ),
                    ));
                }
                if metadata.is_dir() {
                    pending.push(entry.path());
                }
            }
        }
    }

    for ancestor in output.ancestors() {
        match fs::metadata(ancestor) {
            Ok(metadata)
                if metadata.is_dir() && identities.contains(&(metadata.dev(), metadata.ino())) =>
            {
                return Ok(true);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

#[cfg(not(unix))]
fn output_aliases_task_package(_output: &Path, _task: &Path) -> io::Result<bool> {
    Ok(false)
}

fn prospective_canonical_directory(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut missing = Vec::<OsString>::new();
    let mut ancestor = absolute.as_path();
    loop {
        match fs::metadata(ancestor) {
            Ok(metadata) if metadata.is_dir() => break,
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::NotADirectory,
                    format!("path component is not a directory: {}", ancestor.display()),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(
                    ancestor
                        .file_name()
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::NotFound,
                                format!(
                                    "directory has no existing ancestor: {}",
                                    absolute.display()
                                ),
                            )
                        })?
                        .to_os_string(),
                );
                ancestor = ancestor.parent().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("directory has no existing ancestor: {}", absolute.display()),
                    )
                })?;
            }
            Err(error) => return Err(error),
        }
    }
    let mut canonical = fs::canonicalize(ancestor)?;
    canonical.extend(missing.into_iter().rev());
    Ok(canonical)
}

fn is_safety_refusal(error: &NanocodexError) -> bool {
    let Some(ResponsesError::Api { event }) = error.responses_error() else {
        return false;
    };
    serde_json::from_str::<ResponsesApiErrorEnvelope>(event)
        .ok()
        .and_then(|event| {
            event
                .error
                .or_else(|| event.response.and_then(|response| response.error))
                .and_then(|error| error.code)
        })
        .is_some_and(|code| code == "cyber_policy")
}

fn error_traceback(error: &dyn Error) -> String {
    let mut traceback = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        traceback.push_str("\nCaused by: ");
        traceback.push_str(&error.to_string());
        source = error.source();
    }
    traceback
}

fn attempt_span(
    eval: &Evaluator,
    task: &Task,
    attempt_id: Uuid,
    trial_name: &str,
    prompt_cache_cohort: u64,
    coordinate: Option<&SweepCoordinate>,
) -> Span {
    let span = info_span!(
        target: "nanocodex_eval",
        parent: None,
        "eval.attempt",
        otel.kind = "internal",
        otel.status_code = tracing::field::Empty,
        eval.id = %eval.id(),
        eval.attempt.id = %attempt_id,
        eval.task.name = task.name(),
        eval.trial.name = trial_name,
        eval.agent.id = tracing::field::Empty,
        eval.trial.number = tracing::field::Empty,
        eval.task.image = task.image().reference(),
        eval.resource.cpus = task.resources().cpus,
        eval.resource.memory_mib = task.resources().memory_mb,
        eval.resource.storage_mib = task.resources().storage_mb,
        eval.resource.gpus = task.resources().gpus,
        eval.network = task.network().as_str(),
        eval.score.status = tracing::field::Empty,
        eval.reward.total = tracing::field::Empty,
        agent.model_calls = tracing::field::Empty,
        agent.tool_calls = tracing::field::Empty,
        agent.response_attempts = tracing::field::Empty,
        agent.response_retries = tracing::field::Empty,
        agent.prompt_cache.cohort = prompt_cache_cohort,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.cached_input_tokens = tracing::field::Empty,
        gen_ai.usage.cache_write_input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.usage.total_tokens = tracing::field::Empty,
        agent.warmup.duration_ns = tracing::field::Empty,
        agent.warmup.input_tokens = tracing::field::Empty,
        agent.warmup.cached_input_tokens = tracing::field::Empty,
        agent.warmup.cache_write_input_tokens = tracing::field::Empty,
        agent.warmup.output_tokens = tracing::field::Empty,
        agent.warmup.total_tokens = tracing::field::Empty,
        cost.usd = tracing::field::Empty,
        eval.cleanup.failed = tracing::field::Empty,
        agent.cleanup.status = tracing::field::Empty,
        agent.cleanup.duration_ns = tracing::field::Empty,
        verifier.cleanup.status = tracing::field::Empty,
        verifier.cleanup.duration_ns = tracing::field::Empty,
        status = tracing::field::Empty,
        error.message = tracing::field::Empty,
        duration_ns = tracing::field::Empty,
    );
    if let Some(coordinate) = coordinate {
        span.record("eval.agent.id", coordinate.agent.as_str());
        span.record("eval.trial.number", coordinate.trial);
    }
    span
}

fn record_attempt_result(
    span: &Span,
    started_at: Instant,
    result: &Result<EvalResult, AttemptRunFailure>,
) {
    let duration_ns = elapsed_ns(started_at);
    span.record("duration_ns", duration_ns);
    match result {
        Ok(result) => {
            record_attempt_success(span, result);
            span.in_scope(|| {
                info!(
                    target: "nanocodex_eval",
                    duration_ns,
                    score.status = eval_status(result.status),
                    "evaluation attempt completed"
                );
            });
        }
        Err(failure) => {
            record_cleanup(span, &failure.cleanup);
            if let Some(agent) = &failure.agent {
                record_agent_metrics(span, agent);
            }
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            span.record("error.message", tracing::field::display(&failure.error));
            span.in_scope(|| {
                info!(
                    target: "nanocodex_eval",
                    duration_ns,
                    error = %failure.error,
                    "evaluation attempt failed"
                );
            });
        }
    }
}

fn record_attempt_success(span: &Span, result: &EvalResult) {
    record_cleanup(span, &result.cleanup);
    record_agent_metrics(span, &result.agent);
    span.record("status", "completed");
    span.record("otel.status_code", "OK");
    span.record("eval.score.status", eval_status(result.status));
    span.record(
        "eval.reward.total",
        result.verifier.rewards.values().sum::<f64>(),
    );
}

fn record_agent_metrics(span: &Span, agent: &AgentResult) {
    let usage = &agent.usage;
    let warmup = &agent.metadata.warmup_usage;
    span.record("agent.model_calls", agent.model_calls);
    span.record("agent.tool_calls", agent.tool_calls);
    span.record("agent.response_attempts", agent.metadata.response_attempts);
    span.record("agent.response_retries", agent.metadata.response_retries);
    span.record("gen_ai.usage.input_tokens", usage.input_tokens);
    span.record(
        "gen_ai.usage.cached_input_tokens",
        usage.cached_input_tokens,
    );
    span.record(
        "gen_ai.usage.cache_write_input_tokens",
        usage.cache_write_input_tokens,
    );
    span.record("gen_ai.usage.output_tokens", usage.output_tokens);
    span.record("gen_ai.usage.total_tokens", usage.total_tokens);
    span.record(
        "agent.warmup.duration_ns",
        agent.metadata.warmup_duration_ns,
    );
    span.record("agent.warmup.input_tokens", warmup.input_tokens);
    span.record(
        "agent.warmup.cached_input_tokens",
        warmup.cached_input_tokens,
    );
    span.record(
        "agent.warmup.cache_write_input_tokens",
        warmup.cache_write_input_tokens,
    );
    span.record("agent.warmup.output_tokens", warmup.output_tokens);
    span.record("agent.warmup.total_tokens", warmup.total_tokens);
    if let Some(cost_usd) = agent.cost_usd {
        span.record("cost.usd", cost_usd);
    }
}

fn record_cleanup(span: &Span, cleanup: &EvalCleanup) {
    span.record("eval.cleanup.failed", cleanup.is_failed());
    span.record("agent.cleanup.status", cleanup_status(&cleanup.agent));
    span.record(
        "agent.cleanup.duration_ns",
        cleanup.agent.timing.as_ref().map_or(0, phase_timing_ns),
    );
    span.record("verifier.cleanup.status", cleanup_status(&cleanup.verifier));
    span.record(
        "verifier.cleanup.duration_ns",
        cleanup.verifier.timing.as_ref().map_or(0, phase_timing_ns),
    );
}

const fn cleanup_status(cleanup: &CleanupPhase) -> &'static str {
    match cleanup.status {
        crate::CleanupStatus::NotRequired => "not_required",
        crate::CleanupStatus::Completed => "completed",
        crate::CleanupStatus::Failed => "failed",
    }
}

fn phase_timing_ns(timing: &PhaseTiming) -> u64 {
    u64::try_from(
        timing
            .finished_at
            .signed_duration_since(timing.started_at)
            .num_nanoseconds()
            .unwrap_or_default()
            .max(0),
    )
    .unwrap_or(u64::MAX)
}

const fn eval_status(status: EvalStatus) -> &'static str {
    match status {
        EvalStatus::Passed => "passed",
        EvalStatus::Failed => "failed",
    }
}

fn validate_attempt_environment(task: &Task, custom_backend: bool) -> Result<(), EvalError> {
    if task.requires_compose() && !custom_backend {
        return Err(EvalError::UnsupportedNativeTask {
            task: task.name().to_owned(),
            reason: "custom Docker Compose environments are not available in native mode",
        });
    }
    Ok(())
}

fn verifier_status(verifier: &crate::VerifierResult) -> EvalStatus {
    if verifier.rewards.values().all(|reward| *reward > 0.0) {
        EvalStatus::Passed
    } else {
        EvalStatus::Failed
    }
}

fn record_span_result<T, E>(span: &tracing::Span, started_at: Instant, result: &Result<T, E>)
where
    E: std::fmt::Display,
{
    let duration_ns = elapsed_ns(started_at);
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
                    "evaluation phase completed"
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
                    "evaluation phase failed"
                );
            });
        }
    }
}

fn record_content(span: &tracing::Span, kind: &'static str, content: &str) {
    span.in_scope(|| {
        info!(
            target: "nanocodex_eval",
            content_kind = kind,
            content,
            "evaluation content"
        );
    });
}

fn elapsed_ns(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn trial_name(task: &Task, attempt_id: Uuid, coordinate: Option<&SweepCoordinate>) -> String {
    let short_name = task.name().rsplit('/').next().unwrap_or(task.name());
    let compact_id = attempt_id.simple().to_string();
    match coordinate {
        Some(coordinate) => format!(
            "{short_name}__{}__{:03}__{}",
            coordinate.agent, coordinate.trial, compact_id
        ),
        None => format!("{short_name}__{compact_id}"),
    }
}

impl AgentResult {
    fn from_terminal(
        final_message: String,
        event: &AgentEvent,
        billing_completeness: BillingCompleteness,
    ) -> Result<Self, EvalError> {
        if !event.kind.is_terminal() {
            return Err(EvalError::AgentEventsClosed);
        }
        let metadata: AgentMetadata = serde_json::from_str(event.payload.get())?;
        Ok(Self {
            final_message,
            model: metadata.model.clone(),
            effort: metadata.effort.clone(),
            model_calls: metadata.model_calls,
            tool_calls: metadata.tool_calls,
            usage: metadata.usage.clone(),
            cost_usd: metadata.cost_usd,
            billing_completeness,
            metadata,
        })
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use std::{
        collections::BTreeMap,
        convert::Infallible,
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    use async_trait::async_trait;
    use futures_util::{SinkExt, StreamExt};
    use nanocodex_agent::{Nanocodex, OpenAi, Tools};
    use nanocodex_tools::{ToolContext, ToolDefinition, ToolOutput, runtime::DynamicToolProvider};
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio::net::TcpListener;
    use tokio_tungstenite::{accept_async, tungstenite::Message};

    use super::{
        AgentEventKind, AgentObservation, AttemptAgent, AttemptVerification, AttemptVerifier,
        AttemptVerifierCleanupFuture, AttemptVerifierFuture, EvalAttempt, Evaluator,
    };
    use crate::{
        AgentStatus, BillingCompleteness, CleanupPhase, CleanupStatus, EvalOutcome, EvalStatus,
        Task, VerifierResult,
    };

    struct AttemptResourceProvider {
        live_resources: Arc<AtomicUsize>,
    }

    struct PackageMutatingProvider {
        mutation: PathBuf,
    }

    impl Drop for AttemptResourceProvider {
        fn drop(&mut self) {
            self.live_resources.fetch_sub(1, Ordering::AcqRel);
        }
    }

    impl Drop for PackageMutatingProvider {
        fn drop(&mut self) {
            fs::write(&self.mutation, "changed after agent execution\n").unwrap();
        }
    }

    #[async_trait]
    impl DynamicToolProvider for AttemptResourceProvider {
        fn start(&self) {}

        fn direct_tools(&self) -> Vec<Arc<dyn nanocodex_tools::Tool>> {
            Vec::new()
        }

        fn available_definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }

        async fn execute(
            &self,
            _name: &str,
            _input: Value,
            _context: ToolContext<'_>,
        ) -> Option<ToolOutput> {
            None
        }
    }

    #[async_trait]
    impl DynamicToolProvider for PackageMutatingProvider {
        fn start(&self) {}

        fn direct_tools(&self) -> Vec<Arc<dyn nanocodex_tools::Tool>> {
            Vec::new()
        }

        fn available_definitions(&self) -> Vec<ToolDefinition> {
            Vec::new()
        }

        async fn execute(
            &self,
            _name: &str,
            _input: Value,
            _context: ToolContext<'_>,
        ) -> Option<ToolOutput> {
            None
        }
    }

    struct ResourceProbeVerifier {
        live_resources: Arc<AtomicUsize>,
    }

    struct ShutdownProbeVerifier {
        shutdowns: Arc<AtomicUsize>,
    }

    struct FailingCleanupVerifier;

    impl AttemptVerifier for ShutdownProbeVerifier {
        fn verify<'a>(
            &'a mut self,
            _task: &'a Task,
            _attempt: EvalAttempt<'a>,
        ) -> AttemptVerifierFuture<'a> {
            Box::pin(async {
                panic!("shutdown probe verifier must not execute after an earlier failure")
            })
        }

        fn shutdown(&mut self) -> AttemptVerifierCleanupFuture<'_> {
            Box::pin(async move {
                self.shutdowns.fetch_add(1, Ordering::AcqRel);
                CleanupPhase::completed(chrono::Utc::now())
            })
        }
    }

    impl AttemptVerifier for FailingCleanupVerifier {
        fn verify<'a>(
            &'a mut self,
            _task: &'a Task,
            _attempt: EvalAttempt<'a>,
        ) -> AttemptVerifierFuture<'a> {
            Box::pin(async {
                let cleanup_error = std::io::Error::other("deterministic verifier cleanup failure");
                Err(super::AttemptVerificationFailure::new(
                    std::io::Error::other("deterministic verifier primary failure"),
                    CleanupPhase::failed(chrono::Utc::now(), &cleanup_error),
                ))
            })
        }
    }

    impl AttemptVerifier for ResourceProbeVerifier {
        fn verify<'a>(
            &'a mut self,
            _task: &'a Task,
            _attempt: EvalAttempt<'a>,
        ) -> AttemptVerifierFuture<'a> {
            assert_eq!(
                self.live_resources.load(Ordering::Acquire),
                0,
                "attempt-owned agent resources must be joined before verification starts"
            );
            Box::pin(async {
                let cleanup_started = chrono::Utc::now();
                let cleanup_error = std::io::Error::other("deterministic verifier cleanup failure");
                Ok(AttemptVerification {
                    result: VerifierResult {
                        exit_code: 0,
                        rewards: BTreeMap::from([("reward".to_owned(), 1.0)]),
                    },
                    stdout: String::new(),
                    stderr: String::new(),
                    cleanup: CleanupPhase::failed(cleanup_started, &cleanup_error),
                })
            })
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_resources_are_joined_before_attempt_verifier() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let warmup = socket.next().await.unwrap().unwrap();
            assert!(warmup.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp-warmup", "usage": null }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let generation = socket.next().await.unwrap().unwrap();
            assert!(generation.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp-generation",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": "done" }]
                            }],
                            "usage": {
                                "input_tokens": 1,
                                "input_tokens_details": { "cached_tokens": 0 },
                                "output_tokens": 1,
                                "output_tokens_details": { "reasoning_tokens": 0 },
                                "total_tokens": 2
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while socket.next().await.is_some() {}
        });
        let live_resources = Arc::new(AtomicUsize::new(0));
        let openai = OpenAi::builder("test")
            .websocket_url(endpoint)
            .build()
            .unwrap();
        let tool_resources = Arc::clone(&live_resources);
        let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
            tool_resources.fetch_add(1, Ordering::AcqRel);
            Tools::builder()
                .without_defaults()
                .provider(AttemptResourceProvider {
                    live_resources: Arc::clone(&tool_resources),
                })
                .build()
        });
        let output = tempdir().unwrap();
        let verifier_resources = Arc::clone(&live_resources);
        let (evaluator, _events) = Evaluator::builder(nanocodex)
            .output_directory(output.path())
            .attempt_agent(move |_attempt, builder| {
                Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(ResourceProbeVerifier {
                    live_resources: Arc::clone(&verifier_resources),
                }))
            })
            .build()
            .unwrap();
        let task = Task::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
        )
        .unwrap();

        let outcome = evaluator.task(task).await.unwrap();
        let result = outcome
            .scored()
            .expect("the successful verifier must return a scored outcome");

        assert_eq!(result.status, EvalStatus::Passed);
        assert_eq!(result.outcome, EvalOutcome::Passed);
        assert_eq!(result.cleanup.agent.status, CleanupStatus::Completed);
        assert_eq!(result.cleanup.verifier.status, CleanupStatus::Failed);
        assert_eq!(live_resources.load(Ordering::Acquire), 0);
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verifier_primary_and_cleanup_failures_are_both_retained() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let warmup = socket.next().await.unwrap().unwrap();
            assert!(warmup.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp-warmup", "usage": null }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let generation = socket.next().await.unwrap().unwrap();
            assert!(generation.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp-generation",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": "done" }]
                            }],
                            "usage": {
                                "input_tokens": 1,
                                "input_tokens_details": { "cached_tokens": 0 },
                                "output_tokens": 1,
                                "output_tokens_details": { "reasoning_tokens": 0 },
                                "total_tokens": 2
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while socket.next().await.is_some() {}
        });
        let openai = OpenAi::builder("test")
            .websocket_url(endpoint)
            .build()
            .unwrap();
        let output = tempdir().unwrap();
        let (evaluator, _events) = Evaluator::builder(Nanocodex::builder(openai))
            .output_directory(output.path())
            .attempt_agent(|_attempt, builder| {
                Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(FailingCleanupVerifier))
            })
            .build()
            .unwrap();
        let task = Task::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
        )
        .unwrap();

        let outcome = evaluator.task(task).await.unwrap();
        let failure = outcome
            .unscored()
            .expect("verifier execution failure must be unscored");

        assert_eq!(failure.kind, crate::EvalFailureKind::Verifier);
        assert!(
            failure
                .message
                .contains("deterministic verifier primary failure")
        );
        assert!(
            failure
                .traceback
                .contains("deterministic verifier primary failure")
        );
        assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Failed);
        assert!(
            failure
                .cleanup
                .verifier
                .diagnostic
                .as_ref()
                .is_some_and(|diagnostic| diagnostic
                    .message
                    .contains("deterministic verifier cleanup failure"))
        );
        assert!(failure.timing.verifier.is_some());
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verifier_is_joined_after_post_agent_package_validation_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let warmup = socket.next().await.unwrap().unwrap();
            assert!(warmup.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp-warmup", "usage": null }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let generation = socket.next().await.unwrap().unwrap();
            assert!(generation.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp-generation",
                            "status": "completed",
                            "output": [{
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": "done" }]
                            }],
                            "usage": {
                                "input_tokens": 1,
                                "input_tokens_details": { "cached_tokens": 0 },
                                "output_tokens": 1,
                                "output_tokens_details": { "reasoning_tokens": 0 },
                                "total_tokens": 2
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while socket.next().await.is_some() {}
        });
        let (task_directory, task) = task_with_agent_timeout(5.0);
        let mutation = task_directory.path().join("environment/README.md");
        let openai = OpenAi::builder("test")
            .websocket_url(endpoint)
            .build()
            .unwrap();
        let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
            Tools::builder()
                .without_defaults()
                .provider(PackageMutatingProvider {
                    mutation: mutation.clone(),
                })
                .build()
        });
        let output = tempdir().unwrap();
        let verifier_shutdowns = Arc::new(AtomicUsize::new(0));
        let verifier_shutdowns_for_attempt = Arc::clone(&verifier_shutdowns);
        let (evaluator, _events) = Evaluator::builder(nanocodex)
            .output_directory(output.path())
            .attempt_agent(move |_attempt, builder| {
                Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(ShutdownProbeVerifier {
                    shutdowns: Arc::clone(&verifier_shutdowns_for_attempt),
                }))
            })
            .build()
            .unwrap();

        let outcome = evaluator.task(task).await.unwrap();
        let failure = outcome
            .unscored()
            .expect("post-agent package mutation must be returned as unscored");

        assert_eq!(failure.kind, crate::EvalFailureKind::Environment);
        assert_eq!(failure.cleanup.agent.status, CleanupStatus::Completed);
        assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Completed);
        assert_eq!(verifier_shutdowns.load(Ordering::Acquire), 1);
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn agent_resources_are_joined_after_execution_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let warmup = socket.next().await.unwrap().unwrap();
            assert!(warmup.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp-warmup", "usage": null }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let generation = socket.next().await.unwrap().unwrap();
            assert!(generation.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.failed",
                        "response": {
                            "id": "resp-failed",
                            "status": "failed",
                            "error": {
                                "code": "cyber_policy",
                                "message": "deterministic safety refusal"
                            }
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            while socket.next().await.is_some() {}
        });
        let live_resources = Arc::new(AtomicUsize::new(0));
        let openai = OpenAi::builder("test")
            .websocket_url(endpoint)
            .build()
            .unwrap();
        let tool_resources = Arc::clone(&live_resources);
        let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
            tool_resources.fetch_add(1, Ordering::AcqRel);
            Tools::builder()
                .without_defaults()
                .provider(AttemptResourceProvider {
                    live_resources: Arc::clone(&tool_resources),
                })
                .build()
        });
        let output = tempdir().unwrap();
        let verifier_shutdowns = Arc::new(AtomicUsize::new(0));
        let verifier_shutdowns_for_attempt = Arc::clone(&verifier_shutdowns);
        let (evaluator, _events) = Evaluator::builder(nanocodex)
            .output_directory(output.path())
            .attempt_agent(move |_attempt, builder| {
                Ok::<_, Infallible>(AttemptAgent::new(builder).verifier(ShutdownProbeVerifier {
                    shutdowns: Arc::clone(&verifier_shutdowns_for_attempt),
                }))
            })
            .build()
            .unwrap();
        let task = Task::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
        )
        .unwrap();

        let outcome = evaluator
            .task(task)
            .await
            .expect("an accepted provider failure must return a terminal outcome");

        assert_eq!(live_resources.load(Ordering::Acquire), 0);
        let failure = outcome
            .unscored()
            .expect("the provider failure must be retained as unscored");
        assert_eq!(failure.outcome, EvalOutcome::SafetyRefusal);
        assert_eq!(failure.kind, crate::EvalFailureKind::AgentSafetyRefusal);
        assert_eq!(failure.cleanup.agent.status, CleanupStatus::Completed);
        assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Completed);
        assert_eq!(verifier_shutdowns.load(Ordering::Acquire), 1);
        assert!(failure.cleanup.agent.timing.is_some());
        assert!(failure.timing.agent_execution.is_some());
        assert!(failure.timing.queue_wait.finished_at >= failure.timing.queue_wait.started_at);
        let agent = failure
            .agent
            .as_ref()
            .expect("terminal run metrics must survive the provider failure");
        assert_eq!(agent.metadata.status, AgentStatus::Failed);
        assert_eq!(agent.billing_completeness, BillingCompleteness::Unknown);
        server.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn verifier_is_joined_after_attempt_readiness_failure() {
        let output = tempdir().unwrap();
        let verifier_shutdowns = Arc::new(AtomicUsize::new(0));
        let verifier_shutdowns_for_attempt = Arc::clone(&verifier_shutdowns);
        let nanocodex = Nanocodex::builder(OpenAi::new("test").unwrap());
        let (evaluator, _events) = Evaluator::builder(nanocodex)
            .output_directory(output.path())
            .attempt_agent(move |_attempt, builder| {
                Ok::<_, Infallible>(
                    AttemptAgent::new(builder)
                        .ready(async {
                            Err(std::io::Error::other(
                                "deterministic attempt readiness failure",
                            ))
                        })
                        .verifier(ShutdownProbeVerifier {
                            shutdowns: Arc::clone(&verifier_shutdowns_for_attempt),
                        }),
                )
            })
            .build()
            .unwrap();
        let task = Task::load(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting"),
        )
        .unwrap();

        let outcome = evaluator.task(task).await.unwrap();
        let failure = outcome
            .unscored()
            .expect("readiness failure must be returned as unscored");

        assert_eq!(failure.kind, crate::EvalFailureKind::Environment);
        assert_eq!(failure.cleanup.agent.status, CleanupStatus::NotRequired);
        assert_eq!(failure.cleanup.verifier.status, CleanupStatus::Completed);
        assert_eq!(verifier_shutdowns.load(Ordering::Acquire), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn model_in_flight_timeout_retains_unknown_billing_snapshot() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = accept_async(stream).await.unwrap();
            let warmup = socket.next().await.unwrap().unwrap();
            assert!(warmup.is_text());
            socket
                .send(Message::Text(
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp-warmup", "usage": null }
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let generation = socket.next().await.unwrap().unwrap();
            assert!(generation.is_text());
            while socket.next().await.is_some() {}
        });
        let live_resources = Arc::new(AtomicUsize::new(0));
        let openai = OpenAi::builder("test")
            .websocket_url(endpoint)
            .build()
            .unwrap();
        let tool_resources = Arc::clone(&live_resources);
        let nanocodex = Nanocodex::builder(openai).tools_factory(move |_agent| {
            tool_resources.fetch_add(1, Ordering::AcqRel);
            Tools::builder()
                .without_defaults()
                .provider(AttemptResourceProvider {
                    live_resources: Arc::clone(&tool_resources),
                })
                .build()
        });
        let output = tempdir().unwrap();
        let (evaluator, _events) = Evaluator::builder(nanocodex)
            .output_directory(output.path())
            .build()
            .unwrap();
        let (_task_directory, task) = task_with_agent_timeout(0.05);

        let outcome = evaluator
            .task(task)
            .await
            .expect("an accepted timeout must return a terminal outcome");

        assert_eq!(live_resources.load(Ordering::Acquire), 0);
        let failure = outcome
            .unscored()
            .expect("the timeout must be retained as unscored");
        assert_eq!(failure.outcome, EvalOutcome::AgentTimeout);
        assert_eq!(failure.cleanup.agent.status, CleanupStatus::Completed);
        let agent = failure
            .agent
            .as_ref()
            .expect("cancellation must emit a partial terminal snapshot");
        assert_eq!(agent.metadata.status, AgentStatus::Cancelled);
        assert_eq!(agent.billing_completeness, BillingCompleteness::Unknown);
        server.await.unwrap();
    }

    #[test]
    fn completed_model_call_leaves_idle_tool_timeout_billing_complete() {
        let mut observation = AgentObservation::default();
        observation.observe_lifecycle(AgentEventKind::ModelCallStarted);
        observation.observe_lifecycle(AgentEventKind::ModelCallCompleted);

        assert_eq!(observation.billable_in_flight, 0);
        assert_eq!(
            observation.billing_completeness(),
            BillingCompleteness::Complete
        );
    }

    #[test]
    fn failed_model_call_marks_billing_snapshot_unknown() {
        let mut observation = AgentObservation::default();
        observation.observe_lifecycle(AgentEventKind::ModelCallStarted);
        observation.observe_lifecycle(AgentEventKind::ModelCallFailed);

        assert_eq!(observation.billable_in_flight, 0);
        assert_eq!(
            observation.billing_completeness(),
            BillingCompleteness::Unknown
        );
    }

    fn task_with_agent_timeout(timeout_seconds: f64) -> (tempfile::TempDir, Task) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../tasks/write-greeting");
        let destination = tempdir().unwrap();
        for directory in ["environment", "tests"] {
            fs::create_dir_all(destination.path().join(directory)).unwrap();
        }
        for file in [
            "instruction.md",
            "environment/Dockerfile",
            "environment/README.md",
            "tests/test.sh",
        ] {
            fs::copy(source.join(file), destination.path().join(file)).unwrap();
        }
        let manifest = fs::read_to_string(source.join("task.toml"))
            .unwrap()
            .replace(
                "timeout_sec = 300.0",
                &format!("timeout_sec = {timeout_seconds}"),
            );
        fs::write(destination.path().join("task.toml"), manifest).unwrap();
        let task = Task::load(destination.path()).unwrap();
        (destination, task)
    }
}

#[cfg(test)]
mod tracing_tests {
    use std::{
        collections::HashMap,
        fs,
        sync::{Arc, Mutex, Once, OnceLock},
        time::Duration,
    };

    use nanocodex_agent::{Nanocodex, NanocodexError, OpenAi, transport::ResponsesError};
    use tempfile::tempdir;
    use tracing::{Id, Instrument, Subscriber, field::Visit, span::Attributes};
    use tracing_subscriber::{
        Layer, layer::Context as LayerContext, prelude::*, registry::LookupSpan,
    };
    use uuid::Uuid;

    use super::{
        AdmissionController, EvalError, Evaluator, SweepCoordinate, failure_kind,
        output_aliases_task_package, trial_name, validate_attempt_environment,
    };
    use crate::{EvalFailureKind, Sweep, Task, native::NativeAttempt, sweep::AgentId};

    #[derive(Clone, Default)]
    struct TraceCapture(Arc<Mutex<HashMap<u64, CapturedSpan>>>);
    static TRACE_CAPTURE: OnceLock<TraceCapture> = OnceLock::new();
    static TRACE_SUBSCRIBER: Once = Once::new();

    struct CapturedSpan {
        name: &'static str,
        parent: Option<u64>,
        fields: HashMap<String, String>,
    }

    struct FieldCapture<'a>(&'a mut HashMap<String, String>);

    fn install_trace_capture() -> TraceCapture {
        let capture = TRACE_CAPTURE.get_or_init(TraceCapture::default).clone();
        TRACE_SUBSCRIBER.call_once(|| {
            tracing::subscriber::set_global_default(
                tracing_subscriber::registry().with(capture.clone()),
            )
            .expect("test process has no pre-existing global tracing subscriber");
            tracing::callsite::rebuild_interest_cache();
        });
        capture
    }

    #[test]
    fn fresh_finite_run_is_bound_before_execution() {
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

        assert!(!eval.resumed());
        assert_eq!(eval.planned_attempts(), Some(2));
        assert_eq!(eval.remaining_attempts(&sweep).unwrap(), 2);
        assert!(eval.directory().join("run.json").is_file());
    }

    #[test]
    fn colliding_task_names_and_immediate_retries_get_distinct_attempt_paths() {
        let tasks = tempdir().unwrap();
        let first = write_named_task(tasks.path(), "first", "one/shared");
        let second = write_named_task(tasks.path(), "second", "two/shared");
        let coordinate = SweepCoordinate {
            agent: AgentId::new("default").unwrap(),
            trial: 1,
        };
        let ids = [
            Uuid::from_u128(0x1234_5678_0000_0000_0000_0000_0000_0001),
            Uuid::from_u128(0x1234_5678_0000_0000_0000_0000_0000_0002),
            Uuid::from_u128(0x1234_5678_0000_0000_0000_0000_0000_0003),
        ];
        let first_name = trial_name(&first, ids[0], Some(&coordinate));
        let second_name = trial_name(&second, ids[1], Some(&coordinate));
        let retry_name = trial_name(&first, ids[2], Some(&coordinate));

        let compact_ids = ids.map(|id| id.simple().to_string());
        assert_eq!(&compact_ids[0][..8], &compact_ids[1][..8]);
        assert_eq!(&compact_ids[0][..8], &compact_ids[2][..8]);
        assert_ne!(first_name, second_name);
        assert_ne!(first_name, retry_name);
        assert_ne!(second_name, retry_name);
        assert!(first_name.ends_with(&compact_ids[0]));
        assert!(second_name.ends_with(&compact_ids[1]));
        assert!(retry_name.ends_with(&compact_ids[2]));

        let output = tempdir().unwrap();
        let first_attempt = NativeAttempt::prepare(output.path(), &first_name, &first).unwrap();
        fs::write(
            first_attempt.paths.workspace.join("abandoned-partial"),
            "partial\n",
        )
        .unwrap();
        let second_attempt = NativeAttempt::prepare(output.path(), &second_name, &second).unwrap();
        let retry_attempt = NativeAttempt::prepare(output.path(), &retry_name, &first).unwrap();

        assert_ne!(first_attempt.paths.root, second_attempt.paths.root);
        assert_ne!(first_attempt.paths.root, retry_attempt.paths.root);
        assert!(
            !retry_attempt
                .paths
                .workspace
                .join("abandoned-partial")
                .exists()
        );
    }

    #[test]
    fn admission_is_work_conserving_within_memory_and_concurrency_limits() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let admission = Arc::new(AdmissionController::new(2, Some(4)));
            let three = admission.acquire(3).await.unwrap();

            assert!(
                tokio::time::timeout(Duration::from_millis(5), admission.acquire(2))
                    .await
                    .is_err()
            );
            let one = tokio::time::timeout(Duration::from_millis(5), admission.acquire(1))
                .await
                .unwrap()
                .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(5), admission.acquire(1))
                    .await
                    .is_err()
            );

            drop(one);
            drop(three);
            let oversized = admission.acquire(10).await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(5), admission.acquire(1))
                    .await
                    .is_err()
            );
            drop(oversized);
        });
    }

    #[test]
    fn draining_closes_admission_without_cancelling_admitted_work() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let admission = Arc::new(AdmissionController::new(1, None));
            let admitted = admission.acquire(1).await.unwrap();
            let waiting = {
                let admission = Arc::clone(&admission);
                tokio::spawn(async move { admission.acquire(1).await })
            };

            assert_eq!(admission.begin_drain(), 1);
            assert_eq!(admission.begin_drain(), 1);
            assert!(waiting.await.unwrap().is_none());

            drop(admitted);
            assert!(admission.acquire(1).await.is_none());
        });
    }

    impl Visit for FieldCapture<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name().to_owned(), value.to_owned());
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.0.insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl<S> Layer<S> for TraceCapture
    where
        S: Subscriber + for<'lookup> LookupSpan<'lookup>,
    {
        fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: LayerContext<'_, S>) {
            let parent = attributes
                .parent()
                .map(|parent| parent.clone().into_u64())
                .or_else(|| {
                    attributes
                        .is_contextual()
                        .then(|| context.current_span().id().map(Id::into_u64))
                        .flatten()
                });
            let mut fields = HashMap::new();
            attributes.record(&mut FieldCapture(&mut fields));
            self.0.lock().unwrap().insert(
                id.clone().into_u64(),
                CapturedSpan {
                    name: attributes.metadata().name(),
                    parent,
                    fields,
                },
            );
        }

        fn on_record(
            &self,
            id: &Id,
            values: &tracing::span::Record<'_>,
            _context: LayerContext<'_, S>,
        ) {
            if let Some(span) = self.0.lock().unwrap().get_mut(&id.clone().into_u64()) {
                values.record(&mut FieldCapture(&mut span.fields));
            }
        }
    }

    #[test]
    fn failed_attempt_does_not_cancel_pending_batch_work() {
        let capture = install_trace_capture();
        let task_root = tempdir().unwrap();
        fs::create_dir(task_root.path().join("tests")).unwrap();
        fs::create_dir(task_root.path().join("environment")).unwrap();
        fs::write(
            task_root.path().join("task.toml"),
            r#"
schema_version = "1.1"
[task]
name = "terminal-bench/traced"
description = "Tracing fixture"
[metadata]
custom_docker_compose = true
[agent]
timeout_sec = 1.0
[verifier]
timeout_sec = 1.0
[environment]
docker_image = "example/traced:latest"
cpus = 1
memory_mb = 128
storage_mb = 128
gpus = 0
allow_internet = false
"#,
        )
        .unwrap();
        fs::write(
            task_root.path().join("instruction.md"),
            "do the traced work\n",
        )
        .unwrap();
        fs::write(task_root.path().join("tests/test.sh"), "exit 0\n").unwrap();
        let task = Task::load(task_root.path()).unwrap();
        assert!(matches!(
            validate_attempt_environment(&task, false),
            Err(EvalError::UnsupportedNativeTask { .. })
        ));
        assert!(validate_attempt_environment(&task, true).is_ok());
        let output = tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let eval_id = runtime.block_on(async {
            let (eval, _events) =
                Evaluator::builder(Nanocodex::builder(OpenAi::new("test").unwrap()))
                    .output_directory(output.path())
                    .build()
                    .unwrap();
            let eval_id = eval.id().to_string();
            let result = eval
                .tasks(vec![task.clone(), task])
                .instrument(tracing::info_span!("test.parent"))
                .await;
            let outcomes = result.expect("accepted failures must remain in the batch result");
            assert_eq!(outcomes.len(), 2);
            assert!(outcomes.iter().all(|outcome| {
                outcome
                    .unscored()
                    .is_some_and(|failure| failure.kind == EvalFailureKind::Environment)
            }));
            eval_id
        });

        let spans = capture.0.lock().unwrap();
        let attempts = spans
            .iter()
            .filter(|(_, span)| {
                span.name == "eval.attempt"
                    && span.fields.get("eval.id").is_some_and(|id| id == &eval_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(attempts.len(), 2);
        for (_, attempt) in &attempts {
            assert!(attempt.parent.is_none());
            assert_eq!(
                attempt.fields.get("status").map(String::as_str),
                Some("failed")
            );
            assert!(attempt.fields.contains_key("duration_ns"));
        }
        let setups = spans
            .values()
            .filter(|span| {
                span.name == "eval.environment.setup"
                    && attempts
                        .iter()
                        .any(|(attempt_id, _)| span.parent == Some(**attempt_id))
            })
            .collect::<Vec<_>>();
        assert_eq!(setups.len(), 2);
        for setup in setups {
            assert!(
                attempts
                    .iter()
                    .any(|(attempt_id, _)| setup.parent == Some(**attempt_id))
            );
            assert_eq!(
                setup.fields.get("status").map(String::as_str),
                Some("failed")
            );
            assert!(setup.fields.contains_key("duration_ns"));
        }
    }

    #[test]
    fn classifies_cyber_policy_as_an_agent_safety_refusal() {
        let error = EvalError::Nanocodex(NanocodexError::Responses(ResponsesError::Api {
            event: r#"{"type":"error","error":{"code":"cyber_policy"}}"#.to_owned(),
        }));

        assert_eq!(failure_kind(&error), EvalFailureKind::AgentSafetyRefusal);
    }

    #[test]
    fn rejects_a_task_package_mutated_after_load_before_attempt_setup() {
        let tasks = tempdir().unwrap();
        let task = write_named_task(tasks.path(), "changed", "terminal-bench/changed");
        fs::write(task.environment_directory().join("late-input"), "changed\n").unwrap();
        let output = tempdir().unwrap();
        let (eval, _) = Evaluator::builder(Nanocodex::builder(OpenAi::new("test").unwrap()))
            .output_directory(output.path())
            .build()
            .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let outcome = runtime.block_on(eval.task(task)).unwrap();
        let failure = outcome
            .unscored()
            .expect("task package mutation must be an unscored attempt");

        assert!(matches!(failure.kind, EvalFailureKind::Environment));
        assert!(
            fs::read_dir(eval.directory())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| !entry.file_type().is_ok_and(|kind| kind.is_dir()))
        );
    }

    #[test]
    fn rejects_finite_output_nested_in_a_hashed_task_package_before_creation() {
        let tasks = tempdir().unwrap();
        let task = write_named_task(tasks.path(), "overlap", "terminal-bench/overlap");
        let output = task.environment_directory().join("retained/evals");
        let sweep = Sweep::builder()
            .task(task)
            .agent(
                "default",
                Nanocodex::builder(OpenAi::new("test-key").unwrap()),
            )
            .unwrap()
            .build()
            .unwrap();

        let result = Evaluator::builder(Nanocodex::builder(OpenAi::new("test-key").unwrap()))
            .output_directory(&output)
            .fresh_run(&sweep)
            .build();

        assert!(matches!(result, Err(EvalError::OutputOverlapsTask { .. })));
        assert!(!output.exists());
    }

    #[cfg(unix)]
    #[test]
    fn detects_an_output_directory_reached_through_a_filesystem_alias() {
        let tasks = tempdir().unwrap();
        let task = write_named_task(tasks.path(), "alias", "terminal-bench/alias");
        let aliases = tempdir().unwrap();
        let alias = aliases.path().join("environment");
        std::os::unix::fs::symlink(task.environment_directory(), &alias).unwrap();

        assert!(output_aliases_task_package(&alias.join("retained/evals"), task.root()).unwrap());
    }

    fn write_named_task(parent: &std::path::Path, directory: &str, name: &str) -> Task {
        let root = parent.join(directory);
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("environment")).unwrap();
        fs::create_dir(root.join("tests")).unwrap();
        fs::write(root.join("instruction.md"), "Do the work.\n").unwrap();
        fs::write(root.join("tests/test.sh"), "exit 0\n").unwrap();
        fs::write(
            root.join("task.toml"),
            format!(
                r#"
schema_version = "1.1"
[task]
name = "{name}"
description = "attempt identity fixture"
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
"#
            ),
        )
        .unwrap();
        Task::load(root).unwrap()
    }
}
