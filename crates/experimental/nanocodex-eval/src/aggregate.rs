//! Stable plot-ready facts derived from retained evaluation attempts.

use std::{cmp::Ordering, collections::BTreeMap, path::PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::{EvalResult, EvalStatus, SweepAttemptResult};

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
    /// Verifier-derived success state.
    pub passed: bool,
    /// Estimated total USD cost, including warmup when reported.
    pub cost_usd: Option<f64>,
    /// Exact phase measurements available for this attempt.
    pub latency: LatencyBreakdown,
    /// Retained attempt and trajectory locations.
    pub artifacts: AttemptFactArtifacts,
}

/// Plot-relevant latency phases in nanoseconds.
#[derive(Clone, Debug, Default, Serialize)]
pub struct LatencyBreakdown {
    /// Time waiting for scheduler admission.
    pub queue_wait_ns: u64,
    /// Cold image resolution and construction attributed to this attempt.
    pub cold_image_ns: u64,
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
    /// Attempt wall time.
    pub total_ns: u64,
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
    /// One point per configuration.
    pub configurations: Vec<ConfigurationAggregate>,
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
        Self::from_result(attempt.agent().as_str(), attempt.trial(), attempt.result())
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
            passed: result.status == EvalStatus::Passed,
            cost_usd: result.agent.cost_usd,
            latency: LatencyBreakdown {
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
                total_ns: duration(result.timing.started_at, result.timing.finished_at),
                ..LatencyBreakdown::default()
            },
            artifacts: AttemptFactArtifacts {
                directory: result.artifacts.directory.clone(),
                trajectory: result.artifacts.directory.join("agent/trajectory.json"),
                verifier_output: result.artifacts.verifier_output.clone(),
            },
        }
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
            schema_version: 1,
            attempts,
            configurations,
        }
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
            cost_usd: MetricSummary::new(attempts.iter().filter_map(|attempt| attempt.cost_usd)),
            latency_seconds: MetricSummary::new(
                attempts
                    .iter()
                    .map(|attempt| attempt.latency.total_ns as f64 / 1_000_000_000.0),
            ),
            pass_at_k,
            tasks: tasks
                .into_iter()
                .map(|(task_name, attempts)| TaskAggregate {
                    task_name,
                    attempt_ids: attempts.iter().map(|attempt| attempt.attempt_id).collect(),
                    success: RateEstimate::new(&attempts),
                    cost_usd: MetricSummary::new(
                        attempts.iter().filter_map(|attempt| attempt.cost_usd),
                    ),
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
        let samples = attempts.len();
        let successes = attempts.iter().filter(|attempt| attempt.passed).count();
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
    let Some(minimum) = tasks.values().map(Vec::len).min() else {
        return BTreeMap::new();
    };
    (1..=u16::try_from(minimum).unwrap_or(u16::MAX))
        .map(|k| {
            let estimate = tasks
                .values()
                .map(|attempts| {
                    let n = attempts.len() as u64;
                    let correct = attempts.iter().filter(|attempt| attempt.passed).count() as u64;
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
            passed,
            cost_usd: Some(f64::from(repetition)),
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
}
