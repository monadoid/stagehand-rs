use std::{any::TypeId, sync::Arc};

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::{
    accessibility::GetFullAxTreeParams,
    dom::{BackendNodeId, DescribeNodeParams, ResolveNodeParams},
    network, page as page_domain,
};
use chromiumoxide::cdp::js_protocol::runtime::{CallFunctionOnParams, EvaluateParams};
use chromiumoxide::page::Page as ChromiumPage;
use serde_json::{self, Value as JsonValue};
use tokio::time::{Duration, sleep};

use crate::a11y::{AccessibilityError, AccessibilityPage, AccessibilitySession};
use crate::browser::BrowserRuntime;
use crate::client::{StagehandClient, StagehandClientError};
use crate::metrics::StagehandFunctionName;
use crate::runtime::ChromiumoxideRuntime;
use crate::types::page::{
    ActOptions, ActResult, ExtractOptions, ExtractResult, ObserveOptions, ObserveResult,
};

mod local;

/// High-level wrapper around a Stagehand-managed page.
pub struct StagehandPage<'client, R: BrowserRuntime> {
    client: &'client StagehandClient<R>,
    page_id: String,
}

impl<'client, R> StagehandPage<'client, R>
where
    R: BrowserRuntime + 'static,
{
    pub fn new(client: &'client StagehandClient<R>, page_id: impl Into<String>) -> Self {
        Self {
            client,
            page_id: page_id.into(),
        }
    }

    pub(crate) fn client(&self) -> &StagehandClient<R> {
        self.client
    }

    fn as_chromiumoxide(&self) -> Option<&StagehandPage<'client, Arc<ChromiumoxideRuntime>>> {
        if TypeId::of::<R>() == TypeId::of::<Arc<ChromiumoxideRuntime>>() {
            let ptr = self as *const StagehandPage<'client, R>
                as *const StagehandPage<'client, Arc<ChromiumoxideRuntime>>;
            // SAFETY: we confirm the concrete type via TypeId comparison above.
            Some(unsafe { &*ptr })
        } else {
            None
        }
    }

    pub fn id(&self) -> &str {
        &self.page_id
    }

    pub async fn ensure_injection(&self) -> Result<(), StagehandClientError> {
        self.client
            .ensure_page_ready(self.page_id.clone(), None)
            .await
    }

    async fn frame_id(&self) -> Result<Option<String>, StagehandClientError> {
        let page_id = self.page_id.clone();
        self.client
            .with_context(|ctx| {
                let page_id = page_id.clone();
                Box::pin(async move { Ok(ctx.frame_id_for(&page_id)) })
            })
            .await
    }

    pub async fn act(&self, options: ActOptions) -> Result<ActResult, StagehandClientError> {
        if !self.client.use_api() {
            if let Some(local_page) = self.as_chromiumoxide() {
                return local::act_local(local_page, options).await;
            }
            return Err(StagehandClientError::Unsupported(
                "local act handler unsupported for this runtime",
            ));
        }

        self.ensure_injection().await?;
        let frame_id = self.frame_id().await?;

        let mut payload = serde_json::to_value(&options)?;
        if let Some(frame_id) = frame_id {
            if let Some(map) = payload.as_object_mut() {
                map.insert("frameId".to_string(), JsonValue::String(frame_id));
            }
        }

        let response = self.client.execute_api("act", payload).await?;
        self.record_metrics(&response, StagehandFunctionName::Act);

        let result_value = resolve_result_value(&response);
        let result: ActResult = serde_json::from_value(result_value)?;
        Ok(result)
    }

    pub async fn observe(
        &self,
        options: ObserveOptions,
    ) -> Result<Vec<ObserveResult>, StagehandClientError> {
        if !self.client.use_api() {
            if let Some(local_page) = self.as_chromiumoxide() {
                return local::observe_local(local_page, options, false).await;
            }
            return Err(StagehandClientError::Unsupported(
                "local observe handler unsupported for this runtime",
            ));
        }

        self.ensure_injection().await?;
        let frame_id = self.frame_id().await?;

        let mut payload = serde_json::to_value(&options)?;
        if let Some(frame_id) = frame_id {
            if let Some(map) = payload.as_object_mut() {
                map.insert("frameId".to_string(), JsonValue::String(frame_id));
            }
        }

        let response = self.client.execute_api("observe", payload).await?;
        self.record_metrics(&response, StagehandFunctionName::Observe);

        let value = resolve_result_value(&response);
        if value.is_array() {
            let items: Vec<ObserveResult> = serde_json::from_value(value)?;
            Ok(items)
        } else if value.is_object() {
            let item: ObserveResult = serde_json::from_value(value)?;
            Ok(vec![item])
        } else {
            Ok(Vec::new())
        }
    }

    pub async fn extract(
        &self,
        options: ExtractOptions,
    ) -> Result<ExtractResult, StagehandClientError> {
        if !self.client.use_api() {
            if let Some(local_page) = self.as_chromiumoxide() {
                return local::extract_local(local_page, options).await;
            }
            return Err(StagehandClientError::Unsupported(
                "local extract handler unsupported for this runtime",
            ));
        }

        self.ensure_injection().await?;
        let frame_id = self.frame_id().await?;

        let mut payload = serde_json::to_value(&options)?;
        if let Some(frame_id) = frame_id {
            if let Some(map) = payload.as_object_mut() {
                map.insert("frameId".to_string(), JsonValue::String(frame_id));
            }
        }

        let response = self.client.execute_api("extract", payload).await?;
        self.record_metrics(&response, StagehandFunctionName::Extract);

        let value = resolve_result_value(&response);
        let result: ExtractResult = serde_json::from_value(value)?;
        Ok(result)
    }

    fn record_metrics(&self, value: &JsonValue, function: StagehandFunctionName) {
        if let Some(meta) = find_metadata(value) {
            if let (Some(prompt), Some(completion), Some(inference)) = (
                meta.get("promptTokens").and_then(JsonValue::as_u64),
                meta.get("completionTokens").and_then(JsonValue::as_u64),
                meta.get("inferenceTimeMs").and_then(JsonValue::as_u64),
            ) {
                self.client
                    .update_metrics(function, prompt, completion, inference);
            }
        }
    }
}

