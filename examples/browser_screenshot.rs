use std::path::PathBuf;

use clap::Parser;
use eyre::{Result, eyre};
use nanocodex_browser::{Browser, BrowserAction, BrowserActionResult, BrowserTarget};
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Capture a webpage through nanocodex-browser")]
struct Args {
    /// Absolute HTTP(S) URL to capture.
    url: Url,

    /// Host path where the completed screenshot is copied.
    output: PathBuf,

    /// Time allowed for client-side rendering after navigation.
    #[arg(long, default_value_t = 1_500)]
    wait_ms: u64,

    /// Capture the complete scrollable page instead of the current viewport.
    #[arg(long)]
    full_page: bool,

    /// Dedicated Chrome `DevTools` endpoint, typically in a VM or virtual display.
    #[arg(long)]
    cdp_endpoint: Option<Url>,

    /// Require this text in the rendered page body; may be repeated.
    #[arg(long = "expect-text")]
    expected_text: Vec<String>,

    /// Require `navigator.webdriver` to have this value.
    #[arg(long)]
    expect_webdriver: Option<bool>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut browser = Browser::builder();
    if let Some(endpoint) = args.cdp_endpoint {
        browser = browser.cdp_endpoint(endpoint);
    }
    let browser = browser.build()?;
    browser
        .execute(BrowserAction::Open {
            url: args.url.to_string(),
        })
        .await?;
    browser
        .execute(BrowserAction::WaitForTimeout {
            milliseconds: args.wait_ms,
        })
        .await?;
    if !args.expected_text.is_empty() {
        let body = browser
            .execute(BrowserAction::GetText {
                target: BrowserTarget::css("body"),
            })
            .await?;
        let BrowserActionResult::Text { text, .. } = body else {
            return Err(eyre!("browser returned a non-text result"));
        };
        for expected in &args.expected_text {
            if !text.contains(expected) {
                return Err(eyre!("rendered page is missing expected text: {expected}"));
            }
        }
    }
    if let Some(expected) = args.expect_webdriver {
        let webdriver = browser
            .execute(BrowserAction::Evaluate {
                expression: "navigator.webdriver".to_owned(),
            })
            .await?;
        let BrowserActionResult::Evaluation { value, .. } = webdriver else {
            return Err(eyre!("browser returned a non-evaluation result"));
        };
        if value.as_bool() != Some(expected) {
            return Err(eyre!(
                "navigator.webdriver was {value}, expected {expected}"
            ));
        }
    }
    let screenshot = browser
        .execute(BrowserAction::Screenshot {
            full_page: args.full_page,
            annotate: false,
        })
        .await?;
    let BrowserActionResult::Screenshot { path, .. } = screenshot else {
        return Err(eyre!("browser returned a non-screenshot result"));
    };

    if let Some(parent) = args.output.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::copy(path, &args.output).await?;
    browser.close().await?;
    println!("{}", args.output.display());
    Ok(())
}
