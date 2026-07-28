use std::{hint::black_box, path::PathBuf, sync::Arc, time::Duration};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use nanocodex_agent::{
    Nanocodex, OpenAi, Thinking,
    events::{AgentEvent, AgentEventKind},
};
use nanocodex_eval::{AtifBuilder, Evaluator, Sweep, Task};
use serde_json::{Value, json, value::RawValue};

const TRACE_TURNS: usize = 64;
const EVENTS_PER_TURN: usize = 6;

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
    group.finish();
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
