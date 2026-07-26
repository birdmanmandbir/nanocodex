use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use clap::Parser;
use eyre::{Result, bail, eyre};
use nanocodex_browser::{Browser, BrowserAction, BrowserActionResult, BrowserGate, BrowserTarget};
use rand::Rng;
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Run a bounded browser correctness canary")]
struct Args {
    /// Absolute HTTP(S) URL to revisit.
    url: Url,

    /// Chrome `DevTools` endpoint for the browser under test.
    #[arg(long)]
    cdp_endpoint: Url,

    /// Number of navigations before reporting success.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u16).range(1..))]
    attempts: u16,

    /// Minimum delay after one completed navigation before starting the next.
    #[arg(long, default_value_t = 8_000)]
    minimum_interval_ms: u64,

    /// Additional uniformly distributed delay after the minimum interval.
    #[arg(long, default_value_t = 2_000)]
    jitter_ms: u64,

    /// Time allowed for client-side rendering after each navigation.
    #[arg(long, default_value_t = 1_500)]
    render_wait_ms: u64,

    /// Time allowed for a transient JavaScript forwarding gate to clear itself.
    #[arg(long, default_value_t = 10_000)]
    gate_grace_ms: u64,

    /// Require this text in the rendered page body; may be repeated.
    #[arg(long = "expect-text", required = true)]
    expected_text: Vec<String>,

    /// Directory receiving the final or first-failure screenshot.
    #[arg(long)]
    evidence_directory: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    tokio::fs::create_dir_all(&args.evidence_directory).await?;

    let browser = Browser::builder()
        .cdp_endpoint(args.cdp_endpoint.clone())
        .build()?;
    let outcome = run_canary(&browser, &args).await;
    let close_result = browser.close().await;

    match (outcome, close_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
        (Err(error), Err(close_error)) => {
            Err(error.wrap_err(format!("browser shutdown also failed: {close_error}")))
        }
    }
}

async fn run_canary(browser: &Browser, args: &Args) -> Result<()> {
    for attempt in 1..=args.attempts {
        let started_at = Instant::now();
        let attempt_outcome = inspect_once(browser, args).await;
        let elapsed = started_at.elapsed();

        match attempt_outcome {
            Ok(observation) => {
                println!(
                    "attempt={attempt}/{total} result=pass elapsed_ms={} transient_gate={} body_bytes={} body_hash={:016x} title={:?}",
                    elapsed.as_millis(),
                    observation.transient_gate,
                    observation.body_bytes,
                    observation.body_hash,
                    observation.title,
                    total = args.attempts,
                );
                if attempt == args.attempts {
                    save_evidence(browser, &args.evidence_directory, "final.png").await?;
                    println!(
                        "canary=pass attempts={} evidence={}",
                        args.attempts,
                        args.evidence_directory.join("final.png").display()
                    );
                    return Ok(());
                }
            }
            Err(error) => {
                let evidence = args
                    .evidence_directory
                    .join(format!("failure-{attempt:03}.png"));
                let screenshot_error = save_evidence(
                    browser,
                    &args.evidence_directory,
                    &format!("failure-{attempt:03}.png"),
                )
                .await
                .err();
                eprintln!(
                    "attempt={attempt}/{total} result=fail elapsed_ms={} evidence={} error={error:#}",
                    elapsed.as_millis(),
                    evidence.display(),
                    total = args.attempts,
                );
                if let Some(screenshot_error) = screenshot_error {
                    return Err(error.wrap_err(format!(
                        "failure screenshot also failed: {screenshot_error}"
                    )));
                }
                return Err(error);
            }
        }

        let jitter = if args.jitter_ms == 0 {
            0
        } else {
            rand::rng().random_range(0..=args.jitter_ms)
        };
        let delay = Duration::from_millis(args.minimum_interval_ms.saturating_add(jitter));
        println!("next_attempt_delay_ms={}", delay.as_millis());
        tokio::time::sleep(delay).await;
    }

    bail!("canary completed without a terminal outcome")
}

async fn inspect_once(browser: &Browser, args: &Args) -> Result<Observation> {
    browser
        .execute(BrowserAction::Open {
            url: args.url.to_string(),
        })
        .await?;
    browser
        .execute(BrowserAction::WaitForTimeout {
            milliseconds: args.render_wait_ms,
        })
        .await?;

    let transient_gate = wait_for_gate(browser, Duration::from_millis(args.gate_grace_ms)).await?;

    let body = browser
        .execute(BrowserAction::GetText {
            target: BrowserTarget::css("body"),
        })
        .await?;
    let BrowserActionResult::Text { text, .. } = body else {
        bail!("browser returned a non-text result");
    };
    let missing = args
        .expected_text
        .iter()
        .filter(|expected| !text.contains(expected.as_str()))
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!("rendered page is missing expected text: {missing:?}");
    }

    let webdriver = browser
        .execute(BrowserAction::Evaluate {
            expression: "navigator.webdriver".to_owned(),
        })
        .await?;
    let BrowserActionResult::Evaluation { value, .. } = webdriver else {
        bail!("browser returned a non-evaluation result");
    };
    if value.as_bool() != Some(false) {
        bail!("navigator.webdriver was {value}, expected false");
    }

    let title = browser.execute(BrowserAction::GetTitle).await?;
    let BrowserActionResult::Title { title, .. } = title else {
        bail!("browser returned a non-title result");
    };
    let active_url = browser.execute(BrowserAction::GetUrl).await?;
    let BrowserActionResult::Url { url, .. } = active_url else {
        bail!("browser returned a non-URL result");
    };
    if Url::parse(&url)?.origin() != args.url.origin() {
        bail!("page navigated to an unexpected origin: {url}");
    }

    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    Ok(Observation {
        transient_gate,
        body_bytes: text.len(),
        body_hash: hasher.finish(),
        title,
    })
}

async fn wait_for_gate(browser: &Browser, grace: Duration) -> Result<bool> {
    let started_at = Instant::now();
    let mut transient_gate = false;
    loop {
        let result = match browser.execute(BrowserAction::DetectGate).await {
            Ok(result) => result,
            Err(_) if started_at.elapsed() < grace => {
                transient_gate = true;
                tokio::time::sleep(Duration::from_millis(250)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let BrowserActionResult::Gate { gate, .. } = result else {
            bail!("browser returned a non-gate result");
        };
        match gate {
            BrowserGate::Clear => return Ok(transient_gate),
            BrowserGate::JsChallenge { .. } if started_at.elapsed() < grace => {
                transient_gate = true;
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            gate => bail!("page displayed a persistent browser gate: {gate:?}"),
        }
    }
}

async fn save_evidence(browser: &Browser, directory: &Path, name: &str) -> Result<()> {
    let screenshot = browser
        .execute(BrowserAction::Screenshot {
            full_page: true,
            annotate: false,
        })
        .await?;
    let BrowserActionResult::Screenshot { path, .. } = screenshot else {
        return Err(eyre!("browser returned a non-screenshot result"));
    };
    tokio::fs::copy(path, directory.join(name)).await?;
    Ok(())
}

struct Observation {
    transient_gate: bool,
    body_bytes: usize,
    body_hash: u64,
    title: String,
}
