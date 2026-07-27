use std::{
    fmt,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use clap::Parser;
use eyre::{Context, Result, bail, eyre};
use futures_util::{StreamExt, stream};
use nanocodex_browser::{
    Browser, BrowserAction, BrowserActionResult, BrowserElementReference, BrowserTarget,
    VirtualAuthenticator,
};
use serde::Serialize;
use tokio::time::sleep;
use url::Url;

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const ELEMENT_TIMEOUT: Duration = Duration::from_secs(30);
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(30);
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(90);
const ACCOUNT_LABEL: &str = "nanocodex-browser@tempo.xyz";

#[derive(Debug, Parser)]
#[command(about = "Run every interactive Tempo docs demo through nanocodex-browser")]
struct Args {
    /// Root of the deployed Tempo developer docs.
    #[arg(long, default_value = "https://tempo.xyz/developers/docs/")]
    base_url: Url,

    /// Directory under which this run retains its typed report and screenshots.
    #[arg(long, default_value = "target/tempo-docs-stress")]
    output: PathBuf,

    /// Run only demos whose stable name contains this value. Repeatable.
    #[arg(long)]
    demo: Vec<String>,

    /// Number of isolated browser sessions allowed to run concurrently.
    #[arg(long, default_value_t = 2)]
    concurrency: usize,

    /// Print the stable demo names without launching Chromium.
    #[arg(long)]
    list: bool,
}

#[derive(Clone, Copy, Debug)]
struct DemoCase {
    name: &'static str,
    route: &'static str,
    workflow: Workflow,
}

#[derive(Clone, Copy, Debug)]
enum Workflow {
    Faucet,
    PasskeyAccounts,
    Payment,
    Token(TokenWorkflow),
    Swap,
    Liquidity,
    Zone(ZoneWorkflow),
    VirtualAddresses,
}

#[derive(Clone, Copy, Debug)]
enum TokenWorkflow {
    Deploy,
    Mint,
    Manage,
    Fees,
    Rewards,
}

#[derive(Clone, Copy, Debug)]
enum ZoneWorkflow {
    Deposit,
    SendWithin,
    SendAcross,
    SwapAcross,
    Withdraw,
}

#[derive(Clone, Copy, Debug)]
enum NameMatch {
    Exact(&'static str),
    Prefix(&'static str),
    Contains(&'static str),
}

impl NameMatch {
    fn matches(self, actual: &str) -> bool {
        match self {
            Self::Exact(expected) => actual == expected,
            Self::Prefix(expected) => actual.starts_with(expected),
            Self::Contains(expected) => actual.contains(expected),
        }
    }
}

impl fmt::Display for NameMatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(name) => write!(formatter, "{name:?}"),
            Self::Prefix(name) => write!(formatter, "prefix {name:?}"),
            Self::Contains(name) => write!(formatter, "containing {name:?}"),
        }
    }
}

#[derive(Clone, Debug)]
struct ElementMatch {
    reference: String,
}

struct DemoRunner {
    browser: Browser,
    case: DemoCase,
    url: Url,
    evidence_dir: PathBuf,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum DemoStatus {
    Passed,
    Unavailable,
    Failed,
}

#[derive(Debug, Serialize)]
struct DemoOutcome {
    name: &'static str,
    url: String,
    status: DemoStatus,
    elapsed_ms: u128,
    final_url: Option<String>,
    screenshot: Option<PathBuf>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct StressReport {
    protocol_version: u32,
    base_url: String,
    concurrency: usize,
    elapsed_ms: u128,
    demos: Vec<DemoOutcome>,
}

#[derive(Debug)]
struct DemoUnavailable {
    reason: String,
}

impl fmt::Display for DemoUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.reason)
    }
}

impl std::error::Error for DemoUnavailable {}

