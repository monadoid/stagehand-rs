use std::{
    any::TypeId,
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::Arc,
    time::Instant,
};

use async_trait::async_trait;
use chromiumoxide::cdp::IntoEventKind;
use chromiumoxide::cdp::browser_protocol::{
    accessibility::{
        DisableParams as AccessibilityDisableParams, EnableParams as AccessibilityEnableParams,
    },
    dom::{BackendNodeId, DescribeNodeParams, EnableParams as DomEnableParams, ResolveNodeParams},
    network::{
        self, EventLoadingFailed, EventLoadingFinished, EventRequestServedFromCache,
        EventRequestWillBeSent, EventResponseReceived, ResourceType,
    },
    page as page_domain,
    page::EventFrameStoppedLoading,
};
use chromiumoxide::cdp::js_protocol::runtime::{CallFunctionOnParams, EvaluateParams};
use chromiumoxide::listeners::EventStream;
use chromiumoxide::page::Page as ChromiumPage;
use futures_util::StreamExt;
use serde_json::{self, Value as JsonValue, json};
use tokio::{
    sync::mpsc,
    task::JoinHandle,
    time::{self, Duration, MissedTickBehavior, Sleep},
};

use crate::a11y::{
    AccessibilityError, AccessibilityPage, AccessibilitySession, GetFullAxTreeCommand,
};
use crate::browser::BrowserRuntime;
use crate::client::{StagehandClient, StagehandClientError};
use crate::logging::StagehandLogger;
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

    pub async fn goto(&self, url: &str) -> Result<(), StagehandClientError> {
        if !self.client.use_api() {
            if let Some(local_page) = self.as_chromiumoxide() {
                return local_page.goto_local(url).await;
            }
            return Err(StagehandClientError::Unsupported(
                "local navigation unsupported for this runtime",
            ));
        }

        Err(StagehandClientError::Unsupported(
            "remote navigation not yet implemented",
        ))
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

    pub async fn goto_local(&self, url: &str) -> Result<(), StagehandClientError> {
        let page = self.chromium_page().await?;
        page.goto(url).await.map_err(cdp_error)?;
        Ok(())
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
            "DOM" => {
                page.execute(DomEnableParams::default())
                    .await
                    .map_err(cdp_error)?;
            }
            "Accessibility" => {
                page.execute(AccessibilityEnableParams::default())
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
            "DOM" => {
                page.execute(chromiumoxide::cdp::browser_protocol::dom::DisableParams::default())
                    .await
                    .map_err(cdp_error)?;
            }
            "Accessibility" => {
                page.execute(AccessibilityDisableParams::default())
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
        let default_timeout = self.client.config().dom_settle_timeout_ms.unwrap_or(30_000);
        let timeout_ms = timeout_ms.unwrap_or(default_timeout);
        let quiet_window = Duration::from_millis(500);
        let stall_threshold = Duration::from_secs(2);
        let mut stall_tick = time::interval(Duration::from_millis(500));
        stall_tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let logger = self.client().logger();

        // Ensure required domains are enabled for network and frame tracking.
        for domain in ["Network", "Page", "DOM"] {
            if let Err(err) = self.enable_cdp_domain(domain).await {
                logger.debug(
                    format!("Failed to enable {domain} domain before DOM settle wait: {err}"),
                    Some("dom-settle"),
                    None,
                );
            }
        }

        let page = self.chromium_page().await?;

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut listener_handles: Vec<JoinHandle<()>> = Vec::new();

        listener_handles.push(spawn_dom_event_listener(
            page.event_listener::<EventRequestWillBeSent>()
                .await
                .map_err(cdp_error)?,
            tx.clone(),
            DomSettledEvent::RequestWillBeSent,
        ));
        listener_handles.push(spawn_dom_event_listener(
            page.event_listener::<EventLoadingFinished>()
                .await
                .map_err(cdp_error)?,
            tx.clone(),
            DomSettledEvent::LoadingFinished,
        ));
        listener_handles.push(spawn_dom_event_listener(
            page.event_listener::<EventLoadingFailed>()
                .await
                .map_err(cdp_error)?,
            tx.clone(),
            DomSettledEvent::LoadingFailed,
        ));
        listener_handles.push(spawn_dom_event_listener(
            page.event_listener::<EventRequestServedFromCache>()
                .await
                .map_err(cdp_error)?,
            tx.clone(),
            DomSettledEvent::RequestServedFromCache,
        ));
        listener_handles.push(spawn_dom_event_listener(
            page.event_listener::<EventResponseReceived>()
                .await
                .map_err(cdp_error)?,
            tx.clone(),
            DomSettledEvent::ResponseReceived,
        ));
        listener_handles.push(spawn_dom_event_listener(
            page.event_listener::<EventFrameStoppedLoading>()
                .await
                .map_err(cdp_error)?,
            tx.clone(),
            DomSettledEvent::FrameStopped,
        ));
        drop(tx);

        let mut inflight: HashSet<String> = HashSet::new();
        let mut meta: HashMap<String, RequestMeta> = HashMap::new();
        let mut doc_by_frame: HashMap<String, String> = HashMap::new();

        let mut quiet_timer: Option<Pin<Box<time::Sleep>>> = None;
        start_quiet_timer(&mut quiet_timer, quiet_window);
        let mut timeout_timer = Box::pin(time::sleep(Duration::from_millis(timeout_ms)));

        loop {
            tokio::select! {
                maybe_event = rx.recv() => {
                    match maybe_event {
                        Some(event) => {
                            handle_dom_event(
                                event,
                                &mut inflight,
                                &mut meta,
                                &mut doc_by_frame,
                                &mut quiet_timer,
                                quiet_window,
                            );
                        }
                        None => break,
                    }
                }
                _ = async {
                    if let Some(timer) = quiet_timer.as_mut() {
                        timer.as_mut().await;
                    }
                }, if quiet_timer.is_some() => {
                    break;
                }
                _ = stall_tick.tick() => {
                    sweep_stalled_requests(
                        &mut inflight,
                        &mut meta,
                        &mut doc_by_frame,
                        stall_threshold,
                        quiet_window,
                        &mut quiet_timer,
                        logger.as_ref(),
                    );
                }
                _ = &mut timeout_timer => {
                    if !inflight.is_empty() {
                        logger.debug(
                            format!(
                                "DOM settle timeout reached with {} inflight requests",
                                inflight.len()
                            ),
                            Some("dom-settle"),
                            None,
                        );
                    }
                    break;
                }
            }
        }

        for handle in listener_handles {
            handle.abort();
        }

        Ok(())
    }
}

#[derive(Debug, Clone)]
struct RequestMeta {
    url: String,
    started_at: Instant,
}

enum DomSettledEvent {
    RequestWillBeSent(EventRequestWillBeSent),
    LoadingFinished(EventLoadingFinished),
    LoadingFailed(EventLoadingFailed),
    RequestServedFromCache(EventRequestServedFromCache),
    ResponseReceived(EventResponseReceived),
    FrameStopped(EventFrameStoppedLoading),
}

fn spawn_dom_event_listener<T, F>(
    mut stream: EventStream<T>,
    tx: mpsc::UnboundedSender<DomSettledEvent>,
    map: F,
) -> JoinHandle<()>
where
    T: IntoEventKind + Clone + Unpin + Send + 'static,
    F: Fn(T) -> DomSettledEvent + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let owned = (*event).clone();
            if tx.send(map(owned)).is_err() {
                break;
            }
        }
    })
}

