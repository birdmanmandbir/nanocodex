//! Stable plot-ready facts derived from retained evaluation attempts.

use std::{cmp::Ordering, collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::{
    BillingCompleteness, EvalAttemptOutcome, EvalFailure, EvalOutcome, EvalResult,
    SweepAttemptResult,
};

/// One self-contained attempt row used by aggregate and plotting consumers.
#[derive(Clone, Debug, Serialize)]
pub struct AttemptFact {
    /// Stable attempt identity used to navigate to retained evidence.
    pub attempt_id: Uuid,
    /// Dataset task identity.
    pub task_name: String,
    /// Caller-defined configuration identity.
    pub configuration: String,
    /// One-based repetition number.
    pub repetition: u16,
    /// One-based deterministic execution position within this task's sweep.
    pub schedule_ordinal: u64,
    /// Semantic attempt outcome.
    pub outcome: EvalOutcome,
    /// Whether this row contributes to score denominators.
    pub scored: bool,
    /// Verifier-derived success state.
    pub passed: bool,
    /// Whether score retention was accompanied by a cleanup failure.
    pub cleanup_failed: bool,
    /// Estimated total USD cost, including warmup when reported.
    pub cost_usd: Option<f64>,
    /// Whether provider billing is known to be terminal.
    pub billing_completeness: Option<BillingCompleteness>,
    /// Exact phase measurements available for this attempt.
    pub latency: LatencyBreakdown,
    /// Retained attempt and trajectory locations.
    pub artifacts: AttemptFactArtifacts,
}

/// Plot-relevant latency phases in nanoseconds.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LatencyBreakdown {
    /// One-time task-group scheduler admission, attributed to its first
    /// executed coordinate.
    pub task_environment_admission_ns: u64,
    /// Coordinate wait after task-environment boot.
    pub queue_wait_ns: u64,
    /// One-time retained task-environment factory boot, attributed to its first
    /// executed coordinate.
    pub task_environment_boot_ns: u64,
    /// Cold image resolution and construction attributed to this attempt.
    pub cold_image_ns: u64,
    /// Disposable environment materialization.
    pub environment_setup_ns: u64,
    /// Attempt backend construction, boot, and readiness.
    pub environment_readiness_ns: u64,
    /// VM materialization, boot, and readiness.
    pub vm_bootstrap_ns: u64,
    /// Agent setup.
    pub agent_setup_ns: u64,
    /// Complete warm agent execution.
    pub agent_execution_ns: u64,
    /// Sum of model waits.
    pub model_ns: u64,
    /// Sum of actual tool work.
    pub tool_work_ns: u64,
    /// Tool wall time including overlap.
    pub tool_wall_ns: u64,
    /// Verifier execution.
    pub verifier_ns: u64,
    /// Explicit agent and verifier cleanup.
    pub cleanup_ns: u64,
    /// Sum of disjoint measured phases.
    pub total_ns: u64,
    /// End-to-end wall time containing every attributed phase.
    pub observed_wall_ns: u64,
}

/// Paths that connect one plot point back to exact evidence.
#[derive(Clone, Debug, Serialize)]
pub struct AttemptFactArtifacts {
    /// Retained attempt directory.
    pub directory: PathBuf,
    /// Canonical ATIF trajectory.
    pub trajectory: PathBuf,
    /// Verifier output.
    pub verifier_output: PathBuf,
}

/// Stable aggregate export consumed by plot renderers and notebooks.
#[derive(Clone, Debug, Serialize)]
pub struct AggregateDataset {
    /// Schema version for the stable JSON export.
    pub schema_version: u32,
    /// Complete source rows; aggregates never replace drilldown evidence.
    pub attempts: Vec<AttemptFact>,
    /// Run-level cold preparation that is shared rather than attributed to an
    /// arbitrary attempt.
    pub run_timing: Option<AggregateRunTiming>,
    /// One point per configuration.
    pub configurations: Vec<ConfigurationAggregate>,
}

/// Run-level latency that cannot be assigned honestly to one attempt.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct AggregateRunTiming {
    /// Image resolution, rootfs preparation, and shared verifier cache setup.
    pub cold_image_and_cache_ns: u64,
}