const DEMOS: &[DemoCase] = &[
    DemoCase {
        name: "faucet",
        route: "quickstart/faucet",
        workflow: Workflow::Faucet,
    },
    DemoCase {
        name: "passkey-accounts",
        route: "guide/use-accounts/embed-passkeys",
        workflow: Workflow::PasskeyAccounts,
    },
    DemoCase {
        name: "send-a-payment",
        route: "guide/payments/send-a-payment",
        workflow: Workflow::Payment,
    },
    DemoCase {
        name: "create-a-stablecoin",
        route: "guide/issuance/create-a-stablecoin",
        workflow: Workflow::Token(TokenWorkflow::Deploy),
    },
    DemoCase {
        name: "mint-stablecoins",
        route: "guide/issuance/mint-stablecoins",
        workflow: Workflow::Token(TokenWorkflow::Mint),
    },
    DemoCase {
        name: "manage-stablecoin",
        route: "guide/issuance/manage-stablecoin",
        workflow: Workflow::Token(TokenWorkflow::Manage),
    },
    DemoCase {
        name: "executing-swaps",
        route: "guide/stablecoin-dex/executing-swaps",
        workflow: Workflow::Swap,
    },
    DemoCase {
        name: "providing-liquidity",
        route: "guide/stablecoin-dex/providing-liquidity",
        workflow: Workflow::Liquidity,
    },
    DemoCase {
        name: "use-for-fees",
        route: "guide/issuance/use-for-fees",
        workflow: Workflow::Token(TokenWorkflow::Fees),
    },
    DemoCase {
        name: "distribute-rewards",
        route: "guide/issuance/distribute-rewards",
        workflow: Workflow::Token(TokenWorkflow::Rewards),
    },
    DemoCase {
        name: "deposit-to-a-zone",
        route: "guide/private-zones/deposit-to-a-zone",
        workflow: Workflow::Zone(ZoneWorkflow::Deposit),
    },
    DemoCase {
        name: "send-tokens-within-a-zone",
        route: "guide/private-zones/send-tokens-within-a-zone",
        workflow: Workflow::Zone(ZoneWorkflow::SendWithin),
    },
    DemoCase {
        name: "send-tokens-across-zones",
        route: "guide/private-zones/send-tokens-across-zones",
        workflow: Workflow::Zone(ZoneWorkflow::SendAcross),
    },
    DemoCase {
        name: "swap-across-zones",
        route: "guide/private-zones/swap-across-zones",
        workflow: Workflow::Zone(ZoneWorkflow::SwapAcross),
    },
    DemoCase {
        name: "withdraw-from-a-zone",
        route: "guide/private-zones/withdraw-from-a-zone",
        workflow: Workflow::Zone(ZoneWorkflow::Withdraw),
    },
    DemoCase {
        name: "virtual-addresses",
        route: "guide/payments/virtual-addresses",
        workflow: Workflow::VirtualAddresses,
    },
];

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    if args.list {
        for case in DEMOS {
            println!("{}", case.name);
        }
        return Ok(());
    }
    if args.concurrency == 0 {
        bail!("--concurrency must be greater than zero");
    }

    let cases = DEMOS
        .iter()
        .copied()
        .filter(|case| {
            args.demo.is_empty() || args.demo.iter().any(|filter| case.name.contains(filter))
        })
        .collect::<Vec<_>>();
    if cases.is_empty() {
        bail!("no demo matched --demo {:?}", args.demo);
    }

    let run_id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .wrap_err("system clock is before the Unix epoch")?
        .as_millis();
    let run_dir = args.output.join(run_id.to_string());
    tokio::fs::create_dir_all(&run_dir).await?;

    let started = Instant::now();
    let base_url = args.base_url.clone();
    let evidence_dir = run_dir.clone();
    let mut outcomes = stream::iter(cases.into_iter().map(|case| {
        let base_url = base_url.clone();
        let evidence_dir = evidence_dir.clone();
        async move {
            println!("[running] {}", case.name);
            let outcome = DemoRunner::run(case, &base_url, &evidence_dir).await;
            match outcome.status {
                DemoStatus::Passed => {
                    println!("[passed ] {} ({} ms)", case.name, outcome.elapsed_ms);
                }
                DemoStatus::Unavailable => {
                    println!(
                        "[missing] {} ({} ms): {}",
                        case.name,
                        outcome.elapsed_ms,
                        outcome.error.as_deref().unwrap_or("unavailable")
                    );
                }
                DemoStatus::Failed => {
                    println!(
                        "[failed ] {} ({} ms): {}",
                        case.name,
                        outcome.elapsed_ms,
                        outcome.error.as_deref().unwrap_or("unknown failure")
                    );
                }
            }
            outcome
        }
    }))
    .buffer_unordered(args.concurrency)
    .collect::<Vec<_>>()
    .await;
    outcomes.sort_by_key(|outcome| {
        DEMOS
            .iter()
            .position(|case| case.name == outcome.name)
            .unwrap_or(usize::MAX)
    });

    let failures = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, DemoStatus::Failed))
        .count();
    let unavailable = outcomes
        .iter()
        .filter(|outcome| matches!(outcome.status, DemoStatus::Unavailable))
        .count();
    let report = StressReport {
        protocol_version: 1,
        base_url: args.base_url.to_string(),
        concurrency: args.concurrency,
        elapsed_ms: started.elapsed().as_millis(),
        demos: outcomes,
    };
    let report_path = run_dir.join("report.json");
    tokio::fs::write(&report_path, serde_json::to_vec_pretty(&report)?).await?;
    println!(
        "{} passed, {unavailable} unavailable, {failures} failed in {} ms; report: {}",
        report.demos.len() - failures - unavailable,
        report.elapsed_ms,
        report_path.display()
    );

    if failures > 0 {
        bail!("{failures} Tempo docs demos failed");
    }
    Ok(())
}

