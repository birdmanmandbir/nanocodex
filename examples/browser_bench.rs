use std::time::{Duration, Instant};

use clap::Parser;
use eyre::{Result, bail, eyre};
use nanocodex_browser::{
    Browser, BrowserAction, BrowserActionName, BrowserActionResult, BrowserTarget,
};
use serde::Serialize;
use tokio::task::JoinSet;
use url::Url;

const BENCHMARK_PAGE: &str = "data:text/html,<main><h1 id='status'>Ready</h1><button>Save</button><input aria-label='Name'></main>";

#[derive(Debug, Parser)]
#[command(about = "Measure isolated browser startup and warm action latency")]
struct Args {
    /// Number of independent local browser sessions to run concurrently.
    #[arg(long, value_parser = clap::value_parser!(u16).range(1..))]
    sessions: Option<u16>,

    /// Dedicated CDP endpoint; repeat once per concurrent remote browser.
    #[arg(long)]
    cdp_endpoint: Vec<Url>,

    /// Number of warm DOM reads measured in each session.
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u16).range(1..))]
    warm_reads: u16,
}

#[derive(Debug, Serialize)]
struct BenchmarkReport {
    concurrent_sessions: usize,
    wall_ms: f64,
    sessions: Vec<SessionReport>,
}

#[derive(Debug, Serialize)]
struct SessionReport {
    session: usize,
    backend: &'static str,
    build_ms: f64,
    startup_and_navigation_ms: f64,
    snapshot_ms: f64,
    snapshot_references: usize,
    warm_get_text: DurationSummary,
    screenshot_ms: f64,
    cleanup_ms: f64,
}

#[derive(Debug, Serialize)]
struct DurationSummary {
    samples: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let endpoints = benchmark_endpoints(&args)?;
    let wall_started = Instant::now();
    let mut sessions = JoinSet::new();
    for (index, endpoint) in endpoints.into_iter().enumerate() {
        sessions.spawn(benchmark_session(index + 1, endpoint, args.warm_reads));
    }

    let mut reports = Vec::new();
    while let Some(report) = sessions.join_next().await {
        reports.push(report??);
    }
    reports.sort_by_key(|report| report.session);
    let report = BenchmarkReport {
        concurrent_sessions: reports.len(),
        wall_ms: milliseconds(wall_started.elapsed()),
        sessions: reports,
    };
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn benchmark_endpoints(args: &Args) -> Result<Vec<Option<Url>>> {
    if args.cdp_endpoint.is_empty() {
        return Ok((0..usize::from(args.sessions.unwrap_or(1)))
            .map(|_| None)
            .collect());
    }
    if let Some(sessions) = args.sessions
        && usize::from(sessions) != args.cdp_endpoint.len()
    {
        bail!(
            "--sessions must equal the number of --cdp-endpoint values ({}), got {sessions}",
            args.cdp_endpoint.len()
        );
    }
    Ok(args.cdp_endpoint.iter().cloned().map(Some).collect())
}

async fn benchmark_session(
    session: usize,
    endpoint: Option<Url>,
    warm_reads: u16,
) -> Result<SessionReport> {
    let backend = if endpoint.is_some() {
        "remote_cdp"
    } else {
        "local"
    };
    let build_started = Instant::now();
    let mut builder = Browser::builder();
    if let Some(endpoint) = endpoint {
        builder = builder.cdp_endpoint(endpoint);
    }
    let browser = builder.build()?;
    let build_ms = milliseconds(build_started.elapsed());

    let measured = measure_actions(&browser, session, backend, build_ms, warm_reads).await;
    let cleanup_started = Instant::now();
    let cleanup = browser.close().await;
    let cleanup_ms = milliseconds(cleanup_started.elapsed());
    match (measured, cleanup) {
        (Ok(mut report), Ok(())) => {
            report.cleanup_ms = cleanup_ms;
            Ok(report)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(close_error)) => {
            Err(error.wrap_err(format!("browser cleanup also failed: {close_error}")))
        }
    }
}

async fn measure_actions(
    browser: &Browser,
    session: usize,
    backend: &'static str,
    build_ms: f64,
    warm_reads: u16,
) -> Result<SessionReport> {
    let startup_started = Instant::now();
    let opened = browser
        .execute(BrowserAction::Open {
            url: BENCHMARK_PAGE.to_owned(),
        })
        .await?;
    let BrowserActionResult::Action {
        action: BrowserActionName::Open,
        executed: true,
        ..
    } = opened
    else {
        bail!("session {session} returned an unexpected open result");
    };
    let startup_and_navigation_ms = milliseconds(startup_started.elapsed());

    let snapshot_started = Instant::now();
    let snapshot = browser
        .execute(BrowserAction::Snapshot {
            interactive: true,
            compact: true,
            depth: None,
            selector: None,
            include_urls: false,
        })
        .await?;
    let snapshot_ms = milliseconds(snapshot_started.elapsed());
    let BrowserActionResult::Snapshot { refs, .. } = snapshot else {
        bail!("session {session} returned an unexpected snapshot result");
    };

    let mut reads = Vec::with_capacity(usize::from(warm_reads));
    for _ in 0..warm_reads {
        let started = Instant::now();
        let text = browser
            .execute(BrowserAction::GetText {
                target: BrowserTarget::css("#status"),
            })
            .await?;
        reads.push(started.elapsed());
        let BrowserActionResult::Text { text, .. } = text else {
            bail!("session {session} returned an unexpected text result");
        };
        if text != "Ready" {
            bail!("session {session} read unexpected benchmark text {text:?}");
        }
    }

    let screenshot_started = Instant::now();
    let screenshot = browser
        .execute(BrowserAction::Screenshot {
            full_page: false,
            annotate: false,
        })
        .await?;
    let screenshot_ms = milliseconds(screenshot_started.elapsed());
    let BrowserActionResult::Screenshot { path, .. } = screenshot else {
        bail!("session {session} returned an unexpected screenshot result");
    };
    if !path.is_file() {
        return Err(eyre!(
            "session {session} did not create screenshot {}",
            path.display()
        ));
    }

    Ok(SessionReport {
        session,
        backend,
        build_ms,
        startup_and_navigation_ms,
        snapshot_ms,
        snapshot_references: refs.len(),
        warm_get_text: summarize(reads),
        screenshot_ms,
        cleanup_ms: 0.0,
    })
}

fn summarize(mut durations: Vec<Duration>) -> DurationSummary {
    durations.sort_unstable();
    let samples = durations.len();
    DurationSummary {
        samples,
        min_ms: milliseconds(durations[0]),
        p50_ms: milliseconds(durations[percentile_index(samples, 50)]),
        p95_ms: milliseconds(durations[percentile_index(samples, 95)]),
        max_ms: milliseconds(durations[samples - 1]),
    }
}

const fn percentile_index(samples: usize, percentile: usize) -> usize {
    (samples - 1) * percentile / 100
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