/// Plot-ready summary for one configuration.
#[derive(Clone, Debug, Serialize)]
pub struct ConfigurationAggregate {
    /// Caller-defined configuration identity.
    pub configuration: String,
    /// Attempt IDs contributing to this point.
    pub attempt_ids: Vec<Uuid>,
    /// Pass-rate estimate and Wilson interval.
    pub success: RateEstimate,
    /// Estimated cost distribution.
    pub cost_usd: MetricSummary,
    /// Total latency distribution in seconds.
    pub latency_seconds: MetricSummary,
    /// Unbiased pass-at-k estimates supported by every task.
    pub pass_at_k: BTreeMap<u16, f64>,
    /// Attempts excluded from score denominators.
    pub unscored_attempts: usize,
    /// Attempts with an explicit cleanup failure.
    pub cleanup_failures: usize,
    /// Attempts whose potentially billable provider usage is not terminal.
    pub billing_unknown_attempts: usize,
    /// Per-task distributions for deeper drilldown.
    pub tasks: Vec<TaskAggregate>,
}

/// Per-task contribution to a configuration point.
#[derive(Clone, Debug, Serialize)]
pub struct TaskAggregate {
    /// Dataset task identity.
    pub task_name: String,
    /// Attempt IDs contributing to this task distribution.
    pub attempt_ids: Vec<Uuid>,
    /// Pass-rate estimate and Wilson interval.
    pub success: RateEstimate,
    /// Attempts excluded from score denominators.
    pub unscored_attempts: usize,
    /// Attempts with an explicit cleanup failure.
    pub cleanup_failures: usize,
    /// Attempts whose potentially billable provider usage is not terminal.
    pub billing_unknown_attempts: usize,
    /// Estimated cost distribution.
    pub cost_usd: MetricSummary,
    /// Total latency distribution in seconds.
    pub latency_seconds: MetricSummary,
}

/// Binomial success estimate with an approximate 95% Wilson interval.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct RateEstimate {
    /// Successful attempts.
    pub successes: usize,
    /// Total attempts.
    pub samples: usize,
    /// Observed success fraction.
    pub rate: f64,
    /// Lower Wilson bound.
    pub confidence_low: f64,
    /// Upper Wilson bound.
    pub confidence_high: f64,
}

/// Exact sample count plus common distribution summaries.
#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct MetricSummary {
    /// Number of known values.
    pub samples: usize,
    /// Minimum value.
    pub minimum: Option<f64>,
    /// Median value.
    pub median: Option<f64>,
    /// Arithmetic mean.
    pub mean: Option<f64>,
    /// Maximum value.
    pub maximum: Option<f64>,
}

impl AttemptFact {
    /// Builds a plot fact from one successful or verifier-failed sweep result.
    #[must_use]
    pub fn from_sweep_attempt(attempt: &SweepAttemptResult) -> Self {
        Self::from_outcome(attempt.agent().as_str(), attempt.trial(), attempt.outcome())
    }

    /// Builds a plot fact from a complete typed terminal attempt output.
    #[must_use]
    pub fn from_outcome(
        configuration: &str,
        repetition: u16,
        outcome: &EvalAttemptOutcome,
    ) -> Self {
        match outcome {
            EvalAttemptOutcome::Scored(result) => {
                Self::from_result(configuration, repetition, result)
            }
            EvalAttemptOutcome::Unscored(failure) => {
                Self::from_failure(configuration, repetition, failure)
            }
        }
    }

