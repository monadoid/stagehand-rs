use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Default schema used for extraction responses.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DefaultExtractSchema {
    pub extraction: String,
}

/// Minimal schema representing a simple page text extraction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EmptyExtractSchema {
    pub page_text: String,
}

/// Serialized representation of an observed element.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserveElementSchema {
    #[serde(rename = "element_id")]
    pub element_id: i64,
    pub description: String,
    pub method: String,
    pub arguments: Vec<String>,
}

/// Collection of observed elements returned by inference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ObserveInferenceSchema {
    pub elements: Vec<ObserveElementSchema>,
}

/// Metadata attached to Stagehand operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetadataSchema {
    pub completed: bool,
    pub progress: String,
}

/// Options controlling Stagehand's `act` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActOptions {
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variables: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom_settle_timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_client_options: Option<Value>,
}

/// Result returned after executing Stagehand's `act` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ActResult {
    pub success: bool,
    pub message: String,
    pub action: String,
}

/// Options controlling Stagehand's `observe` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ObserveOptions {
    pub instruction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draw_overlay: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom_settle_timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_client_options: Option<Value>,
}

/// Result returned from the `observe` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ObserveResult {
    pub selector: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_node_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<String>>,
}

/// Options controlling the `extract` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtractOptions {
    pub instruction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_definition: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_text_extract: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dom_settle_timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_client_options: Option<Value>,
}

/// Result returned from the `extract` command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ExtractResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}
