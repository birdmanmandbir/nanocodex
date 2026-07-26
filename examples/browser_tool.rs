use clap::Parser;
use eyre::{Result, eyre};
use nanocodex_browser::{Browser, BrowserAfterAction, BrowserTool};
use nanocodex_tools::{
    DEFAULT_TOOL_OUTPUT_TOKENS, ToolContext, ToolOutputBody, ToolOutputContent, ToolRuntime, Tools,
};
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Exercise the browser through Nanocodex Code Mode")]
struct Args {
    /// Dedicated Chrome `DevTools` endpoint, such as a headed browser in a VM.
    #[arg(long)]
    cdp_endpoint: Option<Url>,

    /// Number of warm targeted reads to issue through Code Mode.
    #[arg(long, default_value_t = 25, value_parser = clap::value_parser!(u16).range(1..))]
    warm_reads: u16,
}

const CODE_MODE_SOURCE: &str = r#"
const opened = await tools.browser({
  action: "open",
  url: "data:text/html,<main><button onclick=\"document.querySelector('output').textContent='saved'\">Save</button><input aria-label='Name'><output>idle</output></main>"
});
const snapshot = await tools.browser({
  action: "snapshot"
});
const found = await tools.browser({
  action: "snapshot_find",
  query: "Save",
  max_results: 5
});
const name = Object.entries(snapshot.refs)
  .find(([, element]) => element.role === "textbox")?.[0];
const save = Object.entries(found.refs)
  .find(([, element]) => element.role === "button")?.[0];
if (name === undefined) {
  throw new Error("Name field not found");
}
if (save === undefined) {
  throw new Error("Save button not found");
}
await tools.browser({
  action: "fill",
  target: { by: "ref", reference: `@${name}` },
  text: "Nanocodex"
});
const clicked = await tools.browser({
  action: "click",
  target: { by: "ref", reference: `@${save}` }
});
await tools.browser({
  action: "wait_for_text",
  text: "saved",
  target: { by: "css", selector: "output" }
});
let value;
for (let index = 0; index < __WARM_READS__; index++) {
  value = await tools.browser({
    action: "get_value",
    target: { by: "ref", reference: `@${name}` }
  });
}
const screenshot = await tools.browser({
  action: "screenshot"
});
const { modelImage, ...screenshotImage } = screenshot.image;
if (modelImage === null) {
  throw new Error("browser did not expose the screenshot to Code Mode");
}
image(modelImage);
text({
  opened,
  snapshot,
  found,
  clicked,
  value,
  screenshot: { ...screenshot, image: screenshotImage }
});
"#;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut browser = Browser::builder().after_action(BrowserAfterAction::Snapshot);
    if let Some(endpoint) = args.cdp_endpoint {
        browser = browser.cdp_endpoint(endpoint);
    }
    let browser = browser.build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool(BrowserTool::from_browser(browser.clone()))
        .build()?;
    let runtime = ToolRuntime::new_with_tools(".", None, None, &tools);
    let source = CODE_MODE_SOURCE.replace("__WARM_READS__", &args.warm_reads.to_string());
    let execution = runtime.execute_code(&source, context()).await;

    if !execution.success {
        let calls = execution
            .nested_calls
            .iter()
            .map(|call| {
                serde_json::json!({
                    "input": call.input,
                    "output": call.output,
                    "success": call.success,
                })
            })
            .collect::<Vec<_>>();
        return Err(eyre!(
            "Code Mode execution failed:\n{}",
            serde_json::to_string_pretty(&calls)?
        ));
    }
    if !matches!(
        &execution.output,
        ToolOutputBody::Content(content)
            if content
                .iter()
                .any(|item| matches!(item, ToolOutputContent::InputImage { .. }))
    ) {
        return Err(eyre!("Code Mode did not emit the browser screenshot"));
    }

    eprintln!("Browser action timings:");
    let mut warm_reads = Vec::with_capacity(usize::from(args.warm_reads));
    for call in &execution.nested_calls {
        let action = call.input["action"].as_str().unwrap_or("unknown");
        if action == "get_value" {
            warm_reads.push(std::time::Duration::from_nanos(call.duration_ns));
            continue;
        }
        eprintln!(
            "  {action}: {:.2} ms",
            std::time::Duration::from_nanos(call.duration_ns).as_secs_f64() * 1_000.0
        );
    }
    warm_reads.sort_unstable();
    eprintln!(
        "  get_value ({} warm calls): p50 {:.2} ms, p95 {:.2} ms",
        warm_reads.len(),
        milliseconds(warm_reads[warm_reads.len() / 2]),
        milliseconds(warm_reads[(warm_reads.len() - 1) * 95 / 100]),
    );
    println!("Code Mode result:");
    println!("{}", execution_text(&execution.output)?);
    browser.close().await?;
    Ok(())
}

fn context() -> ToolContext<'static> {
    ToolContext {
        model: "browser-tool-example",
        session_id: "browser-tool-example",
        call_id: "browser-tool-example",
        history: &[],
        output_token_budget: DEFAULT_TOOL_OUTPUT_TOKENS,
    }
}

fn execution_text(output: &ToolOutputBody) -> Result<&str> {
    let ToolOutputBody::Content(content) = output else {
        return Err(eyre!("expected Code Mode content output"));
    };
    content
        .iter()
        .rev()
        .find_map(|content| match content {
            ToolOutputContent::InputText { text } => Some(text.as_str()),
            ToolOutputContent::InputImage { .. } | ToolOutputContent::InputAudio { .. } => None,
        })
        .ok_or_else(|| eyre!("missing Code Mode text output"))
}

fn milliseconds(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
