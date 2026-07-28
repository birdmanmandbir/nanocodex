use std::{
    error::Error,
    ffi::OsString,
    fs,
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
    AgentId, AgentMetadata, AgentResult, EvalArtifacts, EvalEnvironment, EvalEvent, EvalEventKind,
    EvalEvents, EvalFailure, EvalFailureKind, EvalResult, EvalStatus, EvalTiming, PhaseTiming,
    Sweep, SweepAttemptResult, SweepResults, Task, TaskLoadError, VerifierResult,
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

type AttemptVerifierFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AttemptVerification, AttemptError>> + Send + 'a>>;
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
}

/// Complete typed output returned by an attempt-owned verifier.
pub struct AttemptVerification {
    /// Process-equivalent exit status and named rewards.
    pub result: VerifierResult,
    /// Complete captured verifier standard output.
    pub stdout: String,
    /// Complete captured verifier standard error.
    pub stderr: String,
}

struct AttemptInput {
    task: Task,
    nanocodex: NanocodexBuilder,
    coordinate: Option<SweepCoordinate>,
    queued_at: DateTime<Utc>,
}

struct AttemptOutput {
    result: EvalResult,
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
    /// Returns an error when setup, the agent, or verification fails.
    pub async fn task(&self, task: Task) -> Result<EvalResult, EvalError> {
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
        .map(|output| output.result)
    }

    /// Runs `count` fresh attempts of the same immutable task.
    ///
    /// Results preserve attempt order even when work completes out of order.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
    pub async fn task_n(&self, task: Task, count: usize) -> Result<Vec<EvalResult>, EvalError> {
        self.tasks(std::iter::repeat_n(task, count).collect()).await
    }

    /// Runs one independent attempt for every task in `tasks`.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
    pub async fn tasks(&self, tasks: Vec<Task>) -> Result<Vec<EvalResult>, EvalError> {
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
            .map(|output| output.result)
            .collect())
    }

    /// Runs `count` fresh attempts for every task in `tasks`.
    ///
    /// Results are grouped in input task order and then trial order.
    ///
    /// # Errors
    ///
    /// Returns the first setup, agent, or verifier error.
    pub async fn tasks_n(
        &self,
        tasks: Vec<Task>,
        count: usize,
    ) -> Result<Vec<EvalResult>, EvalError> {
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
    /// Returns the first setup, agent, or verifier error.
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
                    output.result,
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
        let mut first_error = None;
        while let Some(output) = completed.next().await {
            let Some((index, result)) = output else {
                continue;
            };
            match result {
                Ok(result) => results.push((index, result)),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
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
                queue_wait,
                &mut emitter,
            )
            .instrument(span.clone())
            .await;
        record_attempt_result(&span, trace_started, &result);
        if let Err(error) = &result {
            emitter.emit(EvalEventKind::Failed(Box::new(attempt_failure(
                self, attempt_id, task, trial_name, started_at, error,
            ))));
        }
        result.map(|result| AttemptOutput { result, coordinate })
    }

    async fn run_task_inner(
        &self,
        task: Task,
        nanocodex: NanocodexBuilder,
        attempt_id: Uuid,
        trial_name: String,
        queue_wait: PhaseTiming,
        emitter: &mut AttemptEmitter<'_>,
    ) -> Result<EvalResult, EvalError> {
        reject_output_overlap(self.inner.job.parent_directory(), task.root())?;
        task.validate_package()?;
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
            result?
        };
        emitter.emit(EvalEventKind::AttemptStarted {
            prompt: task.prompt().to_owned(),
            workspace: attempt.paths.workspace.clone(),
        });
        let mut agent = self
            .execute_agent(emitter, &task, &attempt, nanocodex)
            .await?;

        task.validate_package()?;
        emitter.emit(EvalEventKind::VerifierStarted);
        let verifier = self
            .execute_verifier(&task, &attempt, agent.verifier.take())
            .await?;
        task.validate_package()?;
        emitter.emit(EvalEventKind::VerifierOutput {
            stdout: verifier.stdout.clone(),
            stderr: verifier.stderr.clone(),
        });
        emitter.emit(EvalEventKind::VerifierCompleted(verifier.result.clone()));

        let result = EvalResult {
            attempt_id,
            task_name: task.name().to_owned(),
            trial_name,
            status: verifier_status(&verifier.result),
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
    ) -> Result<VerifierExecution, EvalError> {
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
                let execution = verifier
                    .verify(
                        task,
                        EvalAttempt {
                            task,
                            directory: &attempt.paths.root,
                            workspace: &attempt.paths.workspace,
                        },
                    )
                    .await
                    .map_err(EvalError::AttemptVerifier)?;
                Ok(VerifierExecution {
                    result: execution.result,
                    timing: PhaseTiming::finished(started_at),
                    stdout: execution.stdout,
                    stderr: execution.stderr,
                })
            } else {
                attempt.verify(task).await
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
    ) -> Result<AgentExecution, EvalError> {
        let AgentSetup {
            agent,
            mut events,
            verifier,
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
            let event_result = timeout(task.agent_timeout(), async {
                loop {
                    let event = events.recv().await.ok_or(EvalError::AgentEventsClosed)?;
                    let terminal = event.kind.is_terminal();
                    emitter.emit(EvalEventKind::Agent(event.clone()));
                    if terminal {
                        let result = turn.result().await?;
                        return Ok::<_, EvalError>((result, event));
                    }
                }
            })
            .await;
            if let Ok(result) = event_result {
                result
            } else {
                let _ = control.cancel().await;
                Err(EvalError::AgentTimeout(task.agent_timeout()))
            }
        };
        let result = result.instrument(span.clone()).await;
        record_span_result(&span, trace_started, &result);
        let (turn_result, terminal_event) = result?;
        drop(agent);
        Ok(AgentExecution {
            result: AgentResult::from_terminal(turn_result.into_final_message(), &terminal_event)?,
            verifier,
            readiness_timing,
            setup_timing,
            execution_timing: PhaseTiming::finished(execution_started),
        })
    }

    async fn setup_agent(
        &self,
        emitter: &AttemptEmitter<'_>,
        task: &Task,
        attempt: &NativeAttempt,
        nanocodex: NanocodexBuilder,
    ) -> Result<AgentSetup, EvalError> {
        let setup_started = Utc::now();
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
                factory(
                    EvalAttempt {
                        task,
                        directory: &attempt.paths.root,
                        workspace: &attempt.paths.workspace,
                    },
                    builder,
                )
                .map_err(EvalError::AttemptAgent)?
            } else {
                AttemptAgent::new(builder)
            };
            let (builder, readiness, verifier) = configured.into_parts();
            let readiness_started = Utc::now();
            if let Some(readiness) = readiness {
                readiness.await.map_err(EvalError::AttemptAgent)?;
            }
            let readiness_timing = PhaseTiming::finished(readiness_started);
            let (agent, events) = builder.build()?;
            Ok::<_, EvalError>((agent, events, verifier, readiness_timing))
        }
        .instrument(span.clone())
        .await;
        record_span_result(&span, trace_started, &result);
        let (agent, events, verifier, readiness_timing) = result?;
        Ok(AgentSetup {
            agent,
            events,
            verifier,
            readiness_timing,
            timing: PhaseTiming::finished(setup_started),
        })
    }
}