fn resolve_result_value(value: &JsonValue) -> JsonValue {
    let preferred_keys = ["result", "data", "elements"];
    for key in preferred_keys {
        if let Some(inner) = value.get(key) {
            return inner.clone();
        }
    }
    value.clone()
}

fn find_metadata(value: &JsonValue) -> Option<&JsonValue> {
    if let Some(meta) = value.get("metadata") {
        return Some(meta);
    }
    value.get("result").and_then(|inner| inner.get("metadata"))
}

fn cdp_error(err: impl std::fmt::Display) -> StagehandClientError {
    StagehandClientError::Cdp(err.to_string())
}

impl<'client> StagehandPage<'client, Arc<ChromiumoxideRuntime>> {
    async fn chromium_page(&self) -> Result<ChromiumPage, StagehandClientError> {
        self.client
            .browser()
            .runtime()
            .page(&self.page_id)
            .await
            .map_err(StagehandClientError::Browser)?
            .ok_or_else(|| StagehandClientError::Unsupported("page handle unavailable"))
    }

    pub async fn evaluate_expression(
        &self,
        expression: &str,
    ) -> Result<JsonValue, StagehandClientError> {
        let page = self.chromium_page().await?;
        let result = page.evaluate(expression).await.map_err(cdp_error)?;
        Ok(result.value().cloned().unwrap_or_else(|| JsonValue::Null))
    }

    pub async fn send_cdp(
        &self,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, StagehandClientError> {
        let page = self.chromium_page().await?;
        execute_chromium_cdp(&page, method, params).await
    }

    pub async fn enable_cdp_domain(&self, domain: &str) -> Result<(), StagehandClientError> {
        let page = self.chromium_page().await?;
        match domain {
            "Network" => {
                page.execute(network::EnableParams::default())
                    .await
                    .map_err(cdp_error)?;
            }
            "Page" => {
                page.execute(page_domain::EnableParams::default())
                    .await
                    .map_err(cdp_error)?;
            }
            _ => return Err(StagehandClientError::Unsupported("unsupported CDP domain")),
        }
        Ok(())
    }

    pub async fn disable_cdp_domain(&self, domain: &str) -> Result<(), StagehandClientError> {
        let page = self.chromium_page().await?;
        match domain {
            "Network" => {
                page.execute(network::DisableParams::default())
                    .await
                    .map_err(cdp_error)?;
            }
            "Page" => {
                page.execute(page_domain::DisableParams::default())
                    .await
                    .map_err(cdp_error)?;
            }
            _ => return Err(StagehandClientError::Unsupported("unsupported CDP domain")),
        }
        Ok(())
    }

    pub async fn detach_cdp_client(&self) -> Result<(), StagehandClientError> {
        // chromiumoxide manages CDP sessions; nothing to detach.
        Ok(())
    }

    pub async fn wait_for_settled_dom(
        &self,
        timeout_ms: Option<u64>,
    ) -> Result<(), StagehandClientError> {
        let duration = Duration::from_millis(timeout_ms.unwrap_or(1_000));
        sleep(duration).await;
        Ok(())
    }
}

fn map_client_error(err: StagehandClientError) -> AccessibilityError {
    match err {
        StagehandClientError::Cdp(msg) => AccessibilityError::Cdp(msg),
        StagehandClientError::Json(json_err) => AccessibilityError::Serialization(json_err),
        other => AccessibilityError::Unexpected(other.to_string()),
    }
}

struct ChromiumAccessibilitySession {
    page: ChromiumPage,
}

#[async_trait]
impl AccessibilitySession for ChromiumAccessibilitySession {
    async fn send(
        &self,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, AccessibilityError> {
        execute_chromium_cdp(&self.page, method, params)
            .await
            .map_err(map_client_error)
    }

    async fn detach(&self) -> Result<(), AccessibilityError> {
        Ok(())
    }
}

#[async_trait]
impl AccessibilityPage for StagehandPage<'_, Arc<ChromiumoxideRuntime>> {
    async fn ensure_injection(&self) -> Result<(), AccessibilityError> {
        self.ensure_injection().await.map_err(map_client_error)
    }

