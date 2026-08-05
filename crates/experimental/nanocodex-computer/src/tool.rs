use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use nanocodex_oai_api::ImageDetail;
use nanocodex_tools::{
    Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult,
    contract::ToolOutputContent,
    runtime::{DynamicToolProvider, schema_for},
};
use tracing::{Instrument as _, field::Empty, info_span};

use crate::{Computer, ComputerAction, ComputerActionResult};

const MAX_MODEL_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

const TOOL_DESCRIPTION: &str = r"Control one user-selected native macOS application.

Call from Code Mode as `await tools.computer({ action: ..., ... })`. This is a
state-first tool. When the bundle ID is known, always call open_application
first even if the app is already running; this primes renderer accessibility
and restores the previously frontmost app. Then attach to one exact app/window
and inspect the returned state before acting. Observations return a screenshot
and compact accessibility elements with generation-bound references such as `e12_4`.
Use this tool for native applications and user-visible desktop state. For normal
website navigation or inspection, prefer `tools.browser`, which exposes DOM and
browser diagnostics directly. Do not recreate native app control with shell,
AppleScript, Node, or an ad hoc Accessibility client.
References expire on the next observation or action; always use fresh ones.
After the first state for an exact window, `state.accessibility_update` describes
only unambiguous added, changed, and removed elements relative to its base
generation; `state.elements` remains the authoritative complete current tree.

Prefer semantic element click/set_value/perform_action over coordinates.
For whole-field replacement, use set_value directly. If a workflow explicitly
uses a background selection shortcut such as Command-A, inspect the returned
element's selected_text before typing; keyboard delivery can succeed without a
native app changing its selection. Fall back to set_value instead of repeating
an unverified shortcut.
Coordinate input is global and is intended only for controls absent from the
accessibility tree. If screenshot pixels are used, map them to global points as
`window.x + pixel_x * window.width / screenshot.width` and likewise for y.
Forward `state.screenshot.model_image` with Code Mode's `image(...)` helper;
never pass `state.screenshot.path` to `image(...)` and never print the data URL
as text. Keep state in JavaScript, filter elements by role/label/value, perform
deterministic semantic actions, and verify their returned state in the same
cell. Emit only concise candidate summaries when model judgment is actually
needed; never dump the complete element array. Actions target the attached PID
and isolated window without requiring it to stay frontmost. Mutating actions
already settle and return fresh state; do not add wait/observe calls unless
explicit time must pass. Never use Promise.all: one session deliberately
serializes native input.

The human can pause or intervene independently. A paused error means stop
issuing actions until the host resumes control. Never attempt to bypass macOS
Accessibility or Screen Recording permission errors. A locked desktop or
application authorization error is terminal until the human or host changes
that condition. URL policy is host-owned: never retry a disallowed URL or try
to navigate around it.";

/// Nanocodex Code Mode provider for one owned native computer session.
#[derive(Clone)]
pub struct ComputerTool {
    computer: Computer,
}

impl ComputerTool {
    /// Wraps an existing session. Its event/frame consumers remain independent.
    #[must_use]
    pub const fn from_computer(computer: Computer) -> Self {
        Self { computer }
    }

    /// Returns the underlying cheap action handle.
    #[must_use]
    pub const fn computer(&self) -> &Computer {
        &self.computer
    }
}

fn definition() -> ToolDefinition {
    static DEFINITION: OnceLock<ToolDefinition> = OnceLock::new();
    DEFINITION
        .get_or_init(|| {
            ToolDefinition::function("computer", TOOL_DESCRIPTION, schema_for::<ComputerAction>())
                .with_output_schema(schema_for::<ComputerActionResult>())
        })
        .clone()
}

#[async_trait]
impl Tool for ComputerTool {
    fn definition(&self) -> ToolDefinition {
        definition()
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let action = input.decode_json::<ComputerAction>()?;
        let result = self.computer.execute(action).await?;
        output(result).await
    }
}

async fn output(result: ComputerActionResult) -> ToolResult {
    let paths = result.image_paths().cloned().collect::<Vec<_>>();
    if paths.is_empty() {
        return Ok(ToolOutput::json(&result));
    }
    let span = info_span!(
        target: "nanocodex_computer",
        "computer.model_input.prepare",
        computer.action.sequence = result.sequence,
        computer.model_input.image_count = paths.len(),
        computer.model_input.image_bytes = Empty,
        duration_ns = Empty,
        status = Empty,
        otel.status_code = Empty,
    );
    let started = std::time::Instant::now();
    let outcome = output_with_images(result, paths)
        .instrument(span.clone())
        .await;
    span.record("duration_ns", elapsed_ns(started.elapsed()));
    if outcome.is_ok() {
        span.record("status", "ok");
        span.record("otel.status_code", "OK");
    } else {
        span.record("status", "failed");
        span.record("otel.status_code", "ERROR");
    }
    outcome
}

async fn output_with_images(
    result: ComputerActionResult,
    paths: Vec<std::path::PathBuf>,
) -> ToolResult {
    let mut content = vec![ToolOutputContent::InputText {
        text: serde_json::to_string(&result)?,
    }];
    let mut code_mode_value = serde_json::to_value(&result)?;
    let mut image_bytes = 0_u64;
    for path in paths {
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.len() > MAX_MODEL_IMAGE_BYTES {
            return Ok(ToolOutput::error(format!(
                "computer screenshot exceeds {MAX_MODEL_IMAGE_BYTES} bytes: {}",
                path.display()
            )));
        }
        image_bytes = image_bytes.saturating_add(metadata.len());
        let image_url = format!(
            "data:image/png;base64,{}",
            STANDARD.encode(tokio::fs::read(&path).await?)
        );
        content.push(ToolOutputContent::InputImage {
            image_url: image_url.clone(),
            detail: ImageDetail::High,
        });
        if let Some(screenshot) = code_mode_value.pointer_mut("/output/state/screenshot")
            && let Some(object) = screenshot.as_object_mut()
        {
            object.insert(
                "model_image".to_owned(),
                serde_json::Value::String(image_url),
            );
        }
    }
    tracing::Span::current().record("computer.model_input.image_bytes", image_bytes);
    Ok(ToolOutput::content(content).with_code_mode_value(code_mode_value))
}

fn elapsed_ns(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
}

#[async_trait]
impl DynamicToolProvider for ComputerTool {
    fn start(&self) {}

    fn direct_tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![Arc::new(self.clone())]
    }

    fn available_definitions(&self) -> Vec<ToolDefinition> {
        Vec::new()
    }

    fn contains(&self, name: &str) -> bool {
        name == "computer"
    }

    async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        context: ToolContext<'_>,
    ) -> Option<ToolOutput> {
        if name != "computer" {
            return None;
        }
        let input = match serde_json::value::to_raw_value(&input) {
            Ok(input) => ToolInput::Function(input),
            Err(error) => {
                return Some(ToolOutput::error(format!(
                    "failed to encode computer input: {error}"
                )));
            }
        };
        Some(match Tool::execute(self, input, context).await {
            Ok(output) => output,
            Err(error) => ToolOutput::error(error.to_string()),
        })
    }
}
