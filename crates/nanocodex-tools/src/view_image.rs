use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use nanocodex_oai_api::tools::ToolDefinition;
use serde::Deserialize;
use serde_json::json;

use super::{
    ImageDetail, StandardTool, Tool, ToolContext, ToolInput, ToolOutput, ToolOutputContent,
    ToolResult,
};

pub(crate) struct ViewImageHandler {
    workspace: PathBuf,
    max_wire_bytes: Option<u64>,
}

impl ViewImageHandler {
    pub(crate) const fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            max_wire_bytes: None,
        }
    }

    pub(crate) const fn with_wire_limit(workspace: PathBuf, max_wire_bytes: u64) -> Self {
        Self {
            workspace,
            max_wire_bytes: Some(max_wire_bytes),
        }
    }
}

#[async_trait::async_trait]
impl Tool for ViewImageHandler {
    fn definition(&self) -> ToolDefinition {
        StandardTool::ViewImage.definition()
    }

    fn supports_parallel_tool_calls(&self) -> bool {
        true
    }

    async fn execute(&self, input: ToolInput, _context: ToolContext<'_>) -> ToolResult {
        let arguments = input.decode_json::<ViewImageArguments>()?;
        let detail = match arguments.detail.as_deref() {
            None | Some("high") => ImageDetail::High,
            Some("original") => ImageDetail::Original,
            Some(detail) => {
                return Ok(ToolOutput::error(format!(
                    "view_image.detail only supports `high` or `original`; omit `detail` for default high resized behavior, got `{detail}`"
                )));
            }
        };
        let path = resolve(&self.workspace, Path::new(&arguments.path));
        let metadata = match tokio::fs::metadata(&path).await {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => {
                return Ok(ToolOutput::error(format!(
                    "image path `{}` is not a file",
                    path.display()
                )));
            }
            Err(error) => {
                return Ok(ToolOutput::error(format!(
                    "unable to locate image at `{}`: {error}",
                    path.display()
                )));
            }
        };
        if let Some(max_wire_bytes) = self.max_wire_bytes {
            let estimated_wire_bytes = estimated_wire_bytes(metadata.len());
            if estimated_wire_bytes > max_wire_bytes {
                return Ok(ToolOutput::error(format!(
                    "image at `{}` is {} bytes and its VM transfer would require at least \
                     {estimated_wire_bytes} bytes, exceeding the {max_wire_bytes}-byte VM tool \
                     frame limit; resize or convert it to a compact PNG, JPEG, or WebP inside the \
                     VM (for example with ffmpeg or ImageMagick), then call view_image on the \
                     smaller file",
                    path.display(),
                    metadata.len(),
                )));
            }
        }
        let bytes = match tokio::fs::read(&path).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(ToolOutput::error(format!(
                    "unable to read image at `{}`: {error}",
                    path.display()
                )));
            }
        };
        // The model-history boundary owns image validation, resizing, and caching.
        let image_url = format!(
            "data:application/octet-stream;base64,{}",
            STANDARD.encode(bytes)
        );
        Ok(ToolOutput::content(vec![ToolOutputContent::InputImage {
            image_url: image_url.clone(),
            detail,
        }])
        .with_code_mode_value(json!({
            "image_url": image_url,
            "detail": detail,
        })))
    }
}

const DATA_URL_PREFIX_BYTES: u64 = 37;
const VIEW_IMAGE_WIRE_COPIES: u64 = 2;
const VIEW_IMAGE_WIRE_HEADROOM_BYTES: u64 = 4 * 1024;

fn estimated_wire_bytes(raw_bytes: u64) -> u64 {
    raw_bytes
        .div_ceil(3)
        .checked_mul(4)
        .and_then(|encoded| encoded.checked_add(DATA_URL_PREFIX_BYTES))
        .and_then(|data_url| data_url.checked_mul(VIEW_IMAGE_WIRE_COPIES))
        .and_then(|duplicated| duplicated.checked_add(VIEW_IMAGE_WIRE_HEADROOM_BYTES))
        .unwrap_or(u64::MAX)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewImageArguments {
    path: String,
    #[serde(default)]
    detail: Option<String>,
}

fn resolve(workspace: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_owned()
    } else {
        workspace.join(path)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use nanocodex_oai_api::tools::{ToolInput, ToolOutputBody};
    use serde_json::{json, value::to_raw_value};
    use tempfile::tempdir;

    use super::*;

    const VM_FRAME_BYTES: u64 = 64 * 1024 * 1024;
    const PATH_TRACING_IMAGE_BYTES: u64 = 48_262_737;

    #[tokio::test]
    async fn vm_wire_limit_rejects_the_path_tracing_image_before_reading_it() {
        let workspace = tempdir().unwrap();
        let image = workspace.path().join("image.ppm");
        File::create(&image)
            .unwrap()
            .set_len(PATH_TRACING_IMAGE_BYTES)
            .unwrap();
        let handler =
            ViewImageHandler::with_wire_limit(workspace.path().to_owned(), VM_FRAME_BYTES);
        let output = handler
            .execute(
                ToolInput::Function(
                    to_raw_value(&json!({
                        "path": image,
                        "detail": "original",
                    }))
                    .unwrap(),
                ),
                ToolContext::new("model", "session", "call", &[], 10_000),
            )
            .await
            .unwrap();

        assert!(!output.success);
        let ToolOutputBody::Text(error) = output.output else {
            panic!("oversized VM image should return a bounded text error");
        };
        assert!(error.contains("48262737 bytes"));
        assert!(error.contains("VM tool frame limit"));
        assert!(error.contains("resize or convert"));
        assert!(error.contains("PNG, JPEG, or WebP"));
    }

    #[test]
    fn wire_estimate_accounts_for_both_data_url_copies() {
        assert_eq!(estimated_wire_bytes(PATH_TRACING_IMAGE_BYTES), 128_704_802);
        assert!(estimated_wire_bytes(PATH_TRACING_IMAGE_BYTES) > VM_FRAME_BYTES);
    }
}
