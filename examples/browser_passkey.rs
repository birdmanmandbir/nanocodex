use std::time::Duration;

use clap::Parser;
use eyre::{Result, eyre};
use nanocodex_browser::{
    Browser, BrowserAction, BrowserActionResult, BrowserTarget, VirtualAuthenticator,
    VirtualCredential,
};
use tokio::time::Instant;
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Exercise a passkey sign-up and sign-in flow through nanocodex-browser")]
struct Args {
    /// Page containing Sign up, Sign in, and Sign out or Disconnect buttons.
    url: Url,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let browser = Browser::builder()
        .virtual_authenticator(VirtualAuthenticator::platform_passkey())
        .build()?;

    browser
        .execute(BrowserAction::Open {
            url: args.url.to_string(),
        })
        .await?;
    let entry = click_first_named_button(&browser, &["Create account", "Sign in"]).await?;
    if entry == "Sign in" {
        fill_first_textbox(&browser, "nanocodex-browser@tempo.xyz").await?;
        click_first_named_button(&browser, &["Create account", "Sign up"]).await?;
    }
    let registered = wait_for_credential(&browser, |_| true).await?;
    println!(
        "registered passkey: rp_id={}, user={}",
        registered
            .relying_party_id
            .as_deref()
            .unwrap_or("<unknown>"),
        registered.user_name.as_deref().unwrap_or("<unknown>")
    );

    click_named_button_if_present(&browser, "Approve", Duration::from_secs(10)).await?;
    click_first_named_button(&browser, &["Sign out", "Disconnect"]).await?;
    click_named_button(&browser, "Sign in").await?;
    let asserted = wait_for_credential(&browser, |credential| {
        credential.credential_id == registered.credential_id
            && credential.sign_count > registered.sign_count
    })
    .await?;
    println!(
        "authenticated passkey: rp_id={}, sign_count={}",
        asserted.relying_party_id.as_deref().unwrap_or("<unknown>"),
        asserted.sign_count
    );

    browser.close().await?;
    Ok(())
}

async fn click_named_button(browser: &Browser, name: &str) -> Result<()> {
    click_first_named_button(browser, &[name]).await.map(drop)
}

async fn fill_first_textbox(browser: &Browser, text: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = browser
            .execute(BrowserAction::Snapshot {
                interactive: true,
                compact: true,
                depth: None,
                selector: None,
                include_urls: false,
            })
            .await?;
        let BrowserActionResult::Snapshot { refs, .. } = result else {
            return Err(eyre!("browser returned a non-snapshot result"));
        };
        if let Some((reference, _)) = refs.iter().find(|(_, element)| element.role == "textbox") {
            browser
                .execute(BrowserAction::Fill {
                    target: BrowserTarget::reference(format!("@{reference}")),
                    text: text.to_owned(),
                })
                .await?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(eyre!("a passkey account label textbox did not appear"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn click_named_button_if_present(
    browser: &Browser,
    name: &str,
    timeout: Duration,
) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let result = browser
            .execute(BrowserAction::Snapshot {
                interactive: true,
                compact: true,
                depth: None,
                selector: None,
                include_urls: false,
            })
            .await?;
        let BrowserActionResult::Snapshot { refs, .. } = result else {
            return Err(eyre!("browser returned a non-snapshot result"));
        };
        if let Some((reference, _)) = refs
            .iter()
            .find(|(_, element)| element.role == "button" && element.name == name)
        {
            browser
                .execute(BrowserAction::Click {
                    target: BrowserTarget::reference(format!("@{reference}")),
                    options: None,
                })
                .await?;
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn click_first_named_button(browser: &Browser, names: &[&str]) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut latest_snapshot;
    loop {
        let result = browser
            .execute(BrowserAction::Snapshot {
                interactive: true,
                compact: true,
                depth: None,
                selector: None,
                include_urls: false,
            })
            .await?;
        let BrowserActionResult::Snapshot { snapshot, refs, .. } = result else {
            return Err(eyre!("browser returned a non-snapshot result"));
        };
        latest_snapshot = snapshot;
        if let Some((reference, element)) = refs
            .iter()
            .find(|(_, element)| element.role == "button" && names.contains(&element.name.as_str()))
        {
            let name = element.name.clone();
            browser
                .execute(BrowserAction::Click {
                    target: BrowserTarget::reference(format!("@{reference}")),
                    options: None,
                })
                .await?;
            return Ok(name);
        }
        if Instant::now() >= deadline {
            return Err(eyre!(
                "none of the buttons {names:?} appeared; latest snapshot:\n{latest_snapshot}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for_credential(
    browser: &Browser,
    predicate: impl Fn(&VirtualCredential) -> bool,
) -> Result<VirtualCredential> {
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut latest;
    loop {
        latest = browser.virtual_credentials().await?;
        if let Some(credential) = latest.iter().find(|credential| predicate(credential)) {
            let credential = credential.clone();
            return Ok(credential);
        }
        if Instant::now() >= deadline {
            let errors = browser
                .execute(BrowserAction::Errors { limit: None })
                .await?;
            let console = browser
                .execute(BrowserAction::Console { limit: None })
                .await?;
            let screenshot = browser
                .execute(BrowserAction::Screenshot {
                    full_page: false,
                    annotate: false,
                })
                .await?;
            let BrowserActionResult::Screenshot { path, .. } = screenshot else {
                return Err(eyre!("browser returned a non-screenshot result"));
            };
            let failure_screenshot = "/tmp/nanocodex-passkey-failure.png";
            tokio::fs::copy(path, failure_screenshot).await?;
            return Err(eyre!(
                "passkey operation did not complete; credentials: {latest:?}; \
                 errors: {errors:?}; console: {console:?}; \
                 screenshot: {failure_screenshot}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
