use criterion::{criterion_group, criterion_main};

#[allow(dead_code, unused_imports)]
#[path = "../src/tui/actions.rs"]
mod actions_impl;
#[path = "../src/subagents/mod.rs"]
mod subagents;
#[allow(dead_code, unused_imports)]
#[path = "../src/tui/theme.rs"]
pub(crate) mod theme_impl;
use theme_impl as theme;
#[allow(dead_code, unused_imports)]
#[path = "../src/tui/context_diagnostics.rs"]
mod context_diagnostics;
#[allow(dead_code, unused_imports)]
#[path = "../src/tui/floating.rs"]
mod floating;
#[allow(dead_code, unused_imports)]
#[path = "../src/tui/keybindings.rs"]
mod keybindings;
#[allow(dead_code, unused_imports)]
#[path = "../src/tui/spinner.rs"]
mod spinner;
#[allow(dead_code, unused_imports)]
#[path = "../src/tui/subagents.rs"]
mod tui_subagents_impl;
#[allow(dead_code, unused_imports)]
#[path = "../src/tui/waved_text.rs"]
mod waved_text;

mod tui {
    use std::{cell::Cell, hint::black_box, rc::Rc, sync::Arc};

    use criterion::{BatchSize, BenchmarkId, Criterion, Throughput};
    use ratatui::{
        Terminal, TerminalOptions, Viewport,
        backend::{CrosstermBackend, TestBackend},
        layout::Rect,
    };

    pub(crate) use crate::theme_impl as theme;
    use crate::{
        actions_impl as actions, context_diagnostics, floating, keybindings, spinner,
        tui_subagents_impl as subagents, waved_text,
    };

    #[allow(dead_code, unused_imports)]
    mod markdown {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/markdown.rs"));
    }

    #[allow(dead_code, unused_imports)]
    mod diff {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/diff.rs"));
    }

