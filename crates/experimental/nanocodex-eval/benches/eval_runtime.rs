use std::{
    fs,
    hint::black_box,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use nanocodex_agent::{
    Nanocodex, OpenAi, Thinking,
    events::{AgentEvent, AgentEventKind},
};
use nanocodex_eval::{
    AggregateDataset, AtifBuilder, AttemptFact, AttemptFactArtifacts, Evaluator, LatencyBreakdown,
    Sweep, Task, harbor::Harbor,
};
use serde_json::{Value, json, value::RawValue};
use uuid::Uuid;

const TRACE_TURNS: usize = 64;
const EVENTS_PER_TURN: usize = 6;
const REPRESENTATIVE_TASK_ENV: &str = "NANOCODEX_EVAL_BENCH_TASK";

fn benchmark_eval_runtime(criterion: &mut Criterion) {
    let tasks_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tasks");
    let task_paths =
        ["extract-todos", "uppercase-message", "write-greeting"].map(|name| tasks_root.join(name));
    let tasks = task_paths
        .iter()
        .map(Task::load)
        .collect::<Result<Vec<_>, _>>()
        .expect("checked-in benchmark tasks");
    let agent = Nanocodex::builder(OpenAi::new("benchmark-only").expect("static API key"))
        .instructions(
            "Work directly in the provided workspace. Complete the requested task, \
             verify your changes, and keep the final answer concise.",
        )
        .thinking(Thinking::Medium);
    let sweep = build_sweep(&tasks, &agent);

    let mut group = criterion.benchmark_group("eval_runtime");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(3));

    group.bench_function("load_terminal_bench_task", |bencher| {
        bencher.iter(|| {
            black_box(Task::load(black_box(&task_paths[2])).expect("load benchmark task"));
        });
    });

    group.bench_function("validate_checked_in_smoke_task_package", |bencher| {
        bencher.iter(|| {
            black_box(&tasks[2])
                .validate_package()
                .expect("validate benchmark task");
        });
    });

    // Point this at a retained TB2.1 package for the release performance gate.
    // Keeping it opt-in avoids presenting the tiny checked-in smoke task as a
    // representative package-size measurement.
    if let Some(path) = std::env::var_os(REPRESENTATIVE_TASK_ENV).map(PathBuf::from) {
        let bytes = packaged_file_bytes(&path);
        let task = Task::load(&path).unwrap_or_else(|error| {
            panic!(
                "{REPRESENTATIVE_TASK_ENV}={} is not a loadable task: {error}",
                path.display()
            )
        });
        group.throughput(Throughput::Bytes(bytes));
        group.bench_function(
            "validate_representative_terminal_bench_task_package",
            |bencher| {
                bencher.iter(|| {
                    black_box(&task)
                        .validate_package()
                        .expect("validate representative benchmark task");
                });
            },
        );
        group.throughput(Throughput::Bytes(bytes.saturating_mul(4)));
        group.bench_function("validate_representative_harbor_identity_stack", |bencher| {
            bencher.iter(|| {
                Harbor::validate_task_package(black_box(&task))
                    .expect("validate representative Harbor task identity");
            });
        });
    }

    group.throughput(Throughput::Elements(sweep.attempt_count() as u64));
    group.bench_function("plan_3x4x5_sweep", |bencher| {
        bencher.iter(|| {
            black_box(build_sweep(black_box(&tasks), black_box(&agent)));
        });
    });

    let output = tempfile::tempdir().expect("benchmark output");
    let (initial, events) = Evaluator::builder(agent.clone())
        .output_directory(output.path())
        .resume_incomplete(&sweep)
        .build()
        .expect("initialize resumable job");
    drop(events);
    drop(initial);
    group.throughput(Throughput::Elements(1));
    group.bench_function("reopen_incomplete_job", |bencher| {
        bencher.iter(|| {
            let (evaluator, events) = Evaluator::builder(agent.clone())
                .output_directory(output.path())
                .resume_incomplete(&sweep)
                .build()
                .expect("resume benchmark job");
            black_box(evaluator.directory());
            drop(events);
            drop(evaluator);
        });
    });

    let trace = representative_trace();
    group.throughput(Throughput::Elements(trace.len() as u64));
    group.bench_function("project_384_events_to_atif", |bencher| {
        bencher.iter(|| {
            let mut builder = AtifBuilder::default();
            for event in black_box(&trace) {
                builder.apply(event).expect("project typed event");
            }
            black_box(builder);
        });
    });

    let facts = representative_attempt_facts();
    group.throughput(Throughput::Elements(facts.len() as u64));
    group.bench_function("aggregate_60_plot_facts", |bencher| {
        bencher.iter(|| black_box(AggregateDataset::new(black_box(facts.clone()))));
    });
    group.finish();
}

fn packaged_file_bytes(root: &Path) -> u64 {
    const FILES: [&str; 3] = ["task.toml", "instruction.md", "README.md"];
    const DIRECTORIES: [&str; 4] = ["environment", "tests", "solution", "steps"];

    FILES
        .into_iter()
        .map(|name| file_bytes(&root.join(name)))
        .chain(
            DIRECTORIES
                .into_iter()
                .map(|name| directory_file_bytes(&root.join(name))),
        )
        .fold(0_u64, u64::saturating_add)
}

fn directory_file_bytes(directory: &Path) -> u64 {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(error) => panic!(
            "read representative benchmark directory {}: {error}",
            directory.display()
        ),
    };
    entries
        .map(|entry| {
            let path = entry
                .unwrap_or_else(|error| panic!("read benchmark directory entry: {error}"))
                .path();
            if path.is_dir() {
                directory_file_bytes(&path)
            } else {
                file_bytes(&path)
            }
        })
        .fold(0_u64, u64::saturating_add)
}