impl DemoRunner {
    async fn run(case: DemoCase, base_url: &Url, evidence_root: &Path) -> DemoOutcome {
        let started = Instant::now();
        let url = match base_url.join(case.route) {
            Ok(url) => url,
            Err(error) => {
                return DemoOutcome {
                    name: case.name,
                    url: case.route.to_owned(),
                    status: DemoStatus::Failed,
                    elapsed_ms: started.elapsed().as_millis(),
                    final_url: None,
                    screenshot: None,
                    error: Some(format!("could not form demo URL: {error}")),
                };
            }
        };
        let evidence_dir = evidence_root.join(case.name);
        if let Err(error) = tokio::fs::create_dir_all(&evidence_dir).await {
            return DemoOutcome {
                name: case.name,
                url: url.to_string(),
                status: DemoStatus::Failed,
                elapsed_ms: started.elapsed().as_millis(),
                final_url: None,
                screenshot: None,
                error: Some(format!("could not create evidence directory: {error}")),
            };
        }
        let browser = match Browser::builder()
            .virtual_authenticator(VirtualAuthenticator::platform_passkey())
            .build()
        {
            Ok(browser) => browser,
            Err(error) => {
                return DemoOutcome {
                    name: case.name,
                    url: url.to_string(),
                    status: DemoStatus::Failed,
                    elapsed_ms: started.elapsed().as_millis(),
                    final_url: None,
                    screenshot: None,
                    error: Some(format!("could not build browser: {error}")),
                };
            }
        };
        let runner = Self {
            browser,
            case,
            url,
            evidence_dir,
        };
        runner.finish(started).await
    }

    async fn finish(self, started: Instant) -> DemoOutcome {
        let result = self.execute().await;
        let unavailable = result
            .as_ref()
            .err()
            .is_some_and(|error| error.downcast_ref::<DemoUnavailable>().is_some());
        let final_url = self.current_url().await.ok();
        let screenshot = self.capture_screenshot().await.ok();
        let close_error = self.browser.close().await.err();
        let error = match (result.err(), close_error) {
            (None, None) => None,
            (Some(error), None) => Some(format!("{error:#}")),
            (None, Some(error)) => Some(format!("browser close failed: {error}")),
            (Some(error), Some(close_error)) => Some(format!(
                "{error:#}; browser close also failed: {close_error}"
            )),
        };
        DemoOutcome {
            name: self.case.name,
            url: self.url.to_string(),
            status: match (error.is_some(), unavailable) {
                (false, _) => DemoStatus::Passed,
                (true, true) => DemoStatus::Unavailable,
                (true, false) => DemoStatus::Failed,
            },
            elapsed_ms: started.elapsed().as_millis(),
            final_url,
            screenshot,
            error,
        }
    }

