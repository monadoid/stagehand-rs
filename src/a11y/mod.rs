use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::future::join_all;
use serde_json::{Value, json};
use thiserror::Error;

use crate::logging::StagehandLogger;
use crate::types::{AccessibilityNode, AxNode, AxValue, TreeResult};

#[derive(Debug, Error)]
pub enum AccessibilityError {
    #[error("CDP operation failed: {0}")]
    Cdp(String),
    #[error("evaluation failed: {0}")]
    Evaluation(String),
    #[error("unexpected response: {0}")]
    Unexpected(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[async_trait]
pub trait AccessibilityPage: Send + Sync {
    async fn ensure_injection(&self) -> Result<(), AccessibilityError>;

    async fn evaluate(&self, expression: &str) -> Result<Value, AccessibilityError>;

    async fn send_cdp(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<Value, AccessibilityError>;

    async fn disable_cdp_domain(&self, domain: &str) -> Result<(), AccessibilityError>;

    async fn new_cdp_session(
        &self,
    ) -> Result<Box<dyn AccessibilitySession + Send>, AccessibilityError>;
}

#[async_trait]
pub trait AccessibilitySession: Send {
    async fn send(&self, method: &str, params: Option<Value>) -> Result<Value, AccessibilityError>;

    async fn detach(&self) -> Result<(), AccessibilityError>;
}

const GET_NODE_PATH_FUNCTION: &str = r##"
function getNodePath(el) {
  if (!el || (el.nodeType !== Node.ELEMENT_NODE && el.nodeType !== Node.TEXT_NODE)) {
    console.log("el is not a valid node type");
    return "";
  }

  const parts = [];
  let current = el;

  while (current && (current.nodeType === Node.ELEMENT_NODE || current.nodeType === Node.TEXT_NODE)) {
    let index = 0;
    let hasSameTypeSiblings = false;
    const siblings = current.parentElement
      ? Array.from(current.parentElement.childNodes)
      : [];

    for (let i = 0; i < siblings.length; i++) {
      const sibling = siblings[i];
      if (
        sibling.nodeType === current.nodeType &&
        sibling.nodeName === current.nodeName
      ) {
        index = index + 1;
        hasSameTypeSiblings = true;
        if (sibling.isSameNode(current)) {
          break;
        }
      }
    }

    if (!current || !current.parentNode) break;
    if (current.nodeName.toLowerCase() === "html"){
      parts.unshift("html");
      break;
    }

    if (current.nodeName !== "#text") {
      const tagName = current.nodeName.toLowerCase();
      const pathIndex = hasSameTypeSiblings ? `[${index}]` : "";
      parts.unshift(`${tagName}${pathIndex}`);
    }
    
    current = current.parentElement;
  }

  return parts.length ? `/${parts.join("/")}` : "";
}
"##;

const TAG_NAME_FUNCTION: &str =
    "function() { return this.tagName ? this.tagName.toLowerCase() : \"\"; }";

pub fn format_simplified_tree(node: &AccessibilityNode, level: usize) -> String {
    let indent = "  ".repeat(level);
    let node_id = node.node_id.clone().unwrap_or_else(|| "None".to_string());
    let role = node.role.clone().unwrap_or_else(|| "None".to_string());
    let name_part = node
        .name
        .as_ref()
        .filter(|name| !name.is_empty())
        .map(|name| format!(": {name}"))
        .unwrap_or_default();
    let mut result = format!("{indent}[{node_id}] {role}{name_part}\n");
    if let Some(children) = node.children.as_ref() {
        for child in children {
            result.push_str(&format_simplified_tree(child, level + 1));
        }
    }
    result
}

fn normalize_whitespace(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn remove_redundant_static_text_children(
    parent: &AccessibilityNode,
    children: Vec<AccessibilityNode>,
) -> Vec<AccessibilityNode> {
    let Some(target_name) = parent
        .name
        .as_ref()
        .map(|name| normalize_whitespace(name))
        .filter(|name| !name.is_empty())
    else {
        return children;
    };

    let combined = children
        .iter()
        .filter(|child| child.role.as_deref() == Some("StaticText"))
        .filter_map(|child| child.name.as_ref())
        .fold(String::new(), |mut acc, name| {
            let normalized = normalize_whitespace(name);
            if !normalized.is_empty() {
                acc.push_str(&normalized);
            }
            acc
        });

    if combined == target_name {
        children
            .into_iter()
            .filter(|child| {
                if child.role.as_deref() != Some("StaticText") {
                    return true;
                }
                match child.name.as_ref() {
                    Some(name) => name.is_empty(),
                    None => true,
                }
            })
            .collect()
    } else {
        children
    }
}

fn extract_url_from_ax_node(ax_node: &AxNode) -> Option<String> {
    let properties = ax_node.properties.as_ref()?;
    for prop in properties {
        if prop.name != "url" {
            continue;
        }
        let value_obj = prop.value.as_ref()?.as_object()?;
        let url_value = value_obj.get("value")?;
        if let Some(url) = url_value.as_str() {
            return Some(url.trim().to_string());
        }
    }
    None
}

fn json_value_to_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn build_subtree(
    node_id: &str,
    node_map: &HashMap<String, AccessibilityNode>,
    visiting: &mut HashSet<String>,
) -> Option<AccessibilityNode> {
    if !visiting.insert(node_id.to_string()) {
        return node_map.get(node_id).cloned();
    }

    let mut node = node_map.get(node_id)?.clone();
    let child_ids = node.child_ids.clone().unwrap_or_default();
    let mut children = Vec::new();

    for child_id in child_ids {
        if let Some(child) = build_subtree(&child_id, node_map, visiting) {
            children.push(child);
        }
    }

    if !children.is_empty() {
        node.children = Some(children);
    }

    visiting.remove(node_id);
    Some(node)
}

async fn clean_structural_nodes<P: AccessibilityPage + ?Sized>(
    mut node: AccessibilityNode,
    page: Option<&P>,
    logger: Option<&StagehandLogger>,
) -> Result<Option<AccessibilityNode>, AccessibilityError> {
    if let Some(node_id) = node.node_id.as_ref() {
        if node_id
            .parse::<i64>()
            .map(|value| value < 0)
            .unwrap_or(false)
        {
            return Ok(None);
        }
    }

    let children = node.children.take().unwrap_or_default();
    if children.is_empty() {
        if matches!(node.role.as_deref(), Some("generic" | "none")) {
            return Ok(None);
        }
        return Ok(Some(node));
    }

    let futures = children
        .into_iter()
        .map(|child| clean_structural_nodes(child, page, logger));
    let results = join_all(futures).await;

    let mut cleaned_children = Vec::new();
    for result in results {
        if let Some(child) = result? {
            cleaned_children.push(child);
        }
    }

    if matches!(node.role.as_deref(), Some("generic" | "none")) {
        if cleaned_children.len() == 1 {
            return Ok(cleaned_children.into_iter().next());
        } else if cleaned_children.is_empty() {
            return Ok(None);
        }
    }

    if let (Some(page), Some(logger), Some(backend_id)) = (page, logger, node.backend_dom_node_id) {
        if matches!(node.role.as_deref(), Some("generic" | "combobox" | "none")) {
            if let Some(resolved) =
                resolve_structural_role(page, backend_id, logger, node.role.as_deref()).await
            {
                node.role = Some(resolved);
            }
        }
    }

    let cleaned_children = remove_redundant_static_text_children(&node, cleaned_children);

    if cleaned_children.is_empty() {
        if matches!(node.role.as_deref(), Some("generic" | "none")) {
            return Ok(None);
        }
        node.children = Some(Vec::new());
        return Ok(Some(node));
    }

    node.children = Some(cleaned_children);
    Ok(Some(node))
}

async fn resolve_structural_role<P: AccessibilityPage + ?Sized>(
    page: &P,
    backend_node_id: i64,
    logger: &StagehandLogger,
    current_role: Option<&str>,
) -> Option<String> {
    let resolve_result = match page
        .send_cdp(
            "DOM.resolveNode",
            Some(json!({ "backendNodeId": backend_node_id })),
        )
        .await
    {
        Ok(value) => value,
        Err(err) => {
            logger.debug(
                format!("Could not resolve DOM node ID {backend_node_id}"),
                Some("a11y"),
                Some(json!({ "error": err.to_string() })),
            );
            return None;
        }
    };

    let object_id = resolve_result
        .get("object")
        .and_then(|object| object.get("objectId"))
        .and_then(Value::as_str)
        .map(|value| value.to_string());

    let Some(object_id) = object_id else {
        return None;
    };

    let call_result = match page
        .send_cdp(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId": object_id,
                "functionDeclaration": TAG_NAME_FUNCTION,
                "returnByValue": true,
            })),
        )
        .await
    {
        Ok(value) => value,
        Err(err) => {
            logger.debug(
                format!("Could not fetch tagName for node {backend_node_id}"),
                Some("a11y"),
                Some(json!({ "error": err.to_string() })),
            );
            return None;
        }
    };

    let tag_name = call_result
        .get("result")
        .and_then(|result| result.get("value"))
        .and_then(Value::as_str)
        .map(|value| value.to_lowercase());

    if let Some(mut tag_name) = tag_name {
        if matches!(current_role, Some("combobox")) && tag_name == "select" {
            tag_name = "select".to_string();
        }
        return Some(tag_name);
    }

    None
}

pub async fn build_hierarchical_tree<P: AccessibilityPage + ?Sized>(
    nodes: Vec<AxNode>,
    page: Option<&P>,
    logger: Option<&StagehandLogger>,
) -> Result<TreeResult, AccessibilityError> {
    let mut id_to_url = HashMap::new();
    let mut node_map: HashMap<String, AccessibilityNode> = HashMap::new();
    let mut iframe_list = Vec::new();

    for node_data in nodes {
        let node_id = node_data.node_id.clone();
        if node_id
            .parse::<i64>()
            .map(|value| value < 0)
            .unwrap_or(false)
        {
            continue;
        }

        if let Some(url) = extract_url_from_ax_node(&node_data) {
            id_to_url.insert(node_id.clone(), url);
        }

        let has_children = node_data
            .child_ids
            .as_ref()
            .map(|child_ids| !child_ids.is_empty())
            .unwrap_or(false);

        let name_value = node_data
            .name
            .as_ref()
            .and_then(|name| name.value.as_ref())
            .map(json_value_to_string);

        let has_valid_name = name_value
            .as_ref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);

        let role_value = node_data
            .role
            .as_ref()
            .and_then(|role| role.value.as_ref())
            .map(json_value_to_string)
            .unwrap_or_default();

        let is_interactive = !matches!(role_value.as_str(), "none" | "generic" | "InlineTextBox");

        if !has_valid_name && !has_children && !is_interactive {
            continue;
        }

        let mut processed_node = AccessibilityNode::default();
        processed_node.node_id = Some(node_id.clone());
        processed_node.role = Some(role_value.clone());

        if let Some(name) = name_value {
            processed_node.name = Some(name);
        }

        if let Some(description) = node_data
            .description
            .as_ref()
            .and_then(|description| description.value.as_ref())
            .map(json_value_to_string)
        {
            processed_node.description = Some(description);
        }

        if let Some(value) = node_data
            .value
            .as_ref()
            .and_then(|value| value.value.as_ref())
            .map(json_value_to_string)
        {
            processed_node.value = Some(value);
        }

        processed_node.backend_dom_node_id = node_data.backend_dom_node_id;
        processed_node.parent_id = node_data.parent_id.clone();
        processed_node.child_ids = node_data.child_ids.clone();
        processed_node.properties = node_data.properties.clone();

        if processed_node.role.as_deref() == Some("Iframe") {
            iframe_list.push(AccessibilityNode {
                node_id: Some(node_id.clone()),
                role: Some("Iframe".to_string()),
                ..AccessibilityNode::default()
            });
        }

        node_map.insert(node_id, processed_node);
    }