    /// Builds a plot fact from one typed result and explicit coordinates.
    #[must_use]
    pub fn from_result(configuration: &str, repetition: u16, result: &EvalResult) -> Self {
        let duration = |started: DateTime<Utc>, finished: DateTime<Utc>| {
            u64::try_from(
                finished
                    .signed_duration_since(started)
                    .num_nanoseconds()
                    .unwrap_or_default()
                    .max(0),
            )
            .unwrap_or(u64::MAX)
        };
        Self {
            attempt_id: result.attempt_id,
            task_name: result.task_name.clone(),
            configuration: configuration.to_owned(),
            repetition,
            schedule_ordinal: result.schedule_ordinal,
            outcome: result.outcome,
            scored: result.outcome.is_scored(),
            passed: result.outcome.is_passed(),
            cleanup_failed: result.cleanup.is_failed(),
            cost_usd: result.agent.cost_usd,
            billing_completeness: Some(result.agent.billing_completeness),
            latency: LatencyBreakdown {
                task_environment_admission_ns: result
                    .timing
                    .task_environment_boot
                    .as_ref()
                    .map_or(0, |boot| {
                        duration(result.timing.started_at, boot.started_at)
                    }),
                queue_wait_ns: duration(
                    result.timing.queue_wait.started_at,
                    result.timing.queue_wait.finished_at,
                ),
                task_environment_boot_ns: result
                    .timing
                    .task_environment_boot
                    .as_ref()
                    .map_or(0, |timing| duration(timing.started_at, timing.finished_at)),
                environment_setup_ns: duration(
                    result.timing.environment_setup.started_at,
                    result.timing.environment_setup.finished_at,
                ),
                environment_readiness_ns: duration(
                    result.timing.environment_readiness.started_at,
                    result.timing.environment_readiness.finished_at,
                ),
                vm_bootstrap_ns: if result.environment == crate::EvalEnvironment::MicroVm {
                    result
                        .timing
                        .task_environment_boot
                        .as_ref()
                        .map_or(0, |timing| duration(timing.started_at, timing.finished_at))
                        .saturating_add(duration(
                            result.timing.environment_readiness.started_at,
                            result.timing.environment_readiness.finished_at,
                        ))
                } else {
                    0
                },
                agent_setup_ns: duration(
                    result.timing.agent_setup.started_at,
                    result.timing.agent_setup.finished_at,
                ),
                agent_execution_ns: duration(
                    result.timing.agent_execution.started_at,
                    result.timing.agent_execution.finished_at,
                ),
                model_ns: result.agent.metadata.model_duration_ns,
                tool_work_ns: result.agent.metadata.tool_work_duration_ns,
                tool_wall_ns: result.agent.metadata.tool_wall_duration_ns,
                verifier_ns: duration(
                    result.timing.verifier.started_at,
                    result.timing.verifier.finished_at,
                ),
                cleanup_ns: [&result.cleanup.agent, &result.cleanup.verifier]
                    .into_iter()
                    .filter_map(|cleanup| cleanup.timing.as_ref())
                    .map(|timing| duration(timing.started_at, timing.finished_at))
                    .sum(),
                observed_wall_ns: duration(result.timing.started_at, result.timing.finished_at),
                ..LatencyBreakdown::default()
            },
            artifacts: AttemptFactArtifacts {
                directory: result.artifacts.directory.clone(),
                trajectory: result.artifacts.directory.join("agent/trajectory.json"),
                verifier_output: result.artifacts.verifier_output.clone(),
            },
        }
        .with_total()
    }

