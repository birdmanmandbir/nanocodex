use std::path::PathBuf;

use nanocodex_tools::{
    Tool, ToolContext, ToolDefinition, ToolExecution, ToolInput, ToolResult, schema_for,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{ReactDoctor, ReactReport};

const DESCRIPTION: &str = "Analyze local JavaScript, JSX, TypeScript, or TSX \
source with Nanocodex's fast Rust-native React diagnostics. The configured \
workspace root is the security boundary. Pass a relative file or directory; \
omit `path` to scan the complete root. Results are deterministic typed \
diagnostics with exact source spans. Use browser.element_context and \
browser.react_events separately for runtime evidence.";

/// Ordinary Code Mode tool backed by a reusable [`ReactDoctor`].
#[derive(Clone, Debug)]
pub struct ReactDoctorTool {
    doctor: ReactDoctor,
}

impl ReactDoctorTool {
    /// Wraps a configured analyzer as a model-visible tool.
    #[must_use]
    pub const fn new(doctor: ReactDoctor) -> Self {
        Self { doctor }
    }
}

#[async_trait::async_trait]
impl Tool for ReactDoctorTool {
    fn name(&self) -> &'static str {
        "react_doctor"
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition::function(self.name(), DESCRIPTION, schema_for::<ReactDoctorArgs>())
            .with_output_schema(schema_for::<ReactReport>())
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let arguments = input.decode_json::<ReactDoctorArgs>()?;
        let doctor = self.doctor.clone();
        let report = tokio::task::spawn_blocking(move || match arguments.path {
            Some(path) => doctor.analyze_path(path),
            None => doctor.analyze(),
        })
        .await??;
        Ok(ToolExecution::json(&report))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ReactDoctorArgs {
    /// File or directory relative to the configured workspace root.
    #[serde(default)]
    path: Option<PathBuf>,
}