    #[allow(dead_code, unused_imports)]
    mod transcript {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/tui/transcript.rs"
        ));
    }

    #[allow(dead_code, unused_imports)]
    mod composer {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/composer.rs"));
    }

    #[allow(dead_code, unused_imports)]
    mod selection {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/selection.rs"));
    }

    #[allow(dead_code, unused_imports)]
    mod app {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/app.rs"));
    }

    #[allow(dead_code, unused_imports)]
    mod view {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/view.rs"));
    }

    #[allow(dead_code, unused_imports)]
    mod terminal {
        include!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/tui/terminal.rs"));
    }

    use app::App;
    use terminal::{ByteCountingWriter, DrawMetrics, MeasuredBackend};
    use transcript::{ToolStatus, TranscriptItem};

    const WIDTH: u16 = 120;
    const HEIGHT: u16 = 40;
    const HISTORY_TURNS: usize = 384;
    const STREAM_DELTAS: usize = 256;
    const BRANCHES: usize = 32;

    type OutputTerminal = Terminal<MeasuredBackend<CrosstermBackend<ByteCountingWriter<Vec<u8>>>>>;

    fn sized_text(words: usize, salt: usize) -> String {
        const TOKENS: [&str; 8] = [
            "workspace",
            "compile",
            "stream",
            "response",
            "tool",
            "branch",
            "λ",
            "🦀",
        ];
        let mut text = String::with_capacity(words.saturating_mul(9));
        for index in 0..words {
            if index > 0 {
                text.push(if (index + salt).is_multiple_of(23) {
                    '\n'
                } else {
                    ' '
                });
            }
            text.push_str(TOKENS[(index + salt) % TOKENS.len()]);
        }
        text
    }

    fn long_history_app() -> App {
        let mut app = App::new("/workspace/nanocodex".into());
        for turn in 0..HISTORY_TURNS {
            if turn.is_multiple_of(8) {
                app.main.transcript.push_editable_user(
                    format!("request {turn}: {}", sized_text(18, turn)),
                    u64::try_from(turn + 1).expect("fixture prompt id should fit in u64"),
                );
            }
            app.main
                .transcript
                .push(TranscriptItem::Assistant(sized_text(36, turn + 1)));
            if turn.is_multiple_of(3) {
                let call_id = format!("call-{turn}");
                app.main.transcript.push(TranscriptItem::Tool {
                    call_id: call_id.clone(),
                    name: "exec_command".to_owned(),
                    arguments: format!("cargo test -p fixture-{turn}"),
                    status: ToolStatus::Completed,
                });
                assert!(app.main.transcript.set_tool_result(
                    &call_id,
                    ToolStatus::Completed,
                    Some(1_000_000),
                    Some("exit 0".to_owned()),
                ));
            }
        }
        app.main
            .transcript
            .push(TranscriptItem::Assistant("stream tail".to_owned()));
        app
    }

    fn branch_tree_app() -> App {
        let mut app = App::new("/workspace/nanocodex".into());
        app.main
            .transcript
            .push_editable_user("root branch prompt".to_owned(), 1);

        for branch in 1..=BRANCHES {
            app.move_up();
            assert!(app.start_historical_edit());
            app.replace_input(format!("branch {branch}: revise the selected prompt"));
            let request = app
                .commit_historical_edit()
                .expect("fixture branch edit should commit");
            let prompt = app
                .main_branch_opened(
                    request.new_branch,
                    request.source_branch,
                    request.prompt,
                    Arc::from(format!("branch-session-{branch}")),
                )
                .expect("fixture branch should open");
            app.main.transcript.push_editable_user(
                prompt,
                u64::try_from(branch + 1).expect("fixture prompt id should fit in u64"),
            );
            app.main
                .transcript
                .push(TranscriptItem::Assistant(sized_text(24, branch)));
        }
        assert!(app.toggle_branch_navigator());
        assert_eq!(app.branch_previews().len(), BRANCHES + 1);
        app
    }

    fn output_terminal() -> (OutputTerminal, Rc<Cell<u64>>) {
        let output_bytes = Rc::new(Cell::new(0));
        let writer = ByteCountingWriter {
            inner: Vec::new(),
            bytes: Rc::clone(&output_bytes),
        };
        let terminal = Terminal::with_options(
            MeasuredBackend::new(CrosstermBackend::new(writer)),
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, WIDTH, HEIGHT)),
            },
        )
        .expect("output benchmark terminal should initialize");
        (terminal, output_bytes)
    }

    fn measured_draw(
        app: &mut App,
        terminal: &mut OutputTerminal,
        output_bytes: &Cell<u64>,
    ) -> DrawMetrics {
        let bytes_before = output_bytes.get();
        terminal.backend_mut().changed_cells = 0;
        terminal
            .draw(|frame| view::render(frame, app))
            .expect("measured benchmark frame should render");
        DrawMetrics {
            changed_cells: terminal.backend().changed_cells,
            output_bytes: output_bytes.get().saturating_sub(bytes_before),
        }
    }

    fn stream_burst(app: &mut App) {
        for delta in 0..STREAM_DELTAS {
            assert!(app.main.transcript.append_assistant_delta(&format!(
                "\nstreamed row {delta}: deterministic incremental response"
            )));
        }
    }

    fn sample_stream_metrics() -> DrawMetrics {
        let mut app = long_history_app();
        let (mut terminal, output_bytes) = output_terminal();
        let _ = measured_draw(&mut app, &mut terminal, output_bytes.as_ref());
        stream_burst(&mut app);
        measured_draw(&mut app, &mut terminal, output_bytes.as_ref())
    }

    fn sample_actions_modal_metrics() -> DrawMetrics {
        let mut app = long_history_app();
        let (mut terminal, output_bytes) = output_terminal();
        let _ = measured_draw(&mut app, &mut terminal, output_bytes.as_ref());
        app.open_actions();
        measured_draw(&mut app, &mut terminal, output_bytes.as_ref())
    }

    fn sample_tree_metrics() -> DrawMetrics {
        let mut app = branch_tree_app();
        let (mut terminal, output_bytes) = output_terminal();
        let _ = measured_draw(&mut app, &mut terminal, output_bytes.as_ref());
        app.move_branch_navigator(-1);
        measured_draw(&mut app, &mut terminal, output_bytes.as_ref())
    }

    fn assert_output_budget(name: &str, metrics: DrawMetrics, cells: u64, bytes: u64) {
        assert!(
            metrics.changed_cells > 0,
            "{name} must produce a visible terminal diff"
        );
        assert!(
            metrics.changed_cells <= cells,
            "{name} changed {} cells, above the deterministic budget of {cells}",
            metrics.changed_cells
        );
        assert!(
            metrics.output_bytes <= bytes,
            "{name} emitted {} bytes, above the deterministic budget of {bytes}",
            metrics.output_bytes
        );
    }

    pub(super) fn state_update_benchmarks(criterion: &mut Criterion) {
        let mut group = criterion.benchmark_group("tui_polish_state_update");
        group.throughput(Throughput::Elements(
            u64::try_from(STREAM_DELTAS).expect("stream delta count should fit in u64"),
        ));
        group.bench_function("long_history/apply_256_stream_deltas", |bencher| {
            bencher.iter_batched(
                long_history_app,
                |mut app| {
                    stream_burst(&mut app);
                    black_box(app);
                },
                BatchSize::LargeInput,
            );
        });

        group.throughput(Throughput::Elements(
            u64::try_from(BRANCHES + 1).expect("branch count should fit in u64"),
        ));
        group.bench_function("branch_tree/build_33_previews", |bencher| {
            let app = branch_tree_app();
            bencher.iter(|| black_box(app.branch_previews()));
        });
        group.finish();
    }

    pub(super) fn frame_construction_benchmarks(criterion: &mut Criterion) {
        let mut group = criterion.benchmark_group("tui_polish_frame");
        group.sample_size(20);

        group.bench_function("long_history/stream_burst/120x40", |bencher| {
            bencher.iter_batched(
                || {
                    let mut app = long_history_app();
                    stream_burst(&mut app);
                    let terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))
                        .expect("stream frame terminal should initialize");
                    (app, terminal)
                },
                |(mut app, mut terminal)| {
                    terminal
                        .draw(|frame| view::render(frame, &mut app))
                        .expect("stream burst frame should render");
                    black_box((app, terminal));
                },
                BatchSize::LargeInput,
            );
        });

        group.bench_function("long_history/actions_modal/120x40", |bencher| {
            bencher.iter_batched(
                || {
                    let mut app = long_history_app();
                    app.open_actions();
                    let terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))
                        .expect("modal frame terminal should initialize");
                    (app, terminal)
                },
                |(mut app, mut terminal)| {
                    terminal
                        .draw(|frame| view::render(frame, &mut app))
                        .expect("modal frame should render");
                    black_box((app, terminal));
                },
                BatchSize::LargeInput,
            );
        });

        group.bench_function("branch_tree/33_nodes/120x40", |bencher| {
            bencher.iter_batched(
                || {
                    let app = branch_tree_app();
                    let terminal = Terminal::new(TestBackend::new(WIDTH, HEIGHT))
                        .expect("tree frame terminal should initialize");
                    (app, terminal)
                },
                |(mut app, mut terminal)| {
                    terminal
                        .draw(|frame| view::render(frame, &mut app))
                        .expect("tree frame should render");
                    black_box((app, terminal));
                },
                BatchSize::LargeInput,
            );
        });
        group.finish();
    }

    pub(super) fn terminal_diff_regressions(criterion: &mut Criterion) {
        let samples = [
            (
                "stream_burst",
                sample_stream_metrics(),
                WIDTH as u64 * HEIGHT as u64,
                24_000,
            ),
            (
                "actions_modal_open",
                sample_actions_modal_metrics(),
                1_600,
                8_000,
            ),
            ("branch_tree_move", sample_tree_metrics(), 2_000, 10_000),
        ];

        let mut group = criterion.benchmark_group("tui_polish_terminal_diff");
        for (name, metrics, cell_budget, byte_budget) in samples {
            assert_output_budget(name, metrics, cell_budget, byte_budget);
            group.throughput(Throughput::Bytes(metrics.output_bytes));
            group.bench_function(
                BenchmarkId::new(
                    name,
                    format!(
                        "{}cells_{}bytes/120x40",
                        metrics.changed_cells, metrics.output_bytes
                    ),
                ),
                |bencher| match name {
                    "stream_burst" => bencher.iter(|| black_box(sample_stream_metrics())),
                    "actions_modal_open" => {
                        bencher.iter(|| black_box(sample_actions_modal_metrics()))
                    }
                    "branch_tree_move" => bencher.iter(|| black_box(sample_tree_metrics())),
                    _ => unreachable!("all samples have a benchmark implementation"),
                },
            );
        }
        group.finish();
    }
}

criterion_group!(
    benches,
    tui::state_update_benchmarks,
    tui::frame_construction_benchmarks,
    tui::terminal_diff_regressions,
);
criterion_main!(benches);