    /// Builds a plot fact from one typed unscored terminal failure.
    #[must_use]
    pub fn from_failure(configuration: &str, repetition: u16, failure: &EvalFailure) -> Self {
        let duration_between = |started: DateTime<Utc>, finished: DateTime<Utc>| {
            u64::try_from(
                finished
                    .signed_duration_since(started)
                    .num_nanoseconds()
                    .unwrap_or_default()
                    .max(0),
            )
            .unwrap_or(u64::MAX)
        };
        let duration = |timing: Option<&crate::PhaseTiming>| {
            timing.map_or(0, |timing| {
                duration_between(timing.started_at, timing.finished_at)
            })
        };
        let agent = failure.agent.as_ref();
        Self {
            attempt_id: failure.attempt_id,
            task_name: failure.task_name.clone(),
            configuration: configuration.to_owned(),
            repetition,
            schedule_ordinal: failure.schedule_ordinal,
            outcome: failure.outcome,
            scored: false,
            passed: false,
            cleanup_failed: failure.cleanup.is_failed(),
            cost_usd: agent.and_then(|agent| agent.cost_usd),
            billing_completeness: agent.map(|agent| agent.billing_completeness),
            latency: LatencyBreakdown {
                task_environment_admission_ns: failure
                    .timing
                    .task_environment_boot
                    .as_ref()
                    .map_or(0, |boot| {
                        duration_between(failure.started_at, boot.started_at)
                    }),
                queue_wait_ns: duration(Some(&failure.timing.queue_wait)),
                task_environment_boot_ns: duration(failure.timing.task_environment_boot.as_ref()),
                environment_setup_ns: duration(failure.timing.environment_setup.as_ref()),
                environment_readiness_ns: duration(failure.timing.environment_readiness.as_ref()),
                vm_bootstrap_ns: if failure.environment == crate::EvalEnvironment::MicroVm {
                    duration(failure.timing.task_environment_boot.as_ref())
                        .saturating_add(duration(failure.timing.environment_readiness.as_ref()))
                } else {
                    0
                },
                agent_setup_ns: duration(failure.timing.agent_setup.as_ref()),
                agent_execution_ns: duration(failure.timing.agent_execution.as_ref()),
                model_ns: agent.map_or(0, |agent| agent.metadata.model_duration_ns),
                tool_work_ns: agent.map_or(0, |agent| agent.metadata.tool_work_duration_ns),
                tool_wall_ns: agent.map_or(0, |agent| agent.metadata.tool_wall_duration_ns),
                verifier_ns: duration(failure.timing.verifier.as_ref()),
                cleanup_ns: [&failure.cleanup.agent, &failure.cleanup.verifier]
                    .into_iter()
                    .filter_map(|cleanup| cleanup.timing.as_ref())
                    .map(|timing| duration(Some(timing)))
                    .sum(),
                observed_wall_ns: duration_between(failure.started_at, failure.occurred_at),
                ..LatencyBreakdown::default()
            },
            artifacts: AttemptFactArtifacts {
                directory: failure.artifacts.directory.clone(),
                trajectory: failure.artifacts.directory.join("agent/trajectory.json"),
                verifier_output: failure.artifacts.verifier_output.clone(),
            },
        }
        .with_total()
    }

    const fn with_total(mut self) -> Self {
        let latency = &self.latency;
        self.latency.total_ns = latency
            .task_environment_admission_ns
            .saturating_add(latency.queue_wait_ns)
            .saturating_add(latency.task_environment_boot_ns)
            .saturating_add(latency.cold_image_ns)
            .saturating_add(latency.environment_setup_ns)
            .saturating_add(latency.environment_readiness_ns)
            .saturating_add(latency.agent_setup_ns)
            .saturating_add(latency.agent_execution_ns)
            .saturating_add(latency.verifier_ns)
            .saturating_add(latency.cleanup_ns);
        self
    }
}

impl AggregateDataset {
    /// Builds deterministic plot points while retaining every source row.
    #[must_use]
    pub fn new(attempts: Vec<AttemptFact>) -> Self {
        let mut groups = BTreeMap::<String, Vec<&AttemptFact>>::new();
        for attempt in &attempts {
            groups
                .entry(attempt.configuration.clone())
                .or_default()
                .push(attempt);
        }
        let configurations = groups
            .into_iter()
            .map(|(configuration, attempts)| ConfigurationAggregate::new(configuration, &attempts))
            .collect();
        Self {
            schema_version: 4,
            attempts,
            run_timing: None,
            configurations,
        }
    }

    /// Attaches run-level cold image and shared cache preparation latency.
    #[must_use]
    pub const fn with_run_timing(mut self, run_timing: AggregateRunTiming) -> Self {
        self.run_timing = Some(run_timing);
        self
    }

    /// Builds an aggregate directly from typed sweep results.
    #[must_use]
    pub fn from_sweep(attempts: &[SweepAttemptResult]) -> Self {
        Self::new(
            attempts
                .iter()
                .map(AttemptFact::from_sweep_attempt)
                .collect(),
        )
    }
}

