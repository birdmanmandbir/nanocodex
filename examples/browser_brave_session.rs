use std::io;

use clap::{Parser, ValueEnum};
use eyre::{Result, bail};
use nanocodex_browser::{BraveSession, Browser, BrowserAction, BrowserActionResult};
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Open an allowlisted URL in a private headless copy of Brave cookies")]
struct Args {
    /// Authenticated page to open in the private headless session.
    url: Url,

    /// Additional origin needed by the workflow, such as a company Okta origin.
    #[arg(long)]
    allow_origin: Vec<Url>,

    /// Dedicated Chrome `DevTools` endpoint, such as a headed browser in a VM.
    #[arg(long)]
    cdp_endpoint: Option<Url>,

    /// Brave profile directory under the user-data root.
    #[arg(long, default_value = "Default")]
    profile: String,

    /// Also import `localStorage` and `IndexedDB`; requires Brave to be closed.
    #[arg(long)]
    include_site_data: bool,

    /// Open the protected page in ordinary Brave for a passkey/authentication
    /// ceremony, wait for Enter, then refresh and resume headlessly.
    #[arg(long)]
    auth_handoff: bool,

    /// Print an interactive or full semantic snapshot after the page title.
    #[arg(long, value_enum)]
    snapshot: Option<SnapshotKind>,

    /// Time allowed for redirects and client-side rendering after navigation.
    #[arg(long, default_value_t = 0)]
    wait_ms: u64,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SnapshotKind {
    Interactive,
    Full,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let page_origin = Url::parse(&args.url.origin().ascii_serialization())?;
    let mut session = BraveSession::standard()?
        .profile_directory(args.profile)
        .allow_origin(page_origin);
    if args.include_site_data {
        session = session.include_site_data();
    }
    for origin in args.allow_origin {
        session = session.allow_origin(origin);
    }

    let mut browser = Browser::builder().brave_session(session);
    if let Some(endpoint) = args.cdp_endpoint {
        browser = browser.cdp_endpoint(endpoint);
    }
    let browser = browser.build()?;
    if args.auth_handoff {
        let handoff = browser.auth_handoff(args.url.clone())?.open()?;
        eprintln!(
            "Complete authentication in Brave, then press Enter to refresh the headless session."
        );
        tokio::task::spawn_blocking(|| {
            let mut confirmation = String::new();
            io::stdin().read_line(&mut confirmation)?;
            Ok::<_, io::Error>(())
        })
        .await??;
        handoff.resume().await?;
    } else {
        browser
            .execute(BrowserAction::Open {
                url: args.url.to_string(),
            })
            .await?;
    }
    if args.wait_ms > 0 {
        browser
            .execute(BrowserAction::WaitForTimeout {
                milliseconds: args.wait_ms,
            })
            .await?;
    }
    let result = browser.execute(BrowserAction::GetTitle).await?;
    let BrowserActionResult::Title { title, .. } = result else {
        bail!("browser returned a non-title result");
    };
    println!("{title}");
    if let Some(snapshot_kind) = args.snapshot {
        let result = browser
            .execute(BrowserAction::Snapshot {
                interactive: matches!(snapshot_kind, SnapshotKind::Interactive),
                compact: true,
                depth: None,
                selector: None,
                include_urls: false,
            })
            .await?;
        let BrowserActionResult::Snapshot { snapshot, .. } = result else {
            bail!("browser returned a non-snapshot result");
        };
        println!("{snapshot}");
    }
    browser.close().await?;
    Ok(())
}
