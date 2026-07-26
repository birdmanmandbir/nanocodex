use clap::Parser;
use eyre::{Result, eyre};
use nanocodex_browser::{Browser, BrowserTool, ReactDiagnostics};
use nanocodex_tools::{
    DEFAULT_TOOL_OUTPUT_TOKENS, ToolContext, ToolOutputBody, ToolOutputContent, ToolRuntime, Tools,
};
use url::Url;

#[derive(Debug, Parser)]
#[command(about = "Verify full DOM and network coverage through the Code Mode browser binding")]
struct Args {
    /// Application URL to inspect.
    url: Url,

    /// Click the first button whose accessible name contains this text.
    #[arg(long)]
    activate: Option<String>,

    /// Element reference or CSS selector to map back to React/source context.
    #[arg(long, default_value = "main")]
    element_context: String,
}

const INSPECTION_SOURCE: &str = r#"
await tools.browser({ action: "open", url: targetUrl });
const semantic = await tools.browser({
  action: "snapshot",
  interactive: true,
  compact: true
});

if (activationName !== null) {
  const reference = Object.entries(semantic.refs)
    .find(([, element]) =>
      element.role === "button" && element.name.includes(activationName)
    )?.[0];
  if (reference === undefined) {
    throw new Error(`button containing ${activationName} was not present`);
  }
  await tools.browser({
    action: "click",
    target: { by: "ref", reference: `@${reference}` }
  });
  await tools.browser({
    action: "wait_for_selector",
    target: { by: "css", selector: "file-tree-container" }
  });
  await tools.browser({ action: "wait_for_timeout", milliseconds: 500 });
}

const jsGroundTruth = await tools.browser({
  action: "evaluate",
  expression: `(() => {
    const counts = {
      elements: 0,
      textNodes: 0,
      comments: 0,
      attributes: 0,
      openShadowRoots: 0
    };
    const visit = (root) => {
      for (const node of root.childNodes) {
        if (node.nodeType === Node.ELEMENT_NODE) {
          counts.elements++;
          counts.attributes += node.attributes.length;
          if (node.shadowRoot) {
            counts.openShadowRoots++;
            visit(node.shadowRoot);
          }
          visit(node);
        } else if (node.nodeType === Node.TEXT_NODE) {
          counts.textNodes++;
        } else if (node.nodeType === Node.COMMENT_NODE) {
          counts.comments++;
        }
      }
    };
    visit(document);
    return {
      dom: counts,
      resources: performance.getEntriesByType("resource").map((entry) => ({
        url: entry.name,
        initiatorType: entry.initiatorType,
        transferSize: entry.transferSize,
        encodedBodySize: entry.encodedBodySize,
        decodedBodySize: entry.decodedBodySize
      }))
    };
  })()`
});

const complete = await tools.browser({
  action: "dom_snapshot",
  computed_styles: ["display", "visibility", "position"],
  include_dom_rects: true,
  include_paint_order: true
});
await tools.browser({
  action: "wait_for_selector",
  target: { by: "css", selector: elementContextSelector }
});
const selectedElement = await tools.browser({
  action: "element_context",
  target: { by: "css", selector: elementContextSelector }
});
const nodes = complete.snapshot.documents.flatMap((document) => document.nodes);
const customElements = nodes
  .filter((node) => node.nodeType === 1 && node.nodeName.includes("-"))
  .map((node) => node.nodeName.toLowerCase());

let requestCursor = 0;
const requests = [];
for (;;) {
  const page = await tools.browser({
    action: "network_requests",
    after: requestCursor,
    limit: 1000
  });
  requests.push(...page.requests);
  if (!page.has_more || page.last_sequence === null) break;
  requestCursor = page.last_sequence;
}

let messageCursor = 0;
const webSocketMessages = [];
for (;;) {
  const page = await tools.browser({
    action: "web_socket_messages",
    after: messageCursor,
    limit: 1000
  });
  webSocketMessages.push(...page.messages);
  if (!page.has_more || page.last_sequence === null) break;
  messageCursor = page.last_sequence;
}

const capturedUrls = new Set(requests.map((request) => request.url));
const missingResources = jsGroundTruth.value.resources
  .filter((resource) => !capturedUrls.has(resource.url));
const missingResourceTypes = {};
for (const resource of missingResources) {
  missingResourceTypes[resource.initiatorType] =
    (missingResourceTypes[resource.initiatorType] ?? 0) + 1;
}
const capturedRequestTypes = {};
for (const request of requests) {
  capturedRequestTypes[request.resourceType] =
    (capturedRequestTypes[request.resourceType] ?? 0) + 1;
}
const pendingRequests = requests.filter((request) => !request.completed);
const pendingRequestTypes = {};
for (const request of pendingRequests) {
  const key = `${request.context}:${request.resourceType}`;
  pendingRequestTypes[key] = (pendingRequestTypes[key] ?? 0) + 1;
}
const canReadBody = (request) =>
  request.bodyAvailable &&
  request.completed &&
  request.status >= 200 &&
  request.status < 300 &&
  ["Document", "Script", "Fetch", "XHR"].includes(request.resourceType);
const bodyCandidate =
  requests.find((request) =>
    request.context === "child_target" && canReadBody(request)
  ) ??
  requests.find(canReadBody);
let responseBody = null;
if (bodyCandidate !== undefined) {
  try {
    const body = await tools.browser({
      action: "network_body",
      request_id: bodyCandidate.requestId,
      kind: "response"
    });
    responseBody = {
      requestId: body.request_id,
      context: bodyCandidate.context,
      bytes: body.body.length,
      base64Encoded: body.base64_encoded
    };
  } catch (error) {
    responseBody = { requestId: bodyCandidate.requestId, error: String(error) };
  }
}