impl ConfigurationAggregate {
    fn new(configuration: String, attempts: &[&AttemptFact]) -> Self {
        let mut tasks = BTreeMap::<String, Vec<&AttemptFact>>::new();
        for attempt in attempts {
            tasks
                .entry(attempt.task_name.clone())
                .or_default()
                .push(*attempt);
        }
        let pass_at_k = pass_at_k(&tasks);
        Self {
            configuration,
            attempt_ids: attempts.iter().map(|attempt| attempt.attempt_id).collect(),
            success: RateEstimate::new(attempts),
            cost_usd: MetricSummary::new(attempts.iter().filter_map(|attempt| {
                (attempt.billing_completeness == Some(BillingCompleteness::Complete))
                    .then_some(attempt.cost_usd)
                    .flatten()
            })),
            latency_seconds: MetricSummary::new(
                attempts
                    .iter()
                    .map(|attempt| attempt.latency.total_ns as f64 / 1_000_000_000.0),
            ),
            pass_at_k,
            unscored_attempts: attempts.iter().filter(|attempt| !attempt.scored).count(),
            cleanup_failures: attempts
                .iter()
                .filter(|attempt| attempt.cleanup_failed)
                .count(),
            billing_unknown_attempts: attempts
                .iter()
                .filter(|attempt| {
                    attempt.billing_completeness == Some(BillingCompleteness::Unknown)
                })
                .count(),
            tasks: tasks
                .into_iter()
                .map(|(task_name, attempts)| TaskAggregate {
                    task_name,
                    attempt_ids: attempts.iter().map(|attempt| attempt.attempt_id).collect(),
                    success: RateEstimate::new(&attempts),
                    unscored_attempts: attempts.iter().filter(|attempt| !attempt.scored).count(),
                    cleanup_failures: attempts
                        .iter()
                        .filter(|attempt| attempt.cleanup_failed)
                        .count(),
                    billing_unknown_attempts: attempts
                        .iter()
                        .filter(|attempt| {
                            attempt.billing_completeness == Some(BillingCompleteness::Unknown)
                        })
                        .count(),
                    cost_usd: MetricSummary::new(attempts.iter().filter_map(|attempt| {
                        (attempt.billing_completeness == Some(BillingCompleteness::Complete))
                            .then_some(attempt.cost_usd)
                            .flatten()
                    })),
                    latency_seconds: MetricSummary::new(
                        attempts
                            .iter()
                            .map(|attempt| attempt.latency.total_ns as f64 / 1_000_000_000.0),
                    ),
                })
                .collect(),
        }
    }
}

impl RateEstimate {
    fn new(attempts: &[&AttemptFact]) -> Self {
        let samples = attempts.iter().filter(|attempt| attempt.scored).count();
        let successes = attempts
            .iter()
            .filter(|attempt| attempt.scored && attempt.passed)
            .count();
        if samples == 0 {
            return Self {
                successes,
                samples,
                rate: 0.0,
                confidence_low: 0.0,
                confidence_high: 0.0,
            };
        }
        let n = samples as f64;
        let rate = successes as f64 / n;
        let z = 1.959_963_984_540_054;
        let denominator = 1.0 + z * z / n;
        let center = (rate + z * z / (2.0 * n)) / denominator;
        let margin = z * ((rate * (1.0 - rate) + z * z / (4.0 * n)) / n).sqrt() / denominator;
        Self {
            successes,
            samples,
            rate,
            confidence_low: (center - margin).max(0.0),
            confidence_high: (center + margin).min(1.0),
        }
    }
}

impl MetricSummary {
    fn new(values: impl IntoIterator<Item = f64>) -> Self {
        let mut values = values
            .into_iter()
            .filter(|value| value.is_finite())
            .collect::<Vec<_>>();
        values.sort_by(|left, right| left.partial_cmp(right).unwrap_or(Ordering::Equal));
        let samples = values.len();
        if samples == 0 {
            return Self::default();
        }
        let median = if samples % 2 == 0 {
            (values[samples / 2 - 1] + values[samples / 2]) / 2.0
        } else {
            values[samples / 2]
        };
        Self {
            samples,
            minimum: values.first().copied(),
            median: Some(median),
            mean: Some(values.iter().sum::<f64>() / samples as f64),
            maximum: values.last().copied(),
        }
    }
}