    async fn execute(&self) -> Result<()> {
        self.browser
            .execute(BrowserAction::Open {
                url: self.url.to_string(),
            })
            .await?;
        let current_url = self.current_url().await?;
        let current_url = Url::parse(&current_url)?;
        if !current_url
            .path()
            .trim_end_matches('/')
            .ends_with(self.case.route)
        {
            return Err(DemoUnavailable {
                reason: format!(
                    "the source-backed demo route redirects to {current_url} on the live docs"
                ),
            }
            .into());
        }
        if self
            .snapshot(false)
            .await?
            .values()
            .any(|element| element.role == "heading" && element.name == "Page not found")
        {
            return Err(DemoUnavailable {
                reason: "the source-backed demo route is not deployed on the live docs".to_owned(),
            }
            .into());
        }
        match self.case.workflow {
            Workflow::Faucet => self.run_faucet().await,
            Workflow::PasskeyAccounts => self.run_passkey_accounts().await,
            Workflow::Payment => self.run_payment().await,
            Workflow::Token(workflow) => self.run_token(workflow).await,
            Workflow::Swap => self.run_swap().await,
            Workflow::Liquidity => self.run_liquidity().await,
            Workflow::Zone(workflow) => self.run_zone(workflow).await,
            Workflow::VirtualAddresses => self.run_virtual_addresses().await,
        }
    }

    async fn run_faucet(&self) -> Result<()> {
        self.click("tab", NameMatch::Exact("Fund an address"))
            .await?;
        self.fill(
            "textbox",
            NameMatch::Exact("Address to fund"),
            "0xbeefcafe54750903ac1c8909323af7beb21ea2cb",
        )
        .await?;
        self.click("button", NameMatch::Exact("Add funds")).await?;
        self.wait(
            "button",
            NameMatch::Exact("Add more funds"),
            TRANSACTION_TIMEOUT,
        )
        .await?;
        self.wait("link", NameMatch::Exact("View transfers"), ELEMENT_TIMEOUT)
            .await
            .map(drop)
    }

    async fn run_passkey_accounts(&self) -> Result<()> {
        self.connect().await?;
        self.click_any(
            "button",
            &[NameMatch::Exact("Sign out"), NameMatch::Exact("Disconnect")],
        )
        .await?;
        self.click("button", NameMatch::Exact("Sign in")).await?;
        self.wait_any(
            "button",
            &[NameMatch::Exact("Sign out"), NameMatch::Exact("Disconnect")],
            TRANSACTION_TIMEOUT,
        )
        .await
        .map(drop)
    }

    async fn run_payment(&self) -> Result<()> {
        self.connect_and_fund().await?;
        self.click("button", NameMatch::Exact("Enter details"))
            .await?;
        self.fill(
            "textbox",
            NameMatch::Exact("Memo (optional)"),
            "nanocodex-browser-stress",
        )
        .await?;
        self.click_transaction(NameMatch::Exact("Send")).await
    }

    async fn run_token(&self, workflow: TokenWorkflow) -> Result<()> {
        self.connect_and_fund().await?;
        let (name, symbol) = match workflow {
            TokenWorkflow::Deploy => ("Nanocodex Test USD", "NANO"),
            TokenWorkflow::Mint => ("Nanocodex Mint USD", "NMINT"),
            TokenWorkflow::Manage => ("Nanocodex Manage USD", "NMAN"),
            TokenWorkflow::Fees => ("Nanocodex Fee USD", "NFEE"),
            TokenWorkflow::Rewards => ("Nanocodex Reward USD", "NRWD"),
        };
        self.fill("textbox", NameMatch::Contains("Token name"), name)
            .await?;
        self.fill("textbox", NameMatch::Contains("Token symbol"), symbol)
            .await?;
        self.click_transaction(NameMatch::Exact("Deploy")).await?;

        if matches!(workflow, TokenWorkflow::Deploy) {
            return Ok(());
        }
        self.enter_and_transact("Grant").await?;
        if matches!(workflow, TokenWorkflow::Manage) {
            return self.enter_and_transact("Revoke").await;
        }
        self.enter_and_transact("Mint").await?;
        match workflow {
            TokenWorkflow::Mint => Ok(()),
            TokenWorkflow::Fees => {
                self.click_transaction(NameMatch::Exact("Add Liquidity"))
                    .await?;
                self.enter_and_transact("Send").await
            }
            TokenWorkflow::Rewards => {
                for button in ["Opt In", "Start Reward", "Claim"] {
                    self.click_transaction(NameMatch::Exact(button)).await?;
                }
                Ok(())
            }
            TokenWorkflow::Deploy | TokenWorkflow::Manage => unreachable!(),
        }
    }