fn handle_dom_event(
    event: DomSettledEvent,
    inflight: &mut HashSet<String>,
    meta: &mut HashMap<String, RequestMeta>,
    doc_by_frame: &mut HashMap<String, String>,
    quiet_timer: &mut Option<Pin<Box<time::Sleep>>>,
    quiet_window: Duration,
) {
    match event {
        DomSettledEvent::RequestWillBeSent(ev) => {
            if matches!(
                ev.r#type.as_ref(),
                Some(ResourceType::WebSocket | ResourceType::EventSource)
            ) {
                return;
            }

            let request_id = ev.request_id.as_ref().to_string();
            inflight.insert(request_id.clone());
            meta.insert(
                request_id.clone(),
                RequestMeta {
                    url: ev.request.url.clone(),
                    started_at: Instant::now(),
                },
            );

            if matches!(ev.r#type.as_ref(), Some(ResourceType::Document)) {
                if let Some(frame_id) = ev.frame_id.as_ref() {
                    doc_by_frame.insert(frame_id.as_ref().to_string(), request_id.clone());
                }
            }

            clear_quiet_timer(quiet_timer);
        }
        DomSettledEvent::LoadingFinished(ev) => {
            finish_request(
                ev.request_id.as_ref(),
                inflight,
                meta,
                doc_by_frame,
                quiet_timer,
                quiet_window,
            );
        }
        DomSettledEvent::LoadingFailed(ev) => {
            finish_request(
                ev.request_id.as_ref(),
                inflight,
                meta,
                doc_by_frame,
                quiet_timer,
                quiet_window,
            );
        }
        DomSettledEvent::RequestServedFromCache(ev) => {
            finish_request(
                ev.request_id.as_ref(),
                inflight,
                meta,
                doc_by_frame,
                quiet_timer,
                quiet_window,
            );
        }
        DomSettledEvent::ResponseReceived(ev) => {
            if ev.response.url.starts_with("data:") {
                finish_request(
                    ev.request_id.as_ref(),
                    inflight,
                    meta,
                    doc_by_frame,
                    quiet_timer,
                    quiet_window,
                );
            }
        }
        DomSettledEvent::FrameStopped(ev) => {
            let frame_id = ev.frame_id.as_ref().to_string();
            if let Some(request_id) = doc_by_frame.remove(&frame_id) {
                finish_request(
                    &request_id,
                    inflight,
                    meta,
                    doc_by_frame,
                    quiet_timer,
                    quiet_window,
                );
            }
        }
    }

    if inflight.is_empty() {
        start_quiet_timer(quiet_timer, quiet_window);
    }
}

