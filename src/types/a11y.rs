use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

/// AX property key/value pair as returned by Chromium accessibility snapshots.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AxProperty {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// AX value wrapper that exposes both the primitive type and the value itself.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AxValue {
    #[serde(rename = "type")]
    pub value_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

/// Raw accessibility node shape returned by the Chrome DevTools Protocol.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AxNode {
    #[serde(rename = "nodeId")]
    pub node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<AxValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<AxValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<AxValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<AxValue>,
    #[serde(rename = "backendDOMNodeId", skip_serializing_if = "Option::is_none")]
    pub backend_dom_node_id: Option<i64>,
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(rename = "childIds", skip_serializing_if = "Option::is_none")]
    pub child_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<AxProperty>>,
}

/// Simplified accessibility node used by higher-level algorithms.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct AccessibilityNode {
    #[serde(rename = "nodeId", skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(rename = "backendDOMNodeId", skip_serializing_if = "Option::is_none")]
    pub backend_dom_node_id: Option<i64>,
    #[serde(rename = "parentId", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(rename = "childIds", skip_serializing_if = "Option::is_none")]
    pub child_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub children: Option<Vec<AccessibilityNode>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<Vec<AxProperty>>,
}

/// Combined accessibility tree result returned by Stagehand.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TreeResult {
    pub tree: Vec<AccessibilityNode>,
    pub simplified: String,
    pub iframes: Vec<AccessibilityNode>,
    #[serde(rename = "idToUrl")]
    pub id_to_url: HashMap<String, String>,
}

/// Placeholder type aliases corresponding to Playwright/CDP handles in Python.
pub type CdpSession = Value;
pub type Locator = Value;

/// Error reported when the Playwright command surface raises failures.
#[derive(Debug, Error)]
#[error("Playwright command failed: {0}")]
pub struct PlaywrightCommandError(pub String);

/// Error reported when a requested Playwright method is not supported.
#[derive(Debug, Error)]
#[error("Playwright method not supported: {0}")]
pub struct PlaywrightMethodNotSupportedError(pub String);
