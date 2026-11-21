use crate::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use function_platform::{create_computer_control, ComputerControl};
use serde_json::json;
use std::sync::Arc;

pub struct ScreenTool {
    control: Arc<dyn ComputerControl>,
}

impl ScreenTool {
    pub fn new() -> Self {
        Self {
            control: Arc::from(create_computer_control()),
        }
    }
}

impl Default for ScreenTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for ScreenTool {
    fn name(&self) -> &str {
        "computer_screen"
    }

    fn description(&self) -> &str {
        "Inspect screen resolution or cursor coordinates, capture the desktop, or list, observe, and inspect an individual app window without activating it."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["dimensions", "cursor_position", "screenshot", "target_windows", "target_screenshot", "target_accessibility", "target_accessibility_action"],
                    "description": "Use target_windows to list capturable app windows. target_screenshot observes one window without bringing it to the front, including when another app covers it. target_accessibility inspects its controls; target_accessibility_action operates an observed control. Use screenshot for the full desktop."
                },
                "window_id": { "type": "integer", "description": "Window ID returned by target_windows" },
                "ref": { "type": "string", "description": "Element ref returned by target_accessibility" },
                "operation": { "type": "string", "enum": ["press", "focus", "set_value"] },
                "value": { "type": "string", "description": "Value to set on an accessibility text field" }
            },
            "required": ["action"]
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &ToolContext,
    ) -> Result<ToolResult, ToolError> {
        let action = params
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("screenshot");

        match action {
            "target_accessibility" => {
                let window_id = required_window_id(&params, self.name())?;
                let mut snapshot = self.control.target_accessibility_snapshot(window_id).map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: e.to_string() })?;
                if let Some(object) = snapshot.as_object_mut() {
                    let title = self.control.list_target_windows().ok()
                        .and_then(|windows| windows.into_iter().find(|window| window.id == window_id).map(|window| window.title));
                    object.insert("title".into(), json!(title));
                }
                Ok(ToolResult::success(format!("Read accessible controls in window {window_id}"), snapshot))
            }
            "target_accessibility_action" => {
                let window_id = required_window_id(&params, self.name())?;
                let ref_id = params.get("ref").and_then(|value| value.as_str()).ok_or_else(|| ToolError::InvalidParameters { tool: self.name().into(), details: "ref must come from a recent target_accessibility snapshot".into() })?;
                let operation = params.get("operation").and_then(|value| value.as_str()).ok_or_else(|| ToolError::InvalidParameters { tool: self.name().into(), details: "operation is required".into() })?;
                let value = params.get("value").and_then(|value| value.as_str());
                let output = self.control.target_accessibility_action(window_id, ref_id, operation, value).map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: e.to_string() })?;
                Ok(ToolResult::success(format!("Applied accessibility action '{operation}'"), output))
            }
            "target_windows" => {
                let windows = self.control.list_target_windows().map_err(|e| ToolError::ExecutionFailed {
                    tool: self.name().into(), details: e.to_string(),
                })?;
                let output = serde_json::to_value(&windows).unwrap_or_else(|_| json!([]));
                Ok(ToolResult::success(format!("Found {} capturable application windows", windows.len()), json!({ "windows": output })))
            }
            "target_screenshot" => {
                let window_id = params.get("window_id").and_then(|v| v.as_u64()).and_then(|v| u32::try_from(v).ok()).ok_or_else(|| ToolError::InvalidParameters {
                    tool: self.name().into(), details: "window_id must be a valid ID from target_windows".into(),
                })?;
                let window = self.control.list_target_windows().map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: e.to_string() })?
                    .into_iter().find(|window| window.id == window_id).ok_or_else(|| ToolError::ExecutionFailed {
                        tool: self.name().into(), details: "Target window closed or changed; list target windows again".into(),
                    })?;
                let frame = self.control.target_window_frame(window_id).map_err(|e| ToolError::ExecutionFailed {
                    tool: self.name().into(), details: e.to_string(),
                })?;
                let (width, height) = png_dimensions(&frame).unwrap_or((0, 0));
                Ok(ToolResult::success(
                    format!("Captured target window {window_id} ({width}x{height})"),
                    json!({ "window_id": window_id, "title": window.title, "width": width, "height": height, "base64": to_base64(&frame) }),
                ))
            }
            "screenshot" => {
                let dims = self.control.get_screen_dimensions();
                match self.control.take_screenshot() {
                    Ok(bytes) => {
                        let b64 = to_base64(&bytes);

                        // Screenshot bytes remain in memory and are passed directly to the model.
                        // Do not persist temporary visual context in the user's attachments folder.
                        Ok(ToolResult::success(
                            format!(
                                "Screenshot captured successfully ({}x{} image pixels; screen input space {}x{} logical points; {} KB)",
                                png_dimensions(&bytes).map(|d| d.0).unwrap_or(dims.width),
                                png_dimensions(&bytes).map(|d| d.1).unwrap_or(dims.height),
                                dims.width,
                                dims.height,
                                bytes.len() / 1024
                            ),
                            {
                                let (pixel_width, pixel_height) = png_dimensions(&bytes).unwrap_or((dims.width, dims.height));
                                json!({
                                    "status": "success",
                                    "width": pixel_width,
                                    "height": pixel_height,
                                    "pixel_width": pixel_width,
                                    "pixel_height": pixel_height,
                                    "screen_width": dims.width,
                                    "screen_height": dims.height,
                                    "coordinate_space": "pixel coordinates in returned screenshot image; mouse tool converts once to logical screen points",
                                    "base64": b64,
                                })
                            },
                        ))
                    }
                    Err(e) => Ok(ToolResult::failure(
                        "Failed to capture screen",
                        format!("{}", e),
                    )),
                }
            }
            "dimensions" => {
                let dims = self.control.get_screen_dimensions();
                Ok(ToolResult::success(
                    format!("Screen resolution: {}x{}", dims.width, dims.height),
                    json!({ "width": dims.width, "height": dims.height }),
                ))
            }
            "cursor_position" => {
                let (x, y) = self.control.get_cursor_position();
                Ok(ToolResult::success(
                    format!("Cursor position: ({}, {})", x, y),
                    json!({ "cursor_x": x, "cursor_y": y }),
                ))
            }
            other => Err(ToolError::InvalidParameters {
                tool: self.name().into(),
                details: format!("Unknown screen action: '{}'", other),
            }),
        }
    }
}

fn required_window_id(params: &serde_json::Value, tool: &str) -> Result<u32, ToolError> {
    params.get("window_id").and_then(|value| value.as_u64()).and_then(|value| u32::try_from(value).ok()).ok_or_else(|| ToolError::InvalidParameters {
        tool: tool.into(), details: "window_id must be a valid ID returned by target_windows".into(),
    })
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() >= 24 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        if width > 0 && height > 0 { return Some((width, height)); }
    }
    None
}

fn to_base64(data: &[u8]) -> String {
    const CHARSET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = chunk.get(1).copied().unwrap_or(0) as usize;
        let b2 = chunk.get(2).copied().unwrap_or(0) as usize;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARSET[(triple >> 18) & 0x3F] as char);
        result.push(CHARSET[(triple >> 12) & 0x3F] as char);
        if chunk.len() > 1 {
            result.push(CHARSET[(triple >> 6) & 0x3F] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARSET[triple & 0x3F] as char);
        } else {
            result.push('=');
        }
    }
    result
}
