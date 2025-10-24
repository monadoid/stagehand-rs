use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Agent configuration prior to execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<HashMap<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClickAction {
    pub x: i32,
    pub y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DoubleClickAction {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TypeAction {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub y: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub press_enter_after: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyPressAction {
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScrollAction {
    pub x: i32,
    pub y: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll_x: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scroll_y: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DragAction {
    pub path: Vec<Point>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MoveAction {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct WaitAction {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub miliseconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ScreenshotAction;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionArguments {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionAction {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<FunctionArguments>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KeyAction {
    pub text: String,
}

/// Union of all supported agent actions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
pub enum AgentActionPayload {
    #[serde(rename = "click")]
    Click(ClickAction),
    #[serde(rename = "double_click", alias = "doubleClick")]
    DoubleClick(DoubleClickAction),
    #[serde(rename = "type")]
    Type(TypeAction),
    #[serde(rename = "keypress")]
    Keypress(KeyPressAction),
    #[serde(rename = "scroll")]
    Scroll(ScrollAction),
    #[serde(rename = "drag")]
    Drag(DragAction),
    #[serde(rename = "move")]
    Move(MoveAction),
    #[serde(rename = "wait")]
    Wait(WaitAction),
    #[serde(rename = "screenshot")]
    Screenshot(ScreenshotAction),
    #[serde(rename = "function")]
    Function(FunctionAction),
    #[serde(rename = "key")]
    Key(KeyAction),
}

/// Wrapper describing a single agent action with metadata.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentAction {
    #[serde(rename = "action_type")]
    pub action_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    pub action: AgentActionPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<Vec<HashMap<String, Value>>>,
}

/// Usage statistics produced by an agent run.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub inference_time_ms: i64,
}

/// Summary of an agent execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentResult {
    pub actions: Vec<AgentActionPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<AgentUsage>,
    pub completed: bool,
}

/// Result from executing a single agent action.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionExecutionResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Additional options passed to the agent SDK client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentClientOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_between_actions: Option<u64>,
}

/// Handler-level configuration for agent execution.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentHandlerOptions {
    pub model_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_options: Option<AgentClientOptions>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_provided_instructions: Option<String>,
}

/// User-provided execution parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentExecuteOptions {
    pub instruction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_steps: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_screenshot: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_between_actions: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
}

/// Captured environment state for the agent loop.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EnvState {
    pub screenshot: Vec<u8>,
    pub url: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialize_double_click_alias() {
        let payload: AgentActionPayload =
            serde_json::from_str(r#"{ "type": "doubleClick", "x": 10, "y": 20 }"#)
                .expect("doubleClick alias");

        if let AgentActionPayload::DoubleClick(action) = payload {
            assert_eq!(action.x, 10);
            assert_eq!(action.y, 20);
        } else {
            panic!("expected double click variant");
        }
    }

    #[test]
    fn serialize_agent_action() {
        let action = AgentActionPayload::Click(ClickAction {
            x: 1,
            y: 2,
            button: Some("left".into()),
        });

        let json = serde_json::to_value(&action).expect("serialize");
        assert_eq!(
            json,
            json!({
                "type": "click",
                "x": 1,
                "y": 2,
                "button": "left"
            })
        );
    }
}
