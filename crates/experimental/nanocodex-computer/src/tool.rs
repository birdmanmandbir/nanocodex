use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use nanocodex_oai_api::ImageDetail;
use nanocodex_tools::{
    Tool, ToolContext, ToolDefinition, ToolInput, ToolOutput, ToolResult,
    contract::ToolOutputContent,
    runtime::{DynamicToolProvider, schema_for},
};

use crate::{Computer, ComputerAction, ComputerActionResult};

const MAX_MODEL_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

const TOOL_DESCRIPTION: &str = r"Control one user-selected native macOS application.

Call from Code Mode as `await tools.computer({ action: ..., ... })`. This is a
state-first tool: list applications, attach to one exact app/window, then
observe before acting. Observations return a screenshot and compact
accessibility elements with generation-bound references such as `e12_4`.
References expire on the next observation or action; always use fresh ones.

Prefer semantic element click/set_value/perform_action over coordinates.
Coordinate input is global and is intended only for controls absent from the
accessibility tree. Mutating actions settle and return fresh state, so inspect
that state before continuing when the next step depends on the UI. Compose
sequential operations in one Code Mode cell, but never use Promise.all: one
session deliberately serializes native input.

The human can pause or intervene independently. A paused error means stop
issuing actions until the host resumes control. Never attempt to bypass macOS
Accessibility or Screen Recording permission errors.";

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
    let mut content = vec![ToolOutputContent::InputText {
        text: serde_json::to_string(&result)?,
    }];
    let mut code_mode_value = serde_json::to_value(&result)?;
    for path in paths {
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.len() > MAX_MODEL_IMAGE_BYTES {
            return Ok(ToolOutput::error(format!(
                "computer screenshot exceeds {MAX_MODEL_IMAGE_BYTES} bytes: {}",
                path.display()
            )));
        }
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
    Ok(ToolOutput::content(content).with_code_mode_value(code_mode_value))
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