    async fn evaluate(&self, expression: &str) -> Result<JsonValue, AccessibilityError> {
        self.evaluate_expression(expression)
            .await
            .map_err(map_client_error)
    }

    async fn send_cdp(
        &self,
        method: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, AccessibilityError> {
        self.send_cdp(method, params)
            .await
            .map_err(map_client_error)
    }

    async fn disable_cdp_domain(&self, domain: &str) -> Result<(), AccessibilityError> {
        self.disable_cdp_domain(domain)
            .await
            .map_err(map_client_error)
    }

    async fn new_cdp_session(
        &self,
    ) -> Result<Box<dyn AccessibilitySession + Send>, AccessibilityError> {
        let page = self.chromium_page().await.map_err(map_client_error)?;
        Ok(Box::new(ChromiumAccessibilitySession { page }))
    }
}

async fn execute_chromium_cdp(
    page: &ChromiumPage,
    method: &str,
    params: Option<JsonValue>,
) -> Result<JsonValue, StagehandClientError> {
    match method {
        "DOM.resolveNode" => resolve_node_on_page(page, params).await,
        "Accessibility.getFullAXTree" => {
            let command = if let Some(value) = params {
                serde_json::from_value(value)?
            } else {
                GetFullAxTreeParams::builder().build()
            };
            let response = page.execute(command).await.map_err(cdp_error)?;
            Ok(serde_json::to_value(response.result)?)
        }
        "Runtime.callFunctionOn" => {
            let value = params.ok_or_else(|| {
                StagehandClientError::Unsupported("Runtime.callFunctionOn requires params")
            })?;
            let command: CallFunctionOnParams = serde_json::from_value(value)?;
            let response = page.execute(command).await.map_err(cdp_error)?;
            Ok(serde_json::to_value(response.result)?)
        }
        "Runtime.evaluate" => {
            let value = params.ok_or_else(|| {
                StagehandClientError::Unsupported("Runtime.evaluate requires params")
            })?;
            let command: EvaluateParams = serde_json::from_value(value)?;
            let response = page.execute(command).await.map_err(cdp_error)?;
            Ok(serde_json::to_value(response.result)?)
        }
        "DOM.describeNode" => {
            let value = params.ok_or_else(|| {
                StagehandClientError::Unsupported("DOM.describeNode requires params")
            })?;
            let command: DescribeNodeParams = serde_json::from_value(value)?;
            let response = page.execute(command).await.map_err(cdp_error)?;
            Ok(serde_json::to_value(response.result)?)
        }
        _ => Err(StagehandClientError::Unsupported("unsupported CDP method")),
    }
}

async fn resolve_node_on_page(
    page: &ChromiumPage,
    params: Option<JsonValue>,
) -> Result<JsonValue, StagehandClientError> {
    let params = params.ok_or_else(|| {
        StagehandClientError::Unsupported("backendNodeId required for DOM.resolveNode")
    })?;
    let backend_node_id = params
        .get("backendNodeId")
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| StagehandClientError::Unsupported("backendNodeId required"))?;
    let command = ResolveNodeParams::builder()
        .backend_node_id(BackendNodeId::new(backend_node_id))
        .build();
    let response = page.execute(command).await.map_err(cdp_error)?;
    Ok(serde_json::to_value(response.result)?)
}
