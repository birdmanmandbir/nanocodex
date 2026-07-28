use std::{collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{AgentId, Task};

/// Execution environment used for one evaluation attempt.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalEnvironment {
    /// Disposable workspace and verifier processes run directly on the host.
    #[default]
    Native,
    /// Agent tools and verification run in a retained libkrun microVM.
    MicroVm,
}

/// Terminal score classification for one attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStatus {
    /// Every verifier reward was positive.
    Passed,
    /// At least one verifier reward was zero or negative.
    Failed,
}

/// Stable classification for an attempt that could not produce a score.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalFailureKind {
    /// The model provider rejected the attempt for a safety policy.
    AgentSafetyRefusal,
    /// Agent authentication failed.
    AgentAuthentication,
    /// Agent execution exceeded the task deadline.
    AgentTimeout,
    /// Verifier execution exceeded its deadline.
    VerifierTimeout,
    /// Agent setup or execution failed.
    Agent,
    /// Verifier setup or execution failed.
    Verifier,
    /// Attempt workspace or environment setup failed.
    Environment,
    /// The evaluation runtime violated an internal invariant.
    Internal,
}

/// Typed terminal output for an errored or refused attempt.
#[derive(Clone, Debug, Serialize)]
pub struct EvalFailure {
    /// `UUIDv7` identity shared with the attempt's agent session.
    pub attempt_id: Uuid,
    /// Stable task name from the task manifest.
    pub task_name: String,
    /// Filesystem-safe unique trial name.
    pub trial_name: String,
    /// Stable failure classification.
    pub kind: EvalFailureKind,
    /// Human-readable error message.
    pub message: String,
    /// Complete formatted error chain.
    pub traceback: String,
    /// Model selected for the failed attempt.
    pub model: String,
    /// Reasoning effort selected for the failed attempt.
    pub effort: String,
    /// Execution environment selected for the failed attempt.
    pub environment: EvalEnvironment,
    /// Time at which the attempt began.
    pub started_at: DateTime<Utc>,
    /// Time at which the failure was classified.
    pub occurred_at: DateTime<Utc>,
    /// Retained attempt artifact paths.
    pub artifacts: EvalArtifacts,
    #[serde(skip)]
    pub(crate) task: Task,
}

/// Typed result returned by [`crate::Evaluator::task`].
#[derive(Clone, Debug, Serialize)]
pub struct EvalResult {
    /// `UUIDv7` identity shared with the attempt's agent session.
    pub attempt_id: Uuid,
    /// Stable task name from the task manifest.
    pub task_name: String,
    /// Filesystem-safe unique trial name.
    pub trial_name: String,
    /// Verifier-derived pass/fail classification.
    pub status: EvalStatus,
    /// Execution environment used by this attempt.
    pub environment: EvalEnvironment,
    /// Typed terminal agent output and usage.
    pub agent: AgentResult,
    /// Verifier exit code and component rewards.
    pub verifier: VerifierResult,
    /// Attempt phase timestamps.
    pub timing: EvalTiming,
    /// Retained attempt artifact paths.
    pub artifacts: EvalArtifacts,
    #[serde(skip)]
    pub(crate) task: Task,
}

/// Results from an advanced task-by-agent-by-trial sweep.
#[derive(Clone, Debug, Serialize)]
pub struct SweepResults {
    attempts: Vec<SweepAttemptResult>,
    skipped: usize,
}

/// One self-identifying result in a [`SweepResults`] collection.
#[derive(Clone, Debug, Serialize)]
pub struct SweepAttemptResult {
    agent: AgentId,
    trial: u16,
    result: EvalResult,
}

impl SweepResults {
    pub(crate) const fn new(attempts: Vec<SweepAttemptResult>, skipped: usize) -> Self {
        Self { attempts, skipped }
    }

    /// Returns attempts in stable task × agent × trial order.
    #[must_use]
    pub fn attempts(&self) -> &[SweepAttemptResult] {
        &self.attempts
    }

    /// Returns the number of already-completed attempts skipped while resuming.
    #[must_use]
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    /// Consumes the sweep and discards its coordinate wrappers.
    #[must_use]
    pub fn into_results(self) -> Vec<EvalResult> {
        self.attempts
            .into_iter()
            .map(|attempt| attempt.result)
            .collect()
    }
}

impl SweepAttemptResult {
    pub(crate) const fn new(agent: AgentId, trial: u16, result: EvalResult) -> Self {
        Self {
            agent,
            trial,
            result,
        }
    }

    /// Returns the task name for this coordinate.
    #[must_use]
    pub fn task_name(&self) -> &str {
        &self.result.task_name
    }

    /// Returns the caller-defined agent recipe identity.
    #[must_use]
    pub const fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// Returns the one-based trial number.
    #[must_use]
    pub const fn trial(&self) -> u16 {
        self.trial
    }

    /// Returns the typed attempt result.
    #[must_use]
    pub const fn result(&self) -> &EvalResult {
        &self.result
    }

    /// Consumes the coordinate wrapper and returns its attempt result.
    #[must_use]
    pub fn into_result(self) -> EvalResult {
        self.result
    }
}

impl EvalResult {
    /// The immutable task definition used by this attempt.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }
}

impl EvalFailure {
    /// The immutable task definition used by this attempt.
    #[must_use]
    pub const fn task(&self) -> &Task {
        &self.task
    }
}