    let mut root_nodes = Vec::new();
    let root_ids: Vec<String> = node_map
        .iter()
        .filter_map(|(id, node)| {
            if node.parent_id.is_none() {
                Some(id.clone())
            } else {
                None
            }
        })
        .collect();

    for root_id in root_ids {
        let mut visiting = HashSet::new();
        if let Some(built) = build_subtree(&root_id, &node_map, &mut visiting) {
            root_nodes.push(built);
        }
    }

    let cleaning_futures = root_nodes
        .into_iter()
        .map(|node| clean_structural_nodes(node, page, logger));
    let cleaned_results = join_all(cleaning_futures).await;

    let mut final_tree = Vec::new();
    for result in cleaned_results {
        if let Some(node) = result? {
            final_tree.push(node);
        }
    }

    let simplified = final_tree
        .iter()
        .map(|node| format_simplified_tree(node, 0))
        .collect::<Vec<_>>()
        .join("\n");

    Ok(TreeResult {
        tree: final_tree,
        simplified,
        iframes: iframe_list,
        id_to_url,
    })
}

pub async fn get_accessibility_tree<P: AccessibilityPage + ?Sized>(
    page: &P,
    logger: &StagehandLogger,
) -> Result<TreeResult, AccessibilityError> {
    let start = Instant::now();

    let result: Result<(TreeResult, Duration), AccessibilityError> = async {
        let scrollable_backend_ids = find_scrollable_element_ids(page, Some(logger)).await;

        let cdp_result = page.send_cdp("Accessibility.getFullAXTree", None).await?;
        let nodes_value = cdp_result
            .get("nodes")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let mut nodes: Vec<AxNode> = serde_json::from_value(nodes_value)?;

        let processing_start = Instant::now();

        for node in nodes.iter_mut() {
            if let Some(backend_id) = node.backend_dom_node_id {
                if scrollable_backend_ids.contains(&backend_id) {
                    let current_role = node
                        .role
                        .as_ref()
                        .and_then(|role| role.value.as_ref())
                        .and_then(Value::as_str)
                        .unwrap_or_default();

                    let new_role =
                        if current_role.is_empty() || matches!(current_role, "generic" | "none") {
                            "scrollable".to_string()
                        } else {
                            format!("scrollable, {current_role}")
                        };

                    let entry = node.role.get_or_insert_with(|| AxValue {
                        value_type: "string".to_string(),
                        value: None,
                    });
                    entry.value = Some(Value::String(new_role));
                }
            }
        }

        let tree = build_hierarchical_tree(nodes, Some(page), Some(logger)).await?;
        let processing_duration = processing_start.elapsed();
        Ok((tree, processing_duration))
    }
    .await;

    if let Err(disable_err) = page.disable_cdp_domain("Accessibility").await {
        logger.debug(
            "Failed to disable Accessibility domain on cleanup.",
            Some("a11y"),
            Some(json!({ "error": disable_err.to_string() })),
        );
    }

    match result {
        Ok((tree, processing_duration)) => {
            let total_duration = start.elapsed();
            logger.debug(
                format!(
                    "got accessibility tree in {}ms (processing: {}ms)",
                    total_duration.as_millis(),
                    processing_duration.as_millis()
                ),
                Some("a11y"),
                None,
            );
            Ok(tree)
        }
        Err(err) => {
            logger.error(
                "Error getting accessibility tree",
                Some("a11y"),
                Some(json!({ "error": err.to_string() })),
            );
            Err(err)
        }
    }
}