let reactCursor = 0;
let reactStatus = null;
let reactDropped = 0;
const reactEvents = [];
for (;;) {
  const page = await tools.browser({
    action: "react_events",
    after: reactCursor,
    limit: 1000
  });
  reactStatus = page.status;
  reactDropped = page.dropped;
  reactEvents.push(...page.events);
  if (!page.has_more || page.last_sequence === null) break;
  reactCursor = page.last_sequence;
}
const reactCommits = reactEvents.filter((event) => event.kind === "commit");
const componentRenders = new Map();
for (const commit of reactCommits) {
  for (const fiber of commit.tree) {
    const key = fiber.fiberId === null
      ? `${fiber.name}:${fiber.source?.fileName ?? ""}:${fiber.source?.lineNumber ?? ""}`
      : String(fiber.fiberId);
    const current = componentRenders.get(key) ?? {
      name: fiber.name,
      renders: 0,
      totalDurationMs: 0,
      maxDurationMs: 0,
      source: fiber.source,
      ownerName: fiber.ownerName,
      causes: {
        firstMounts: 0,
        parentRenders: 0,
        stateChanges: 0,
        contextChanges: 0,
        hookChanges: 0,
        changedProps: {}
      }
    };
    current.renders++;
    current.totalDurationMs += fiber.actualDurationMs;
    current.maxDurationMs = Math.max(
      current.maxDurationMs,
      fiber.actualDurationMs
    );
    const change = fiber.changeDescription;
    if (change !== null) {
      if (change.isFirstMount) current.causes.firstMounts++;
      if (change.parent) current.causes.parentRenders++;
      if (change.state) current.causes.stateChanges++;
      if (change.context) current.causes.contextChanges++;
      current.causes.hookChanges += change.hooks.length;
      for (const prop of change.props ?? []) {
        current.causes.changedProps[prop] =
          (current.causes.changedProps[prop] ?? 0) + 1;
      }
    }
    componentRenders.set(key, current);
  }
}
const hottestReactFibers = [...componentRenders.values()]
  .sort((left, right) => right.totalDurationMs - left.totalDurationMs)
  .slice(0, 20);

text({
  semanticReferences: Object.keys(semantic.refs).length,
  javascriptGroundTruth: jsGroundTruth.value.dom,
  completeDom: {
    documents: complete.snapshot.documents.length,
    nodes: complete.snapshot.nodeCount,
    elements: complete.snapshot.elementCount,
    textNodes: complete.snapshot.textNodeCount,
    comments: complete.snapshot.commentCount,
    attributes: complete.snapshot.attributeCount,
    shadowTreeNodes: complete.snapshot.shadowTreeNodeCount,
    customElements: [...new Set(customElements)].sort(),
    nodesWithRequestedStyles: nodes.filter((node) =>
      node.layout !== null &&
      Object.keys(node.layout.styles).length === 3
    ).length
  },
  network: {
    capturedRequests: requests.length,
    completedRequests: requests.filter((request) => request.completed).length,
    failedRequests: requests.filter((request) => request.failure !== null).length,
    performanceResources: jsGroundTruth.value.resources.length,
    childTargetRequests: requests.filter((request) =>
      request.context === "child_target"
    ).length,
    childTargetExamples: requests
      .filter((request) => request.context === "child_target")
      .slice(0, 20)
      .map((request) => request.url),
    capturedRequestTypes,
    pendingRequestTypes,
    pendingRequestExamples: pendingRequests.slice(0, 5).map((request) => ({
      context: request.context,
      type: request.resourceType,
      status: request.status,
      url: request.url
    })),
    capturedScriptExamples: requests
      .filter((request) => request.resourceType === "Script")
      .slice(-10)
      .map((request) => request.url),
    capturedShikiExamples: requests
      .filter((request) => request.url.includes("shiki"))
      .slice(0, 20)
      .map((request) => request.url),
    missingResourceCount: missingResources.length,
    missingResourceTypes,
    missingResourceExamples: missingResources.slice(0, 10),
    webSocketConnections: requests.filter((request) =>
      request.resourceType === "WebSocket"
    ).length,
    webSocketMessages: webSocketMessages.length,
    responseBody
  },
  react: {
    status: reactStatus,
    selectedElement: selectedElement.context,
    retainedEvents: reactEvents.length,
    droppedEvents: reactDropped,
    commits: reactCommits.length,
    profilingEvents: reactEvents.filter((event) =>
      !["renderer-injected", "profiling-hooks-status", "commit", "post-commit", "fiber-unmount"]
        .includes(event.kind)
    ).length,
    hottestFibers: hottestReactFibers
  }
});
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
        "// @exec: {{\"yield_time_ms\": 30000, \"max_output_tokens\": 10000}}\nconst targetUrl = {};\nconst activationName = {};\nconst elementContextSelector = {};\n{INSPECTION_SOURCE}",
        serde_json::to_string(args.url.as_str())?,
        serde_json::to_string(&args.activate)?,
        serde_json::to_string(&args.element_context)?,
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
            "Code Mode inspection failed: {}",
            serde_json::to_string_pretty(&calls)?
        ));
    }
    println!("{}", execution_text(&execution.output)?);
    browser.close().await?;
    Ok(())
}

fn context() -> ToolContext<'static> {
    ToolContext {
        model: "browser-inspect-example",
        session_id: "browser-inspect-example",
        call_id: "browser-inspect-example",
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
