use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use clap::Parser;
use eyre::{Result, bail};
use nanocodex_browser::{
    Browser, BrowserAction, BrowserActionResult, BrowserReactEventKind, BrowserTarget,
    ReactDiagnostics,
};
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Measure a repeated browser debugging loop against a real web application")]
struct Args {
    /// Application URL to inspect.
    url: Url,

    /// Number of warm debugging cycles.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u16).range(1..))]
    cycles: u16,

    /// Element read during every debugging cycle.
    #[arg(long, default_value = "main")]
    probe_selector: String,

    /// Click the first snapshot button whose accessible name contains this text.
    #[arg(long)]
    activate: Option<String>,

    /// Wait for this selector after activation.
    #[arg(long)]
    settle_selector: Option<String>,

    /// Write the final semantic snapshot for correctness inspection.
    #[arg(long)]
    snapshot_output: Option<PathBuf>,

    /// Emit this many console events before the warm loop.
    #[arg(long, default_value_t = 0)]
    console_events: u32,

    /// Issue this many local requests before the warm loop.
    #[arg(long, default_value_t = 0)]
    network_events: u16,

    /// Install React Scan Lite before application code and measure its typed event read.
    #[arg(long)]
    react: bool,
}

#[derive(Debug, Serialize)]
struct DebugBenchmarkReport {
    url: String,
    build_ms: f64,
    startup_ms: f64,
    navigation_ms: f64,
    activation_ms: Option<f64>,
    document: DocumentShape,
    cycles: usize,
    snapshot: DurationSummary,
    snapshot_characters: usize,
    snapshot_references: usize,
    get_text: DurationSummary,
    get_styles: DurationSummary,
    get_box: DurationSummary,
    console: DiagnosticSummary,
    errors: DiagnosticSummary,
    network_requests: DiagnosticSummary,
    react: Option<ReactDiagnosticSummary>,
    screenshot_ms: f64,
    cleanup_ms: f64,
    wall_ms: f64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentShape {
    light_dom_nodes: u64,
    open_shadow_roots: u64,
    shadow_dom_nodes: u64,
}

#[derive(Debug, Serialize)]
struct DiagnosticSummary {
    latency: DurationSummary,
    returned_items: usize,
    retained_items: usize,
    dropped_items: u64,
}

#[derive(Debug, Serialize)]
struct ReactDiagnosticSummary {
    read_ms: f64,
    renderer_count: usize,
    events: usize,
    commits: usize,
    fibers: usize,
    dropped: u64,
}

#[derive(Debug, Serialize)]
struct DurationSummary {
    samples: usize,
    min_ms: f64,
    p50_ms: f64,
    p95_ms: f64,
    max_ms: f64,
}

struct WarmMeasurements {
    snapshot: Vec<Duration>,
    snapshot_characters: usize,
    snapshot_references: usize,
    get_text: Vec<Duration>,
    get_styles: Vec<Duration>,
    get_box: Vec<Duration>,
    console: Vec<Duration>,
    console_items: usize,
    console_total: usize,
    console_dropped: u64,
    errors: Vec<Duration>,
    error_items: usize,
    error_total: usize,
    errors_dropped: u64,
    network_requests: Vec<Duration>,
    network_items: usize,
    network_total: usize,
    network_dropped: u64,
    last_snapshot: String,
}

impl WarmMeasurements {
    fn new(cycles: usize) -> Self {
        Self {
            snapshot: Vec::with_capacity(cycles),
            snapshot_characters: 0,
            snapshot_references: 0,
            get_text: Vec::with_capacity(cycles),
            get_styles: Vec::with_capacity(cycles),
            get_box: Vec::with_capacity(cycles),
            console: Vec::with_capacity(cycles),
            console_items: 0,
            console_total: 0,
            console_dropped: 0,
            errors: Vec::with_capacity(cycles),
            error_items: 0,
            error_total: 0,
            errors_dropped: 0,
            network_requests: Vec::with_capacity(cycles),
            network_items: 0,
            network_total: 0,
            network_dropped: 0,
            last_snapshot: String::new(),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let wall_started = Instant::now();
    let build_started = Instant::now();
    let mut browser = Browser::builder();
    if args.react {
        browser = browser.react_diagnostics(ReactDiagnostics::default());
    }
    let browser = browser.build()?;
    let build_ms = milliseconds(build_started.elapsed());

    let measured = measure(&browser, &args, build_ms).await;
    let cleanup_started = Instant::now();
    let cleanup = browser.close().await;
    let cleanup_ms = milliseconds(cleanup_started.elapsed());
    let mut report = match (measured, cleanup) {
        (Ok(report), Ok(())) => report,
        (Err(error), Ok(())) => return Err(error),
        (Ok(_), Err(error)) => return Err(error.into()),
        (Err(error), Err(cleanup_error)) => {
            return Err(error.wrap_err(format!("browser cleanup also failed: {cleanup_error}")));
        }
    };
    report.cleanup_ms = cleanup_ms;
    report.wall_ms = milliseconds(wall_started.elapsed());
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

async fn measure(browser: &Browser, args: &Args, build_ms: f64) -> Result<DebugBenchmarkReport> {
    let startup_started = Instant::now();
    browser.start().await?;
    let startup_ms = milliseconds(startup_started.elapsed());

    let navigation_started = Instant::now();
    browser
        .execute(BrowserAction::Open {
            url: args.url.to_string(),
        })
        .await?;
    let navigation_ms = milliseconds(navigation_started.elapsed());

    let activation_ms = if let Some(name) = &args.activate {
        Some(activate(browser, name, args.settle_selector.as_deref()).await?)
    } else {
        None
    };
    let document = document_shape(browser).await?;
    generate_diagnostic_load(browser, args.console_events, args.network_events).await?;
    let cycles = usize::from(args.cycles);
    let measurements = warm_cycles(browser, &args.probe_selector, cycles).await?;
    let react = if args.react {
        Some(read_react_diagnostics(browser).await?)
    } else {
        None
    };
    if let Some(path) = &args.snapshot_output {
        tokio::fs::write(path, &measurements.last_snapshot).await?;
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
        bail!("browser returned an unexpected screenshot result");
    };
    if !path.is_file() {
        bail!("browser did not create screenshot {}", path.display());
    }

    Ok(DebugBenchmarkReport {
        url: args.url.to_string(),
        build_ms,
        startup_ms,
        navigation_ms,
        activation_ms,
        document,
        cycles,
        snapshot: summarize(measurements.snapshot),
        snapshot_characters: measurements.snapshot_characters,
        snapshot_references: measurements.snapshot_references,
        get_text: summarize(measurements.get_text),
        get_styles: summarize(measurements.get_styles),
        get_box: summarize(measurements.get_box),
        console: DiagnosticSummary {
            latency: summarize(measurements.console),
            returned_items: measurements.console_items,
            retained_items: measurements.console_total,
            dropped_items: measurements.console_dropped,
        },
        errors: DiagnosticSummary {
            latency: summarize(measurements.errors),
            returned_items: measurements.error_items,
            retained_items: measurements.error_total,
            dropped_items: measurements.errors_dropped,
        },
        network_requests: DiagnosticSummary {
            latency: summarize(measurements.network_requests),
            returned_items: measurements.network_items,
            retained_items: measurements.network_total,
            dropped_items: measurements.network_dropped,
        },
        react,
        screenshot_ms,
        cleanup_ms: 0.0,
        wall_ms: 0.0,
    })
}

async fn read_react_diagnostics(browser: &Browser) -> Result<ReactDiagnosticSummary> {
    let started = Instant::now();
    let result = browser
        .execute(BrowserAction::ReactEvents {
            after: Some(0),
            limit: Some(1_000),
        })
        .await?;
    let read_ms = milliseconds(started.elapsed());
    let BrowserActionResult::ReactEvents {
        status,
        events,
        dropped,
        ..
    } = result
    else {
        bail!("browser returned an unexpected React diagnostics result");
    };
    let commits = events
        .iter()
        .filter(|event| event.kind == BrowserReactEventKind::Commit)
        .collect::<Vec<_>>();
    Ok(ReactDiagnosticSummary {
        read_ms,
        renderer_count: status.renderer_count,
        events: events.len(),
        commits: commits.len(),
        fibers: commits.iter().map(|event| event.tree.len()).sum(),
        dropped,
    })
}

async fn activate(
    browser: &Browser,
    accessible_name: &str,
    settle_selector: Option<&str>,
) -> Result<f64> {
    let snapshot = browser
        .execute(BrowserAction::Snapshot {
            interactive: true,
            compact: true,
            depth: None,
            selector: None,
            include_urls: false,
        })
        .await?;
    let BrowserActionResult::Snapshot { refs, .. } = snapshot else {
        bail!("browser returned an unexpected activation snapshot");
    };
    let reference = refs
        .iter()
        .find(|(_, element)| element.role == "button" && element.name.contains(accessible_name))
        .map(|(reference, _)| format!("@{reference}"));
    let Some(reference) = reference else {
        bail!("activation button containing {accessible_name:?} was not present");
    };

    let started = Instant::now();
    browser
        .execute(BrowserAction::Click {
            target: BrowserTarget::reference(reference),
            options: None,
        })
        .await?;
    if let Some(selector) = settle_selector {
        browser
            .execute(BrowserAction::WaitForSelector {
                target: BrowserTarget::css(selector),
                state: None,
            })
            .await?;
    }
    Ok(milliseconds(started.elapsed()))
}

async fn document_shape(browser: &Browser) -> Result<DocumentShape> {
    let result = browser
        .execute(BrowserAction::Evaluate {
            expression: r#"(() => {
  const hosts = Array.from(document.querySelectorAll("*"))
    .filter((element) => element.shadowRoot);
  return {
    lightDomNodes: document.querySelectorAll("*").length,
    openShadowRoots: hosts.length,
    shadowDomNodes: hosts.reduce(
      (count, host) => count + host.shadowRoot.querySelectorAll("*").length,
      0
    )
  };
})()"#
                .to_owned(),
        })
        .await?;
    let BrowserActionResult::Evaluation { value, .. } = result else {
        bail!("browser returned an unexpected document shape result");
    };
    Ok(serde_json::from_value(value)?)
}

async fn generate_diagnostic_load(
    browser: &Browser,
    console_events: u32,
    network_events: u16,
) -> Result<()> {
    if console_events == 0 && network_events == 0 {
        return Ok(());
    }
    browser
        .execute(BrowserAction::Evaluate {
            expression: format!(
                r#"(async () => {{
  for (let index = 0; index < {console_events}; index++) {{
    console.debug("nanocodex-debug-bench", index);
  }}
  await Promise.all(Array.from({{ length: {network_events} }}, (_, index) =>
    fetch(`/favicon.ico?nanocodex_debug_bench=${{index}}`, {{ cache: "no-store" }})
      .catch(() => null)
  ));
  return true;
}})()"#
            ),
        })
        .await?;
    browser
        .execute(BrowserAction::WaitForTimeout { milliseconds: 100 })
        .await?;
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one cycle measures the ordered debugging actions as one cohesive workload"
)]
async fn warm_cycles(
    browser: &Browser,
    probe_selector: &str,
    cycles: usize,
) -> Result<WarmMeasurements> {
    let mut measurements = WarmMeasurements::new(cycles);
    for _ in 0..cycles {
        let started = Instant::now();
        let result = browser
            .execute(BrowserAction::Snapshot {
                interactive: true,
                compact: true,
                depth: None,
                selector: None,
                include_urls: false,
            })
            .await?;
        measurements.snapshot.push(started.elapsed());
        let BrowserActionResult::Snapshot { snapshot, refs, .. } = result else {
            bail!("browser returned an unexpected warm snapshot result");
        };
        measurements.snapshot_characters = snapshot.len();
        measurements.snapshot_references = refs.len();
        measurements.last_snapshot = snapshot;

        let started = Instant::now();
        let result = browser
            .execute(BrowserAction::GetText {
                target: BrowserTarget::css(probe_selector),
            })
            .await?;
        measurements.get_text.push(started.elapsed());
        if !matches!(result, BrowserActionResult::Text { .. }) {
            bail!("browser returned an unexpected text result");
        }

        let started = Instant::now();
        let result = browser
            .execute(BrowserAction::GetStyles {
                target: BrowserTarget::css(probe_selector),
            })
            .await?;
        measurements.get_styles.push(started.elapsed());
        if !matches!(result, BrowserActionResult::Styles { .. }) {
            bail!("browser returned an unexpected styles result");
        }

        let started = Instant::now();
        let result = browser
            .execute(BrowserAction::GetBox {
                target: BrowserTarget::css(probe_selector),
            })
            .await?;
        measurements.get_box.push(started.elapsed());
        if !matches!(result, BrowserActionResult::Box { .. }) {
            bail!("browser returned an unexpected box result");
        }

        let started = Instant::now();
        let result = browser
            .execute(BrowserAction::Console { limit: None })
            .await?;
        measurements.console.push(started.elapsed());
        let BrowserActionResult::Console {
            entries,
            total,
            dropped,
            ..
        } = result
        else {
            bail!("browser returned an unexpected console result");
        };
        measurements.console_items = entries.len();
        measurements.console_total = total;
        measurements.console_dropped = dropped;

        let started = Instant::now();
        let result = browser
            .execute(BrowserAction::Errors { limit: None })
            .await?;
        measurements.errors.push(started.elapsed());
        let BrowserActionResult::Errors {
            errors,
            total,
            dropped,
            ..
        } = result
        else {
            bail!("browser returned an unexpected errors result");
        };
        measurements.error_items = errors.len();
        measurements.error_total = total;
        measurements.errors_dropped = dropped;

        let started = Instant::now();
        let result = browser
            .execute(BrowserAction::NetworkRequests {
                filter: None,
                after: None,
                limit: None,
            })
            .await?;
        measurements.network_requests.push(started.elapsed());
        let BrowserActionResult::NetworkRequests {
            requests,
            total,
            dropped,
            ..
        } = result
        else {
            bail!("browser returned an unexpected network result");
        };
        measurements.network_items = requests.len();
        measurements.network_total = total;
        measurements.network_dropped = dropped;
    }
    Ok(measurements)
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