struct AgentExecution {
    result: AgentResult,
    verifier: Option<Box<dyn AttemptVerifier>>,
    readiness_timing: PhaseTiming,
    setup_timing: PhaseTiming,
    execution_timing: PhaseTiming,
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
    error: ResponsesApiError,
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
    error: &EvalError,
) -> EvalFailure {
    let root = eval.directory().join(&trial_name);
    EvalFailure {
        attempt_id,
        task_name: task.name().to_owned(),
        trial_name,
        kind: failure_kind(error),
        message: error.to_string(),
        traceback: error_traceback(error),
        model: MODEL.to_owned(),
        effort: "unknown".to_owned(),
        environment: eval.attempt_environment(),
        started_at,
        occurred_at: Utc::now(),
        artifacts: EvalArtifacts {
            workspace: root.join("workspace"),
            verifier_output: root.join("verifier/test-stdout.txt"),
            directory: root,
        },
        task,
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
        .and_then(|event| event.error.code)
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

fn record_attempt_result(span: &Span, started_at: Instant, result: &Result<EvalResult, EvalError>) {
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
        Err(error) => {
            span.record("status", "failed");
            span.record("otel.status_code", "ERROR");
            span.record("error.message", tracing::field::display(error));
            span.in_scope(|| {
                info!(
                    target: "nanocodex_eval",
                    duration_ns,
                    error = %error,
                    "evaluation attempt failed"
                );
            });
        }
    }
}

fn record_attempt_success(span: &Span, result: &EvalResult) {
    let usage = &result.agent.usage;
    let warmup = &result.agent.metadata.warmup_usage;
    span.record("status", "completed");
    span.record("otel.status_code", "OK");
    span.record("eval.score.status", eval_status(result.status));
    span.record(
        "eval.reward.total",
        result.verifier.rewards.values().sum::<f64>(),
    );
    span.record("agent.model_calls", result.agent.model_calls);
    span.record("agent.tool_calls", result.agent.tool_calls);
    span.record(
        "agent.response_attempts",
        result.agent.metadata.response_attempts,
    );
    span.record(
        "agent.response_retries",
        result.agent.metadata.response_retries,
    );
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
        result.agent.metadata.warmup_duration_ns,
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
    if let Some(cost_usd) = result.agent.cost_usd {
        span.record("cost.usd", cost_usd);
    }
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
    fn from_terminal(final_message: String, event: &AgentEvent) -> Result<Self, EvalError> {
        if event.kind != AgentEventKind::RunCompleted {
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
            metadata,
        })
    }
}

impl PhaseTiming {
    fn finished(started_at: DateTime<Utc>) -> Self {
        Self {
            started_at,
            finished_at: Utc::now(),
        }
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
    use crate::{
        EvalFailureKind, Sweep, Task, TaskLoadError, native::NativeAttempt, sweep::AgentId,
    };

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
            assert!(matches!(
                result,
                Err(EvalError::UnsupportedNativeTask { .. })
            ));
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

        let error = runtime.block_on(eval.task(task)).unwrap_err();

        assert!(matches!(
            error,
            EvalError::TaskPackage(TaskLoadError::ContentChanged { .. })
        ));
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