impl EvalFailureKind {
    /// Harbor's exception class for this terminal failure.
    #[must_use]
    pub const fn harbor_exception_type(self) -> &'static str {
        match self {
            Self::AgentSafetyRefusal => "AgentSafetyRefusalError",
            Self::AgentAuthentication => "AgentAuthenticationError",
            Self::AgentTimeout => "AgentTimeoutError",
            Self::VerifierTimeout => "VerifierTimeoutError",
            Self::Agent => "AgentError",
            Self::Verifier => "VerifierError",
            Self::Environment => "EnvironmentError",
            Self::Internal => "NanocodexEvalError",
        }
    }
}

/// Terminal agent output and aggregate runtime metadata.
#[derive(Clone, Debug, Serialize)]
pub struct AgentResult {
    /// Final assistant message.
    pub final_message: String,
    /// Model that produced the terminal result.
    pub model: String,
    /// Reasoning effort used by the agent.
    pub effort: String,
    /// Logical model-call count.
    pub model_calls: u32,
    /// Tool-call count.
    pub tool_calls: u32,
    /// Aggregate provider usage, excluding warmup.
    pub usage: UsageTotals,
    /// Estimated aggregate USD cost when provider usage can be priced.
    pub cost_usd: Option<f64>,
    /// Complete typed terminal event metadata.
    pub metadata: AgentMetadata,
}

/// Typed metadata emitted by Nanocodex's terminal event.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentMetadata {
    /// Agent lifecycle terminal status.
    pub status: AgentStatus,
    /// Selected model.
    pub model: String,
    /// Selected reasoning effort.
    pub effort: String,
    /// Responses transport spelling.
    pub transport: String,
    /// Agent orchestration spelling.
    pub orchestration: String,
    /// Millisecond duration retained for JSONL compatibility.
    pub duration_ms: u64,
    /// Exact measured duration in nanoseconds.
    pub duration_ns: u64,
    /// Logical model calls.
    pub model_calls: u32,
    /// Steering messages accepted during the attempt.
    pub steers: u32,
    /// Context compactions completed during the attempt.
    pub compactions: u32,
    /// Tool calls executed during the attempt.
    pub tool_calls: u32,
    /// Responses connection attempts.
    pub connection_attempts: u32,
    /// Successful WebSocket replacements.
    pub websocket_reconnects: u32,
    /// Complete Responses transport attempts.
    pub response_attempts: u32,
    /// Retried Responses attempts.
    pub response_retries: u32,
    /// Time spent connecting to the Responses API.
    pub connection_duration_ns: u64,
    /// Time spent in owned retry backoff.
    pub retry_backoff_duration_ns: u64,
    /// Time spent waiting on model calls.
    pub model_duration_ns: u64,
    /// Time spent priming the prompt cache.
    pub warmup_duration_ns: u64,
    /// Sum of actual tool execution time.
    pub tool_work_duration_ns: u64,
    /// Tool wall time including overlap and scheduling.
    pub tool_wall_duration_ns: u64,
    /// Aggregate provider usage for task execution.
    pub usage: UsageTotals,
    /// Provider usage consumed by cache warmup.
    pub warmup_usage: UsageTotals,
    #[serde(default, rename = "last_response_id", skip_serializing)]
    _last_response_id: Option<String>,
    /// Estimated USD cost from provider usage and the built-in pricing catalog.
    pub cost_usd: Option<f64>,
    /// Stable explanation of whether cost is available.
    pub cost_status: String,
}

/// Terminal state reported by the agent lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    /// The agent completed normally.
    Completed,
    /// The agent failed.
    Failed,
    /// The attempt was cancelled.
    Cancelled,
}

/// Aggregate provider token usage.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct UsageTotals {
    /// Total input tokens.
    pub input_tokens: u64,
    /// Input tokens served from provider cache.
    pub cached_input_tokens: u64,
    /// Provider-reported cache-write input tokens.
    pub cache_write_input_tokens: u64,
    /// Total output tokens.
    pub output_tokens: u64,
    /// Provider-reported reasoning output tokens.
    pub reasoning_output_tokens: u64,
    /// Total input plus output tokens.
    pub total_tokens: u64,
}

/// Terminal output from the task verifier.
#[derive(Clone, Debug, Serialize)]
pub struct VerifierResult {
    /// Process exit code.
    pub exit_code: i32,
    /// Named verifier rewards in deterministic key order.
    pub rewards: BTreeMap<String, f64>,
}

/// Wall-clock boundaries for each attempt phase.
#[derive(Clone, Debug, Serialize)]
pub struct EvalTiming {
    /// Time at which attempt setup began.
    pub started_at: DateTime<Utc>,
    /// Time at which the terminal result became durable.
    pub finished_at: DateTime<Utc>,
    /// Interval spent waiting for scheduler admission.
    pub queue_wait: PhaseTiming,
    /// Disposable environment preparation interval.
    pub environment_setup: PhaseTiming,
    /// Attempt backend readiness interval, including VM boot and guest handshake.
    pub environment_readiness: PhaseTiming,
    /// Agent construction interval.
    pub agent_setup: PhaseTiming,
    /// Agent execution interval.
    pub agent_execution: PhaseTiming,
    /// Verifier execution interval.
    pub verifier: PhaseTiming,
}

/// UTC start and finish timestamps for one attempt phase.
#[derive(Clone, Debug, Serialize)]
pub struct PhaseTiming {
    /// Phase start.
    pub started_at: DateTime<Utc>,
    /// Phase finish.
    pub finished_at: DateTime<Utc>,
}

/// Durable paths retained for an attempt.
#[derive(Clone, Debug, Serialize)]
pub struct EvalArtifacts {
    /// Attempt root containing all retained files.
    pub directory: PathBuf,
    /// Disposable workspace presented to the agent.
    pub workspace: PathBuf,
    /// Captured verifier output file.
    pub verifier_output: PathBuf,
}