    async fn run_swap(&self) -> Result<()> {
        self.connect_and_fund().await?;
        self.click_transaction(NameMatch::Exact("Buy")).await
    }

    async fn run_liquidity(&self) -> Result<()> {
        self.connect_and_fund().await?;
        self.click_transaction(NameMatch::Exact("Place Order"))
            .await?;
        self.wait_enabled("button", NameMatch::Exact("Query Order"))
            .await?;
        self.click("button", NameMatch::Exact("Query Order"))
            .await?;
        self.wait("button", NameMatch::Exact("Query Again"), ELEMENT_TIMEOUT)
            .await
            .map(drop)
    }

    async fn run_zone(&self, workflow: ZoneWorkflow) -> Result<()> {
        self.connect().await?;
        self.wait_enabled("button", NameMatch::Prefix("Authoriz"))
            .await?;
        self.click("button", NameMatch::Prefix("Authoriz")).await?;
        match workflow {
            ZoneWorkflow::Deposit => {
                if self
                    .click_if_present("button", NameMatch::Exact("Get testnet pathUSD"))
                    .await?
                {
                    self.wait_enabled("button", NameMatch::Exact("Deposit 100 pathUSD"))
                        .await?;
                }
                self.click_transaction(NameMatch::Exact("Deposit 100 pathUSD"))
                    .await?;
                self.wait_text(
                    NameMatch::Contains("Wait for Zone A to credit the deposit."),
                    ELEMENT_TIMEOUT,
                )
                .await
            }
            workflow => {
                if self
                    .click_if_present("button", NameMatch::Exact("Get testnet pathUSD"))
                    .await?
                {
                    self.wait_enabled("button", NameMatch::Exact("Approve + top up Zone A"))
                        .await?;
                }
                if Self::find(
                    &self.snapshot(true).await?,
                    "button",
                    NameMatch::Exact("Approve + top up Zone A"),
                )
                .is_some()
                {
                    self.click_transaction(NameMatch::Exact("Approve + top up Zone A"))
                        .await?;
                }
                let expected = match workflow {
                    ZoneWorkflow::SendWithin => {
                        "Send 25 pathUSD from Zone A to the demo recipient."
                    }
                    ZoneWorkflow::SendAcross => {
                        "Withdraw 25 pathUSD from Zone A and route it into Zone B."
                    }
                    ZoneWorkflow::SwapAcross => {
                        "Withdraw 25 pathUSD from Zone A, swap it, and route betaUSD into Zone B."
                    }
                    ZoneWorkflow::Withdraw => "Submit the withdrawal back from Zone A.",
                    ZoneWorkflow::Deposit => unreachable!(),
                };
                self.wait_text(NameMatch::Contains(expected), ELEMENT_TIMEOUT)
                    .await
            }
        }
    }

    async fn run_virtual_addresses(&self) -> Result<()> {
        self.wait(
            "heading",
            NameMatch::Exact("Use virtual addresses for deposits"),
            ELEMENT_TIMEOUT,
        )
        .await?;
        self.click("tab", NameMatch::Exact("Real registration"))
            .await?;
        self.connect().await?;
        self.wait_text(
            NameMatch::Contains("Connected passkey account"),
            ELEMENT_TIMEOUT,
        )
        .await?;
        self.click("button", NameMatch::Exact("Register master id"))
            .await?;
        self.wait_text(NameMatch::Contains("registration tx:"), TRANSACTION_TIMEOUT)
            .await
    }

