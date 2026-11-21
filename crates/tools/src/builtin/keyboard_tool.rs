use crate::{Tool, ToolContext, ToolError, ToolResult};
use async_trait::async_trait;
use function_platform::{create_computer_control, ComputerControl, TargetWindow};
use serde_json::json;
use std::sync::Arc;

pub struct KeyboardTool {
    control: Arc<dyn ComputerControl>,
}

impl KeyboardTool {
    pub fn new() -> Self {
        Self {
            control: Arc::from(create_computer_control()),
        }
    }
}

impl Default for KeyboardTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for KeyboardTool {
    fn name(&self) -> &str {
        "computer_keyboard"
    }

    fn description(&self) -> &str {
        "Type text or press keys. Set target_window_id to send input to that app while it remains in the background, without typing into the user's foreground app."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["type", "press", "shortcut"]
                },
                "text": { "type": "string", "description": "Text to type" },
                "key": { "type": "string", "description": "Key to press (e.g. 'enter', 'tab', 'esc', 'space')" },
                "keys": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Combination of keys for a shortcut, e.g. ['ctrl', 'shift', 'p']"
                },
                "target_window_id": { "type": "integer", "description": "Window ID from computer_screen target_windows; scopes key input to that app even when another app is in front." }
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
            "type" => {
                let text = params.get("text").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::InvalidParameters {
                        tool: self.name().into(),
                        details: "Missing 'text' parameter".into(),
                    }
                })?;
                let result = if let Some(window) = &target { self.control.target_keyboard_type(window, text) } else { self.control.keyboard_type_smooth(text) };
                result.map_err(|e| {
                    ToolError::ExecutionFailed {
                        tool: self.name().into(),
                        details: e.to_string(),
                    }
                })?;
                let mut output = json!({ "typed_length": text.len(), "target_window_id": target_id });
                self.add_preview(target.as_ref(), &mut output);
                Ok(ToolResult::success(
                    format!("Typed {} characters", text.len()),
                    output,
                ))
            }
            "press" => {
                let key = params.get("key").and_then(|v| v.as_str()).ok_or_else(|| {
                    ToolError::InvalidParameters {
                        tool: self.name().into(),
                        details: "Missing 'key' parameter".into(),
                    }
                })?;
                let result = if let Some(window) = &target { self.control.target_keyboard_press(window, key) } else { self.control.keyboard_press(key) };
                result
                    .map_err(|e| ToolError::ExecutionFailed {
                        tool: self.name().into(),
                        details: e.to_string(),
                    })?;
                let mut output = json!({ "key": key, "target_window_id": target_id });
                self.add_preview(target.as_ref(), &mut output);
                Ok(ToolResult::success(
                    format!("Pressed key '{}'", key),
                    output,
                ))
            }
            "shortcut" => {
                let keys_arr = params
                    .get("keys")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| ToolError::InvalidParameters {
                        tool: self.name().into(),
                        details: "Missing 'keys' array parameter".into(),
                    })?;
                let keys: Vec<&str> = keys_arr.iter().filter_map(|k| k.as_str()).collect();
                let result = if let Some(window) = &target { self.control.target_keyboard_shortcut(window, &keys) } else { self.control.keyboard_shortcut(&keys) };
                result
                    .map_err(|e| ToolError::ExecutionFailed {
                        tool: self.name().into(),
                        details: e.to_string(),
                    })?;
                let combo = keys.join("+");
                let mut output = json!({ "shortcut": combo, "target_window_id": target_id });
                self.add_preview(target.as_ref(), &mut output);
                Ok(ToolResult::success(
                    format!("Sent shortcut '{}'", combo),
                    output,
                ))
            }
            other => Err(ToolError::InvalidParameters {
                tool: self.name().into(),
                details: format!("Unknown keyboard action: '{}'", other),
            }),
        }
    }
}

impl KeyboardTool {
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