fn finish_request(
    request_id: &str,
    inflight: &mut HashSet<String>,
    meta: &mut HashMap<String, RequestMeta>,
    doc_by_frame: &mut HashMap<String, String>,
    quiet_timer: &mut Option<Pin<Box<Sleep>>>,
    quiet_window: Duration,
) {
    let was_inflight = inflight.remove(request_id);
    meta.remove(request_id);
    doc_by_frame.retain(|_, rid| rid != request_id);

    if was_inflight {
        clear_quiet_timer(quiet_timer);
    }

    if inflight.is_empty() {
        start_quiet_timer(quiet_timer, quiet_window);
    }
}

fn start_quiet_timer(timer: &mut Option<Pin<Box<Sleep>>>, quiet_window: Duration) {
    if timer.is_none() {
        timer.replace(Box::pin(time::sleep(quiet_window)));
    }
}

fn clear_quiet_timer(timer: &mut Option<Pin<Box<Sleep>>>) {
    if timer.is_some() {
        timer.take();
    }
}

fn sweep_stalled_requests(
    inflight: &mut HashSet<String>,
    meta: &mut HashMap<String, RequestMeta>,
    doc_by_frame: &mut HashMap<String, String>,
    threshold: Duration,
    quiet_window: Duration,
    quiet_timer: &mut Option<Pin<Box<Sleep>>>,
    logger: &StagehandLogger,
) {
    let now = Instant::now();
    let stalled: Vec<(String, String)> = meta
        .iter()
        .filter_map(|(request_id, entry)| {
            if now.duration_since(entry.started_at) > threshold {
                Some((request_id.clone(), entry.url.clone()))
            } else {
                None
            }
        })
        .collect();

    for (request_id, url) in stalled {
        logger.debug(
            "forcing completion of stalled request",
            Some("dom-settle"),
            Some(json!({"url": url})),
        );
        finish_request(
            &request_id,
            inflight,
            meta,
            doc_by_frame,
            quiet_timer,
            quiet_window,
        );
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

    async fn enable_cdp_domain(&self, domain: &str) -> Result<(), AccessibilityError> {
        <StagehandPage<'_, Arc<ChromiumoxideRuntime>>>::enable_cdp_domain(self, domain)
            .await
            .map_err(map_client_error)
    }

    async fn disable_cdp_domain(&self, domain: &str) -> Result<(), AccessibilityError> {
        <StagehandPage<'_, Arc<ChromiumoxideRuntime>>>::disable_cdp_domain(self, domain)
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
            let command: GetFullAxTreeCommand = if let Some(value) = params {
                serde_json::from_value(value)?
            } else {
                GetFullAxTreeCommand::default()
            };
            match page.execute(command).await {
                Ok(response) => match serde_json::to_value(&response.result) {
                    Ok(value) => Ok(value),
                    Err(ser_err) => {
                        eprintln!(
                            "Accessibility.getFullAXTree serialization failed: {ser_err:?} (nodes: {})",
                            response.result.nodes.len()
                        );
                        if let Some(first) = response.result.nodes.first() {
                            eprintln!("First AX node (debug): {:?}", first);
                        }
                        Err(StagehandClientError::Json(ser_err))
                    }
                },
                Err(err) => {
                    eprintln!("Accessibility.getFullAXTree failed: {err:?}");
                    Err(cdp_error(err))
                }
            }
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