    async fn connect_and_fund(&self) -> Result<()> {
        self.connect().await?;
        self.click("button", NameMatch::Exact("Add funds")).await?;
        self.wait(
            "button",
            NameMatch::Exact("Add more funds"),
            TRANSACTION_TIMEOUT,
        )
        .await
        .map(drop)
    }

    async fn connect(&self) -> Result<()> {
        let entry = self
            .wait_any_enabled(
                "button",
                &[NameMatch::Exact("Sign up"), NameMatch::Exact("Sign in")],
                ELEMENT_TIMEOUT,
            )
            .await?;
        self.click_reference(&entry.reference).await?;
        let deadline = Instant::now() + ACCOUNT_TIMEOUT;
        let dependency_check_at = Instant::now() + Duration::from_secs(2);
        let mut dependency_checked = false;
        let mut label_filled = false;
        loop {
            let refs = self.snapshot(true).await?;
            if refs.values().any(|element| {
                element.frame_url.is_none()
                    && element.role == "button"
                    && matches!(element.name.as_str(), "Sign out" | "Disconnect")
            }) {
                return Ok(());
            }
            if !label_filled
                && let Some((reference, _)) = refs.iter().find(|(_, element)| {
                    element.role == "textbox"
                        && element
                            .frame_url
                            .as_deref()
                            .is_some_and(|url| url.contains("wallet.tempo.xyz"))
                })
            {
                self.fill_reference(reference, ACCOUNT_LABEL).await?;
                label_filled = true;
                continue;
            }
            if let Some((reference, _)) = refs.iter().find(|(_, element)| {
                element.role == "button"
                    && element
                        .frame_url
                        .as_deref()
                        .is_some_and(|url| url.contains("wallet.tempo.xyz"))
                    && matches!(
                        element.name.as_str(),
                        "Create account" | "Sign up" | "Approve"
                    )
                    && !element.disabled
            }) {
                self.click_reference(reference).await?;
                continue;
            }
            if !dependency_checked && Instant::now() >= dependency_check_at {
                dependency_checked = true;
                if let Some(reason) = self.passkey_dependency_failure().await? {
                    return Err(DemoUnavailable { reason }.into());
                }
            }
            if Instant::now() >= deadline {
                let credentials = self.browser.virtual_credentials().await?;
                let errors = self
                    .browser
                    .execute(BrowserAction::Errors { limit: None })
                    .await?;
                let console = self
                    .browser
                    .execute(BrowserAction::Console { limit: None })
                    .await?;
                let network = self.passkey_network_requests().await?;
                if let BrowserActionResult::NetworkRequests { requests, .. } = &network
                    && let Some(request) = requests
                        .iter()
                        .find(|request| request.status.is_some_and(|status| status >= 400))
                {
                    return Err(DemoUnavailable {
                        reason: format!(
                            "the live passkey dependency returned HTTP {} for {}",
                            request.status.unwrap_or_default(),
                            request.url
                        ),
                    }
                    .into());
                }
                return Err(eyre!(
                    "account connection did not reach a signed-in state; \
                     credentials: {credentials:?}; errors: {errors:?}; console: {console:?}; \
                     network: {network:?}; \
                     latest snapshot:\n{}",
                    self.snapshot_text(true).await?
                ));
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn passkey_dependency_failure(&self) -> Result<Option<String>> {
        let network = self.passkey_network_requests().await?;
        let BrowserActionResult::NetworkRequests { requests, .. } = network else {
            bail!("browser returned a non-network result");
        };
        Ok(requests
            .iter()
            .find(|request| request.status.is_some_and(|status| status >= 400))
            .map(|request| {
                format!(
                    "the live passkey dependency returned HTTP {} for {}",
                    request.status.unwrap_or_default(),
                    request.url
                )
            }))
    }

    async fn passkey_network_requests(&self) -> Result<BrowserActionResult> {
        Ok(self
            .browser
            .execute(BrowserAction::NetworkRequests {
                filter: Some("keys.tempo.xyz".to_owned()),
                after: None,
                limit: None,
            })
            .await?)
    }

    async fn enter_and_transact(&self, action: &'static str) -> Result<()> {
        self.click("button", NameMatch::Exact("Enter details"))
            .await?;
        self.click_transaction(NameMatch::Exact(action)).await
    }

    async fn click_transaction(&self, name: NameMatch) -> Result<()> {
        let before = self.receipt_count().await?;
        self.click("button", name).await?;
        self.wait_for_new_receipt(before).await
    }

    async fn wait_for_new_receipt(&self, before: usize) -> Result<()> {
        let deadline = Instant::now() + TRANSACTION_TIMEOUT;
        let mut quiet_since = None;
        loop {
            let refs = self.snapshot(true).await?;
            if let Some((reference, _)) = refs.iter().find(|(_, element)| {
                element.role == "button"
                    && element
                        .frame_url
                        .as_deref()
                        .is_some_and(|url| url.contains("wallet.tempo.xyz"))
                    && !element.disabled
                    && (matches!(
                        element.name.as_str(),
                        "Approve" | "Confirm" | "Continue" | "Send"
                    ) || element.name.starts_with("Pay "))
            }) {
                self.click_reference(reference).await?;
                quiet_since = None;
                continue;
            }
            let receipts = refs
                .values()
                .filter(|element| element.role == "link" && element.name == "View receipt")
                .count();
            if receipts > before {
                let quiet_since = quiet_since.get_or_insert_with(Instant::now);
                if quiet_since.elapsed() >= Duration::from_secs(1) {
                    return Ok(());
                }
            }
            if Instant::now() >= deadline {
                return Err(eyre!(
                    "expected a new receipt link after {before}, found {receipts}; latest snapshot:\n{}",
                    self.snapshot_text(true).await?
                ));
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn receipt_count(&self) -> Result<usize> {
        Ok(self
            .snapshot(true)
            .await?
            .values()
            .filter(|element| element.role == "link" && element.name == "View receipt")
            .count())
    }

    async fn click(&self, role: &'static str, name: NameMatch) -> Result<()> {
        let element = self
            .wait_any_enabled(role, &[name], ELEMENT_TIMEOUT)
            .await?;
        self.click_reference(&element.reference).await
    }

    async fn click_any(&self, role: &'static str, names: &[NameMatch]) -> Result<()> {
        let element = self.wait_any_enabled(role, names, ELEMENT_TIMEOUT).await?;
        self.click_reference(&element.reference).await
    }

    async fn click_if_present(&self, role: &'static str, name: NameMatch) -> Result<bool> {
        let refs = self.snapshot(true).await?;
        let Some((reference, element)) = Self::find(&refs, role, name) else {
            return Ok(false);
        };
        if element.disabled {
            return Ok(false);
        }
        self.click_reference(reference).await?;
        Ok(true)
    }

    async fn fill(&self, role: &'static str, name: NameMatch, value: &str) -> Result<()> {
        let element = self.wait(role, name, ELEMENT_TIMEOUT).await?;
        self.fill_reference(&element.reference, value).await
    }

    async fn wait_enabled(&self, role: &'static str, name: NameMatch) -> Result<()> {
        let deadline = Instant::now() + TRANSACTION_TIMEOUT;
        loop {
            let refs = self.snapshot(true).await?;
            if Self::find(&refs, role, name).is_some_and(|(_, element)| !element.disabled) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(eyre!("{role} {name} did not become enabled"));
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait(
        &self,
        role: &'static str,
        name: NameMatch,
        timeout: Duration,
    ) -> Result<ElementMatch> {
        self.wait_any(role, &[name], timeout).await
    }

    async fn wait_any(
        &self,
        role: &'static str,
        names: &[NameMatch],
        timeout: Duration,
    ) -> Result<ElementMatch> {
        let deadline = Instant::now() + timeout;
        loop {
            let refs = self.snapshot(true).await?;
            if let Some((reference, _)) =
                names.iter().find_map(|name| Self::find(&refs, role, *name))
            {
                return Ok(ElementMatch {
                    reference: reference.clone(),
                });
            }
            if Instant::now() >= deadline {
                return Err(eyre!(
                    "{role} matching {names:?} did not appear; latest snapshot:\n{}",
                    self.snapshot_text(true).await?
                ));
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_any_enabled(
        &self,
        role: &'static str,
        names: &[NameMatch],
        timeout: Duration,
    ) -> Result<ElementMatch> {
        let deadline = Instant::now() + timeout;
        loop {
            let refs = self.snapshot(true).await?;
            if let Some((reference, _)) = names.iter().find_map(|name| {
                refs.iter().find(|(_, element)| {
                    element.role == role && name.matches(&element.name) && !element.disabled
                })
            }) {
                return Ok(ElementMatch {
                    reference: reference.clone(),
                });
            }
            if Instant::now() >= deadline {
                return Err(eyre!(
                    "enabled {role} matching {names:?} did not appear; latest snapshot:\n{}",
                    self.snapshot_text(true).await?
                ));
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn wait_text(&self, name: NameMatch, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            let text = self.body_text().await?;
            let present = match name {
                NameMatch::Exact(expected) => text.lines().any(|line| line.trim() == expected),
                NameMatch::Prefix(expected) => {
                    text.lines().any(|line| line.trim().starts_with(expected))
                }
                NameMatch::Contains(expected) => text.contains(expected),
            };
            if present {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(eyre!(
                    "text {name} did not appear; latest snapshot:\n{}",
                    self.snapshot_text(false).await?
                ));
            }
            sleep(POLL_INTERVAL).await;
        }
    }

    async fn body_text(&self) -> Result<String> {
        let result = self
            .browser
            .execute(BrowserAction::GetText {
                target: BrowserTarget::css("body"),
            })
            .await?;
        let BrowserActionResult::Text { text, .. } = result else {
            bail!("browser returned a non-text result");
        };
        Ok(text)
    }

    async fn snapshot(
        &self,
        interactive: bool,
    ) -> Result<std::collections::BTreeMap<String, BrowserElementReference>> {
        let result = self
            .browser
            .execute(BrowserAction::Snapshot {
                interactive,
                compact: true,
                depth: None,
                selector: None,
                include_urls: false,
            })
            .await?;
        let BrowserActionResult::Snapshot { refs, .. } = result else {
            bail!("browser returned a non-snapshot result");
        };
        Ok(refs)
    }

    async fn snapshot_text(&self, interactive: bool) -> Result<String> {
        let result = self
            .browser
            .execute(BrowserAction::Snapshot {
                interactive,
                compact: true,
                depth: None,
                selector: None,
                include_urls: true,
            })
            .await?;
        let BrowserActionResult::Snapshot { snapshot, .. } = result else {
            bail!("browser returned a non-snapshot result");
        };
        Ok(snapshot)
    }

    fn find<'a>(
        refs: &'a std::collections::BTreeMap<String, BrowserElementReference>,
        role: &str,
        name: NameMatch,
    ) -> Option<(&'a String, &'a BrowserElementReference)> {
        refs.iter()
            .find(|(_, element)| element.role == role && name.matches(&element.name))
    }

    async fn click_reference(&self, reference: &str) -> Result<()> {
        self.browser
            .execute(BrowserAction::Click {
                target: BrowserTarget::reference(format!("@{reference}")),
                options: None,
            })
            .await?;
        Ok(())
    }

    async fn fill_reference(&self, reference: &str, text: &str) -> Result<()> {
        self.browser
            .execute(BrowserAction::Fill {
                target: BrowserTarget::reference(format!("@{reference}")),
                text: text.to_owned(),
            })
            .await?;
        Ok(())
    }

    async fn current_url(&self) -> Result<String> {
        let result = self.browser.execute(BrowserAction::GetUrl).await?;
        let BrowserActionResult::Url { url, .. } = result else {
            bail!("browser returned a non-URL result");
        };
        Ok(url)
    }

    async fn capture_screenshot(&self) -> Result<PathBuf> {
        let result = self
            .browser
            .execute(BrowserAction::Screenshot {
                full_page: true,
                annotate: false,
            })
            .await?;
        let BrowserActionResult::Screenshot { path, .. } = result else {
            bail!("browser returned a non-screenshot result");
        };
        let destination = self.evidence_dir.join("final.png");
        tokio::fs::copy(path, &destination).await?;
        Ok(destination)
    }
}