fn file_bytes(path: &Path) -> u64 {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => 0,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!(
            "read representative benchmark file {}: {error}",
            path.display()
        ),
    }
}

fn representative_attempt_facts() -> Vec<AttemptFact> {
    let mut facts = Vec::new();
    for configuration in ["medium-defaults", "medium-web", "high-defaults", "high-web"] {
        for task in ["extract-todos", "uppercase-message", "write-greeting"] {
            for repetition in 1..=5 {
                let directory =
                    PathBuf::from(format!("retained/{configuration}/{task}/{repetition}"));
                facts.push(AttemptFact {
                    attempt_id: Uuid::now_v7(),
                    task_name: task.to_owned(),
                    configuration: configuration.to_owned(),
                    repetition,
                    passed: (repetition + u16::from(configuration.starts_with("high"))) % 3 != 0,
                    cost_usd: Some(0.05 + f64::from(repetition) / 100.0),
                    latency: LatencyBreakdown {
                        queue_wait_ns: u64::from(repetition) * 10_000_000,
                        vm_bootstrap_ns: 90_000_000,
                        agent_execution_ns: 8_000_000_000,
                        model_ns: 7_500_000_000,
                        tool_work_ns: 100_000_000,
                        tool_wall_ns: 120_000_000,
                        verifier_ns: 40_000_000,
                        total_ns: 8_250_000_000,
                        ..LatencyBreakdown::default()
                    },
                    artifacts: AttemptFactArtifacts {
                        trajectory: directory.join("agent/trajectory.json"),
                        verifier_output: directory.join("verifier/test-stdout.txt"),
                        directory,
                    },
                });
            }
        }
    }
    facts
}

fn build_sweep(tasks: &[Task], agent: &nanocodex_agent::NanocodexBuilder) -> nanocodex_eval::Sweep {
    Sweep::builder()
        .tasks(tasks.to_vec())
        .trials(5)
        .agent("medium-defaults", agent.clone())
        .expect("valid agent identity")
        .agent("medium-web", agent.clone())
        .expect("valid agent identity")
        .agent("high-defaults", agent.clone())
        .expect("valid agent identity")
        .agent("high-web", agent.clone())
        .expect("valid agent identity")
        .build()
        .expect("valid benchmark sweep")
}

fn representative_trace() -> Vec<AgentEvent> {
    let message =
        "Retained agent output with identifiers, paths, and verification evidence. ".repeat(24);
    let reasoning = "Inspect the workspace, make the focused change, and verify it. ".repeat(8);
    let mut events = Vec::with_capacity(TRACE_TURNS * EVENTS_PER_TURN);
    let mut sequence = 0_u64;
    for call_index in 0..TRACE_TURNS {
        push_event(
            &mut events,
            &mut sequence,
            AgentEventKind::ModelCallStarted,
            &json!({
                "call_index": call_index,
                "model": "gpt-5.6-sol",
                "effort": "medium"
            }),
        );
        push_event(
            &mut events,
            &mut sequence,
            AgentEventKind::ReasoningSummaryDelta,
            &json!({"model_call_index": call_index, "text": reasoning.as_str()}),
        );
        push_event(
            &mut events,
            &mut sequence,
            AgentEventKind::AssistantMessage,
            &json!({"model_call_index": call_index, "text": message.as_str()}),
        );
        push_event(
            &mut events,
            &mut sequence,
            AgentEventKind::ModelCallCompleted,
            &json!({
                "call_index": call_index,
                "model": "gpt-5.6-sol",
                "attempt": 1,
                "connection_generation": 1,
                "duration_ns": 1_000_000_000_u64,
                "time_to_first_event_ns": 200_000_000_u64,
                "time_to_first_output_ns": 400_000_000_u64,
                "tool_calls": 1,
                "usage": {
                    "input_tokens": 10_000,
                    "input_tokens_details": {
                        "cached_tokens": 9_000,
                        "cache_write_tokens": 0
                    },
                    "output_tokens": 1_000,
                    "output_tokens_details": {"reasoning_tokens": 600},
                    "total_tokens": 11_000
                }
            }),
        );
        let call_id = format!("call-{call_index}");
        push_event(
            &mut events,
            &mut sequence,
            AgentEventKind::ToolCall,
            &json!({
                "call_id": call_id.as_str(),
                "tool": "exec",
                "arguments": {"cmd": "cargo test --workspace"},
                "model_call_index": call_index
            }),
        );
        push_event(
            &mut events,
            &mut sequence,
            AgentEventKind::ToolResult,
            &json!({
                "call_id": call_id.as_str(),
                "status": "completed",
                "duration_ns": 250_000_000_u64,
                "result": {"stdout": "all representative tests passed", "exit_code": 0}
            }),
        );
    }
    events
}

fn push_event(
    events: &mut Vec<AgentEvent>,
    sequence: &mut u64,
    kind: AgentEventKind,
    payload: &Value,
) {
    *sequence += 1;
    let payload = RawValue::from_string(payload.to_string()).expect("valid raw event payload");
    events.push(AgentEvent {
        protocol_version: 1,
        request_id: Arc::from("019c0000-0000-7000-8000-000000000001"),
        seq: *sequence,
        kind,
        payload: Arc::from(payload),
    });
}

criterion_group!(benches, benchmark_eval_runtime);
criterion_main!(benches);