pub async fn find_scrollable_element_ids<P: AccessibilityPage + ?Sized>(
    page: &P,
    logger: Option<&StagehandLogger>,
) -> HashSet<i64> {
    let mut xpaths: Vec<String> = Vec::new();

    match page.ensure_injection().await {
        Ok(_) => match page
            .evaluate("() => window.getScrollableElementXpaths()")
            .await
        {
            Ok(Value::Array(items)) => {
                xpaths = items
                    .into_iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect();
            }
            Ok(other) => {
                if let Some(logger) = logger {
                    logger.debug(
                        "window.getScrollableElementXpaths() did not return a list.",
                        Some("a11y"),
                        Some(json!({ "result": other })),
                    );
                }
            }
            Err(err) => {
                if let Some(logger) = logger {
                    logger.debug(
                        format!("Error calling window.getScrollableElementXpaths: {err}"),
                        Some("a11y"),
                        None,
                    );
                }
            }
        },
        Err(err) => {
            if let Some(logger) = logger {
                logger.debug(
                    format!(
                        "Error ensuring DOM injection before querying scrollable elements: {err}"
                    ),
                    Some("a11y"),
                    None,
                );
            }
        }
    }

    let mut scrollable_backend_ids = HashSet::new();

    if xpaths.is_empty() {
        return scrollable_backend_ids;
    }

    let session = match page.new_cdp_session().await {
        Ok(session) => session,
        Err(err) => {
            if let Some(logger) = logger {
                logger.debug(
                    format!("Error creating CDP session: {err}"),
                    Some("a11y"),
                    None,
                );
            }
            return scrollable_backend_ids;
        }
    };

    for xpath in xpaths {
        if xpath.is_empty() {
            continue;
        }

        let Ok(xpath_json) = serde_json::to_string(&xpath) else {
            continue;
        };

        let expression = format!(
            "(function() {{ try {{ const res = document.evaluate({xpath}, document, null, XPathResult.FIRST_ORDERED_NODE_TYPE, null); return res.singleNodeValue; }} catch (e) {{ console.error('Error evaluating XPath:', {xpath}, e); return null; }} }})()",
            xpath = xpath_json,
        );

        let eval_result = match session
            .send(
                "Runtime.evaluate",
                Some(json!({
                    "expression": expression,
                    "returnByValue": false,
                    "awaitPromise": false,
                })),
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                if let Some(logger) = logger {
                    logger.debug(
                        format!("Error evaluating xpath {xpath}: {err}"),
                        Some("a11y"),
                        None,
                    );
                }
                continue;
            }
        };

        let object_id = eval_result
            .get("result")
            .and_then(|result| result.get("objectId"))
            .and_then(Value::as_str)
            .map(|value| value.to_string());

        let Some(object_id) = object_id else {
            continue;
        };

        let node_info = match session
            .send("DOM.describeNode", Some(json!({ "objectId": object_id })))
            .await
        {
            Ok(value) => value,
            Err(err) => {
                if let Some(logger) = logger {
                    logger.debug(
                        format!("Error describing node for xpath {xpath}: {err}"),
                        Some("a11y"),
                        None,
                    );
                }
                continue;
            }
        };

        if let Some(backend_id) = node_info
            .get("node")
            .and_then(|node| node.get("backendNodeId"))
            .and_then(Value::as_i64)
        {
            scrollable_backend_ids.insert(backend_id);
        }
    }

    if let Err(err) = session.detach().await {
        if let Some(logger) = logger {
            logger.debug(
                format!("Error detaching CDP session: {err}"),
                Some("a11y"),
                None,
            );
        }
    }

    scrollable_backend_ids
}

