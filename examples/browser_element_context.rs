use clap::Parser;
use eyre::{Result, eyre};
use nanocodex_browser::{Browser, BrowserTool, ReactDiagnostics};
use nanocodex_tools::{
    DEFAULT_TOOL_OUTPUT_TOKENS, ToolContext, ToolOutputBody, ToolOutputContent, ToolRuntime, Tools,
};
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Map one rendered element to its React and source context through Code Mode")]
struct Args {
    /// Application URL to inspect.
    url: Url,

    /// Snapshot reference or CSS selector for the rendered element.
    selector: String,
}

const SOURCE: &str = r#"
await tools.browser({ action: "open", url: targetUrl });
await tools.browser({
  action: "wait_for_selector",
  target: { by: "css", selector: targetSelector }
});
const selected = await tools.browser({
  action: "element_context",
  target: { by: "css", selector: targetSelector }
});
const react = await tools.browser({
  action: "react_events",
  after: 0,
  limit: 1
});
text({ context: selected.context, status: react.status });
"#;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let browser = Browser::builder()
        .react_diagnostics(ReactDiagnostics::default())
        .build()?;
    let tools = Tools::builder()
        .without_defaults()
        .tool(BrowserTool::from_browser(browser.clone()))
        .build()?;
    let runtime = ToolRuntime::new_with_tools(".", None, None, &tools);
    let source = format!(
        "// @exec: {{\"yield_time_ms\": 30000, \"max_output_tokens\": 10000}}\nconst targetUrl = {};\nconst targetSelector = {};\n{SOURCE}",
        serde_json::to_string(args.url.as_str())?,
        serde_json::to_string(&args.selector)?,
    );
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
            "Code Mode element inspection failed: {}",
            serde_json::to_string_pretty(&calls)?
        ));
    }
    println!("{}", execution_text(&execution.output)?);
    browser.close().await?;
    Ok(())
}

fn context() -> ToolContext<'static> {
    ToolContext::new(
        "browser-element-context-example",
        "browser-element-context-example",
        "browser-element-context-example",
        &[],
        DEFAULT_TOOL_OUTPUT_TOKENS,
    )
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