fn pass_at_k(tasks: &BTreeMap<String, Vec<&AttemptFact>>) -> BTreeMap<u16, f64> {
    let Some(minimum) = tasks
        .values()
        .map(|attempts| attempts.iter().filter(|attempt| attempt.scored).count())
        .min()
    else {
        return BTreeMap::new();
    };
    (1..=u16::try_from(minimum).unwrap_or(u16::MAX))
        .map(|k| {
            let estimate = tasks
                .values()
                .map(|attempts| {
                    let n = attempts.iter().filter(|attempt| attempt.scored).count() as u64;
                    let correct = attempts
                        .iter()
                        .filter(|attempt| attempt.scored && attempt.passed)
                        .count() as u64;
                    if n - correct < u64::from(k) {
                        return 1.0;
                    }
                    1.0 - (0..u64::from(k)).fold(1.0, |product, index| {
                        product * (n - correct - index) as f64 / (n - index) as f64
                    })
                })
                .sum::<f64>()
                / tasks.len() as f64;
            (k, estimate)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(configuration: &str, task: &str, repetition: u16, passed: bool) -> AttemptFact {
        AttemptFact {
            attempt_id: Uuid::now_v7(),
            task_name: task.to_owned(),
            configuration: configuration.to_owned(),
            repetition,
            schedule_ordinal: u64::from(repetition),
            outcome: if passed {
                EvalOutcome::Passed
            } else {
                EvalOutcome::VerifierFailed
            },
            scored: true,
            passed,
            cleanup_failed: false,
            cost_usd: Some(f64::from(repetition)),
            billing_completeness: Some(BillingCompleteness::Complete),
            latency: LatencyBreakdown {
                total_ns: u64::from(repetition) * 1_000_000_000,
                ..LatencyBreakdown::default()
            },
            artifacts: AttemptFactArtifacts {
                directory: PathBuf::from("attempt"),
                trajectory: PathBuf::from("attempt/agent/trajectory.json"),
                verifier_output: PathBuf::from("attempt/verifier/test-stdout.txt"),
            },
        }
    }

    #[test]
    fn builds_plot_points_with_exact_drilldown_and_pass_at_k() {
        let dataset = AggregateDataset::new(vec![
            fact("medium", "a", 1, true),
            fact("medium", "a", 2, false),
            fact("medium", "b", 1, false),
            fact("medium", "b", 2, false),
        ]);
        let point = &dataset.configurations[0];
        assert_eq!(point.attempt_ids.len(), 4);
        assert_eq!(point.success.successes, 1);
        assert_eq!(point.success.samples, 4);
        assert_eq!(point.cost_usd.median, Some(1.5));
        assert_eq!(point.latency_seconds.mean, Some(1.5));
        assert_eq!(point.tasks.len(), 2);
        assert!((point.pass_at_k[&1] - 0.25).abs() < f64::EPSILON);
        assert!((point.pass_at_k[&2] - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn excludes_infrastructure_errors_without_hiding_cleanup_failures() {
        let passed = fact("medium", "a", 1, true);
        let mut infrastructure = fact("medium", "a", 2, false);
        infrastructure.outcome = EvalOutcome::InfrastructureError;
        infrastructure.scored = false;
        infrastructure.cleanup_failed = true;

        let dataset = AggregateDataset::new(vec![passed, infrastructure]);
        let point = &dataset.configurations[0];

        assert_eq!(point.success.samples, 1);
        assert_eq!(point.success.successes, 1);
        assert_eq!(point.unscored_attempts, 1);
        assert_eq!(point.cleanup_failures, 1);
        assert_eq!(point.tasks[0].success.samples, 1);
        assert_eq!(point.tasks[0].cleanup_failures, 1);
    }

    #[test]
    fn excludes_partial_cost_when_billing_is_unknown() {
        let known = fact("medium", "a", 1, true);
        let mut partial = fact("medium", "a", 2, false);
        partial.outcome = EvalOutcome::AgentTimeout;
        partial.scored = false;
        partial.billing_completeness = Some(BillingCompleteness::Unknown);

        let dataset = AggregateDataset::new(vec![known, partial]);
        let point = &dataset.configurations[0];

        assert_eq!(point.cost_usd.samples, 1);
        assert_eq!(point.cost_usd.mean, Some(1.0));
        assert_eq!(point.billing_unknown_attempts, 1);
        assert_eq!(point.tasks[0].cost_usd.samples, 1);
        assert_eq!(point.tasks[0].billing_unknown_attempts, 1);
        assert_eq!(dataset.attempts[1].cost_usd, Some(2.0));
    }
}