pub async fn get_xpath_by_resolved_object_id(
    session: &dyn AccessibilitySession,
    resolved_object_id: &str,
) -> String {
    let declaration = format!(
        "function() {{ {} return getNodePath(this); }}",
        GET_NODE_PATH_FUNCTION
    );

    match session
        .send(
            "Runtime.callFunctionOn",
            Some(json!({
                "objectId": resolved_object_id,
                "functionDeclaration": declaration,
                "returnByValue": true,
            })),
        )
        .await
    {
        Ok(result) => result
            .get("result")
            .and_then(|value| value.get("value"))
            .and_then(Value::as_str)
            .map(|value| value.to_string())
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    fn ax_value(value: &str) -> AxValue {
        AxValue {
            value_type: "string".to_string(),
            value: Some(Value::String(value.to_string())),
        }
    }

    #[tokio::test]
    async fn build_tree_collapses_structural_nodes() {
        let nodes = vec![
            AxNode {
                node_id: "1".to_string(),
                role: Some(ax_value("RootWebArea")),
                name: Some(ax_value("Root")),
                description: None,
                value: None,
                backend_dom_node_id: Some(1),
                parent_id: None,
                child_ids: Some(vec!["2".to_string()]),
                properties: Some(vec![crate::types::a11y::AxProperty {
                    name: "url".to_string(),
                    value: Some(json!({ "value": "https://example.com" })),
                }]),
            },
            AxNode {
                node_id: "2".to_string(),
                role: Some(ax_value("generic")),
                name: None,
                description: None,
                value: None,
                backend_dom_node_id: Some(2),
                parent_id: Some("1".to_string()),
                child_ids: Some(vec!["3".to_string()]),
                properties: None,
            },
            AxNode {
                node_id: "3".to_string(),
                role: Some(ax_value("button")),
                name: Some(ax_value("Submit")),
                description: None,
                value: None,
                backend_dom_node_id: Some(3),
                parent_id: Some("2".to_string()),
                child_ids: None,
                properties: None,
            },
        ];

        let result = build_hierarchical_tree(nodes, Option::<&MockPage>::None, None)
            .await
            .expect("tree");

        assert_eq!(result.tree.len(), 1);
        let root = &result.tree[0];
        assert_eq!(root.node_id.as_deref(), Some("1"));
        assert_eq!(root.children.as_ref().unwrap().len(), 1);
        let child = &root.children.as_ref().unwrap()[0];
        assert_eq!(child.node_id.as_deref(), Some("3"));
        assert_eq!(child.role.as_deref(), Some("button"));
        assert_eq!(child.name.as_deref(), Some("Submit"));
        assert_eq!(
            result.id_to_url.get("1"),
            Some(&"https://example.com".to_string())
        );
        assert!(result.iframes.is_empty());
        assert!(result.simplified.contains("[3] button: Submit"));
    }

    struct MockPage {
        evaluate_result: Value,
        ensure_calls: AtomicUsize,
        session_responses: Arc<Mutex<VecDeque<Result<Value, AccessibilityError>>>>,
        detached: Arc<AtomicBool>,
    }

    impl MockPage {
        fn new(evaluate_result: Value, responses: Vec<Result<Value, AccessibilityError>>) -> Self {
            Self {
                evaluate_result,
                ensure_calls: AtomicUsize::new(0),
                session_responses: Arc::new(Mutex::new(VecDeque::from(responses))),
                detached: Arc::new(AtomicBool::new(false)),
            }
        }

        fn ensure_injection_calls(&self) -> usize {
            self.ensure_calls.load(Ordering::SeqCst)
        }

        fn detached(&self) -> bool {
            self.detached.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl AccessibilityPage for MockPage {
        async fn ensure_injection(&self) -> Result<(), AccessibilityError> {
            self.ensure_calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn evaluate(&self, _: &str) -> Result<Value, AccessibilityError> {
            Ok(self.evaluate_result.clone())
        }

        async fn send_cdp(
            &self,
            method: &str,
            _: Option<Value>,
        ) -> Result<Value, AccessibilityError> {
            if method == "Accessibility.getFullAXTree" {
                Ok(json!({ "nodes": [] }))
            } else {
                Ok(Value::Null)
            }
        }

        async fn disable_cdp_domain(&self, _: &str) -> Result<(), AccessibilityError> {
            Ok(())
        }

        async fn new_cdp_session(
            &self,
        ) -> Result<Box<dyn AccessibilitySession + Send>, AccessibilityError> {
            Ok(Box::new(MockSession {
                responses: Arc::clone(&self.session_responses),
                detached: Arc::clone(&self.detached),
            }))
        }
    }

    struct MockSession {
        responses: Arc<Mutex<VecDeque<Result<Value, AccessibilityError>>>>,
        detached: Arc<AtomicBool>,
    }

    #[async_trait]
    impl AccessibilitySession for MockSession {
        async fn send(&self, _: &str, _: Option<Value>) -> Result<Value, AccessibilityError> {
            let mut guard = self.responses.lock().unwrap();
            guard
                .pop_front()
                .unwrap_or_else(|| Err(AccessibilityError::Unexpected("no response".into())))
        }

        async fn detach(&self) -> Result<(), AccessibilityError> {
            self.detached.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn find_scrollable_element_ids_extracts_backend_ids() {
        let responses = vec![
            Ok(json!({
                "result": {
                    "objectId": "object-1"
                }
            })),
            Ok(json!({
                "node": {
                    "backendNodeId": 42
                }
            })),
        ];

        let page = MockPage::new(json!(["//div"]), responses);
        let logger = StagehandLogger::new(crate::config::Verbosity::Detailed);

        let ids = find_scrollable_element_ids(&page, Some(&logger)).await;
        assert_eq!(ids.len(), 1);
        assert!(ids.contains(&42));
        assert_eq!(page.ensure_injection_calls(), 1);
        assert!(page.detached());
    }
}
