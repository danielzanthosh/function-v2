use crate::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use function_platform::{create_computer_control, ComputerControl, MouseButton, TargetWindow};
use serde_json::json;
use std::sync::Arc;

pub struct MouseTool {
    control: Arc<dyn ComputerControl>,
}

impl MouseTool {
    pub fn new() -> Self {
        Self {
            control: Arc::from(create_computer_control()),
        }
    }
}

impl Default for MouseTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for MouseTool {
    fn name(&self) -> &str {
        "computer_mouse"
    }

    fn description(&self) -> &str {
        "Control the mouse. With target_window_id, send input to that app while it may remain behind other windows, without moving the user's physical pointer; coordinates are pixels in that window's computer_screen screenshot. Without target_window_id, actions use global mouse input."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["move", "click", "double_click", "right_click", "middle_click", "scroll", "drag"]
                },
                "target_window_id": { "type": "integer", "description": "Window ID from computer_screen target_windows. Scopes input to that window even when covered by another app, and does not move the user's real pointer. Requires coordinates for background actions." },
                "x": { "type": "integer", "description": "X pixel coordinate in the original computer_screen screenshot image; do not scale to logical points yourself" },
                "y": { "type": "integer", "description": "Y pixel coordinate in the original computer_screen screenshot image; do not scale to logical points yourself" },
                "delta": { "type": "integer", "description": "Scroll delta (positive for up, negative for down)" },
                "start_x": { "type": "integer", "description": "Starting X pixel coordinate in the original screenshot image" },
                "start_y": { "type": "integer", "description": "Starting Y pixel coordinate in the original screenshot image" },
                "end_x": { "type": "integer", "description": "Ending X pixel coordinate in the original screenshot image" },
                "end_y": { "type": "integer", "description": "Ending Y pixel coordinate in the original screenshot image" }
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
            .ok_or_else(|| ToolError::InvalidParameters {
                tool: self.name().into(),
                details: "Missing 'action' parameter".into(),
            })?;
        let target_id = params.get("target_window_id").map(|value| value.as_u64().and_then(|id| u32::try_from(id).ok()).ok_or_else(|| ToolError::InvalidParameters {
            tool: self.name().into(), details: "target_window_id must be a valid ID returned by computer_screen target_windows".into(),
        })).transpose()?;
        let target = if let Some(id) = target_id {
            Some(self.control.list_target_windows().map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: e.to_string() })?
                .into_iter().find(|window| window.id == id).ok_or_else(|| ToolError::ExecutionFailed {
                    tool: self.name().into(), details: "Target window closed or changed; list target windows again".into(),
                })?)
        } else { None };

        match action {
            "move" => {
                let x = params.get("x").and_then(|v| v.as_i64()).ok_or_else(|| {
                    ToolError::InvalidParameters {
                        tool: self.name().into(),
                        details: "Missing 'x' coordinate".into(),
                    }
                })? as i32;
                let y = params.get("y").and_then(|v| v.as_i64()).ok_or_else(|| {
                    ToolError::InvalidParameters {
                        tool: self.name().into(),
                        details: "Missing 'y' coordinate".into(),
                    }
                })? as i32;

                if let Some(window) = &target { self.control.target_mouse_move(window, x, y) } else { self.control.mouse_move_smooth(x, y) }
                    .map_err(|e| ToolError::ExecutionFailed {
                        tool: self.name().into(),
                        details: e.to_string(),
                    })?;
                let mut output = json!({ "x": x, "y": y, "target_window_id": target_id, "cursor_x": x, "cursor_y": y });
                self.add_preview(target.as_ref(), &mut output);
                Ok(ToolResult::success(format!("Moved mouse to ({}, {})", x, y), output))
            }
            "click" | "double_click" | "right_click" | "middle_click" => {
                // The model may provide coordinates for a click action. Previously these
                // actions ignored x/y and clicked wherever the cursor happened to be.
                let coords = (
                    params.get("x").and_then(|v| v.as_i64()),
                    params.get("y").and_then(|v| v.as_i64()),
                );
                if target.is_some() && (coords.0.is_none() || coords.1.is_none()) {
                    return Err(ToolError::InvalidParameters { tool: self.name().into(), details: "Background clicks require x and y from the selected window screenshot".into() });
                }
                if let (Some(x), Some(y)) = coords {
                    if let Some(window) = &target {
                        if action == "double_click" { self.control.target_mouse_double_click(window, x as i32, y as i32) } else { self.control.target_mouse_click(window, x as i32, y as i32, match action { "right_click" => MouseButton::Right, "middle_click" => MouseButton::Middle, _ => MouseButton::Left }) }
                    } else { self.control.mouse_move_smooth(x as i32, y as i32) }
                    .map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: format!("Failed to reach click target: {e}") })?;
                }

                let button = match action {
                    "right_click" => MouseButton::Right,
                    "middle_click" => MouseButton::Middle,
                    _ => MouseButton::Left,
                };

                if target.is_none() {
                    match action {
                        "double_click" => self.control.mouse_double_click(button),
                        _ => self.control.mouse_click(button),
                    }
                    .map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: e.to_string() })?;
                }

                let (x, y) = if target.is_some() { coords.0.zip(coords.1).map(|(x,y)| (x as i32,y as i32)).unwrap_or((0,0)) } else { self.control.get_cursor_position() };
                let mut output = json!({ "action": action, "x": x, "y": y, "button": format!("{:?}", button).to_lowercase(), "target_window_id": target_id, "cursor_x": x, "cursor_y": y });
                self.add_preview(target.as_ref(), &mut output);
                Ok(ToolResult::success(
                    format!("{} at ({}, {})", action.replace('_', " "), x, y),
                    output,
                ))
            }
            "scroll" => {
                let delta = params.get("delta").and_then(|v| v.as_i64()).unwrap_or(-1) as i32;
                let result = if let Some(window) = &target {
                    let x = params.get("x").and_then(|v| v.as_i64()).ok_or_else(|| ToolError::InvalidParameters { tool: self.name().into(), details: "Background scroll requires x and y from the selected window screenshot".into() })? as i32;
                    let y = params.get("y").and_then(|v| v.as_i64()).ok_or_else(|| ToolError::InvalidParameters { tool: self.name().into(), details: "Background scroll requires x and y from the selected window screenshot".into() })? as i32;
                    self.control.target_mouse_scroll(window, x, y, delta)
                } else { self.control.mouse_scroll(delta) };
                result.map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: e.to_string() })?;
                let mut output = json!({ "delta": delta, "target_window_id": target_id, "cursor_x": params.get("x").and_then(|v| v.as_i64()), "cursor_y": params.get("y").and_then(|v| v.as_i64()) });
                self.add_preview(target.as_ref(), &mut output);
                Ok(ToolResult::success(format!("Mouse scrolled by {}", delta), output))
            }
            "drag" => {
                if target.is_some() && ["start_x", "start_y", "end_x", "end_y"].iter().any(|key| params.get(*key).and_then(|value| value.as_i64()).is_none()) {
                    return Err(ToolError::InvalidParameters { tool: self.name().into(), details: "Background dragging requires start_x, start_y, end_x and end_y from the selected window screenshot".into() });
                }
                let sx = params.get("start_x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let sy = params.get("start_y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let ex = params.get("end_x").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let ey = params.get("end_y").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
                let result = if let Some(window) = &target { self.control.target_mouse_drag(window, sx, sy, ex, ey) } else { self.control.mouse_drag(sx, sy, ex, ey) };
                result.map_err(|e| ToolError::ExecutionFailed { tool: self.name().into(), details: e.to_string() })?;
                let mut output = json!({ "start": [sx, sy], "end": [ex, ey], "target_window_id": target_id, "cursor_x": ex, "cursor_y": ey });
                self.add_preview(target.as_ref(), &mut output);
                Ok(ToolResult::success(format!("Dragged mouse from ({}, {}) to ({}, {})", sx, sy, ex, ey), output))
            }
            other => Err(ToolError::InvalidParameters {
                tool: self.name().into(),
                details: format!("Unknown mouse action: '{}'", other),
            }),
        }
    }
}

impl MouseTool {
    fn add_preview(&self, target: Option<&TargetWindow>, output: &mut serde_json::Value) {
        if let Some(window) = target {
            output["window_id"] = json!(window.id);
            output["title"] = json!(window.title);
            if let Ok(frame) = self.control.target_window_frame(window.id) {
                use base64::Engine;
                output["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(frame));
            }
        }
    }
}
