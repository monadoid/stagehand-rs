//! High-level Stagehand client scaffolding.
//!
//! This module stitches together the configuration-driven browser planning
//! logic with the `StagehandContext` wrapper, providing a stub implementation
//! that we can grow towards the full Python feature set.

use std::collections::HashSet;
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::future::BoxFuture;
use futures_util::{StreamExt, TryStreamExt};
use reqwest::Client as HttpClient;
use reqwest::header::{CONNECTION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, broadcast, broadcast::error::TryRecvError};
use tokio_util::codec::{FramedRead, LinesCodec};
use tokio_util::io::StreamReader;

use crate::browser::{
    BrowserError, BrowserPlan, BrowserRuntime, BrowserRuntimeError, RuntimeTargetEvent,
    StagehandBrowser,
};
use crate::config::{Environment, LoggerCallback, StagehandConfig};
use crate::context::{StagehandAdapter, StagehandContext, StagehandContextError};
use crate::dom_scripts;
use crate::llm::{OpenAiChatProvider, StagehandLlmClient, StagehandLlmError};
use crate::logging::{LogCallback, LogConfig, StagehandLogger, sync_log_handler};
use crate::metrics::{StagehandFunctionName, StagehandMetrics};
use crate::runtime::ChromiumoxideRuntime;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    status: u16,
    body: String,
}

const MAX_SSE_LINE_LENGTH: usize = 1_048_576;

pub type BoxLineStream = Pin<
    Box<
        dyn futures_util::stream::Stream<Item = Result<String, StagehandClientError>>
            + Send
            + 'static,
    >,
>;

pub struct StreamingHttpResponse {
    pub stream: BoxLineStream,
}

#[async_trait]
pub trait StagehandHttp: Send + Sync {
    async fn post_json(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &JsonValue,
    ) -> Result<HttpResponse, StagehandClientError>;

    async fn post_streaming_lines(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &JsonValue,
    ) -> Result<StreamingHttpResponse, StagehandClientError>;

    async fn get_json(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<HttpResponse, StagehandClientError>;
}

struct ReqwestStagehandHttpClient {
    client: HttpClient,
}

impl Default for ReqwestStagehandHttpClient {
    fn default() -> Self {
        // Match Python's 180-second per-operation timeouts to avoid hanging
        // requests when the upstream API stalls.
        let timeout = Duration::from_secs(180);
        let client = HttpClient::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .pool_idle_timeout(timeout)
            .build()
            .unwrap_or_else(|err| {
                panic!("failed to construct Stagehand HTTP client with timeouts: {err}")
            });
        Self { client }
    }
}

#[async_trait]
impl StagehandHttp for ReqwestStagehandHttpClient {
    async fn post_json(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &JsonValue,
    ) -> Result<HttpResponse, StagehandClientError> {
        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(|err| StagehandClientError::Http(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| StagehandClientError::Http(err.to_string()))?;
        Ok(HttpResponse { status, body })
    }

    async fn post_streaming_lines(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &JsonValue,
    ) -> Result<StreamingHttpResponse, StagehandClientError> {
        let response = self
            .client
            .post(url)
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(|err| StagehandClientError::Http(err.to_string()))?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response
                .text()
                .await
                .map_err(|err| StagehandClientError::Http(err.to_string()))?;
            return Err(StagehandClientError::Api(format!(
                "request failed with status {}: {}",
                status, body
            )));
        }

        let byte_stream = response
            .bytes_stream()
            .map_err(|err| io::Error::new(io::ErrorKind::Other, err));
        let reader = StreamReader::new(byte_stream);
        let lines = FramedRead::new(reader, LinesCodec::new_with_max_length(MAX_SSE_LINE_LENGTH));
        let stream = lines.map(|result| match result {
            Ok(line) => Ok(line),
            Err(err) => Err(StagehandClientError::Http(err.to_string())),
        });

        Ok(StreamingHttpResponse {
            stream: Box::pin(stream),
        })
    }

    async fn get_json(
        &self,
        url: &str,
        headers: HeaderMap,
    ) -> Result<HttpResponse, StagehandClientError> {
        let response = self
            .client
            .get(url)
            .headers(headers)
            .send()
            .await
            .map_err(|err| StagehandClientError::Http(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| StagehandClientError::Http(err.to_string()))?;
        Ok(HttpResponse { status, body })
    }
}

/// Orchestrates browser planning/execution and context management.
pub struct StagehandClient<R: BrowserRuntime> {
    browser: StagehandBrowser<R>,
    adapter: Arc<dyn StagehandAdapter>,
    dom_script: String,
    context: AsyncMutex<Option<StagehandContext>>,
    session_lock: AsyncMutex<()>,
    logger: Arc<StagehandLogger>,
    metrics: Arc<Mutex<StagehandMetrics>>,
    http_client: Arc<dyn StagehandHttp>,
    api_state: AsyncMutex<ApiSessionState>,
    target_events: AsyncMutex<Option<broadcast::Receiver<RuntimeTargetEvent>>>,
    page_switch_lock: Arc<AsyncMutex<()>>,
    llm_factory: Arc<
        dyn Fn(
                &StagehandClient<R>,
            ) -> Result<StagehandLlmClient<OpenAiChatProvider>, StagehandLlmError>
            + Send
            + Sync,
    >,
}

#[derive(Default, Debug, Clone)]
struct ApiSessionState {
    session_id: Option<String>,
}

impl<R: BrowserRuntime> std::fmt::Debug for StagehandClient<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let initialized = self
            .context
            .try_lock()
            .map(|guard| guard.is_some())
            .unwrap_or(true);
        f.debug_struct("StagehandClient")
            .field("plan", &self.browser.plan())
            .field("context_initialized", &initialized)
            .field("logger_attached", &true)
            .finish()
    }
}

fn default_llm_factory<R: BrowserRuntime>() -> Arc<
    dyn Fn(&StagehandClient<R>) -> Result<StagehandLlmClient<OpenAiChatProvider>, StagehandLlmError>
        + Send
        + Sync,
> {
    Arc::new(|client: &StagehandClient<R>| {
        StagehandLlmClient::from_config(client.browser.config(), None)
    })
}

/// Errors surfaced by [`StagehandClient`].
#[derive(Debug, Error)]
pub enum StagehandClientError {
    #[error(transparent)]
    Browser(#[from] BrowserRuntimeError),
    #[error(transparent)]
    Context(#[from] StagehandContextError),
    #[error(transparent)]
    Llm(#[from] StagehandLlmError),
    #[error("http error: {0}")]
    Http(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("stagehand API error: {0}")]
    Api(String),
    #[error("operation unsupported: {0}")]
    Unsupported(&'static str),
    #[error("cdp error: {0}")]
    Cdp(String),
}

impl<R: BrowserRuntime + 'static> StagehandClient<R> {
    pub(crate) fn config(&self) -> &StagehandConfig {
        self.browser.config()
    }

    pub fn use_api(&self) -> bool {
        self.config().use_api
    }

    pub fn update_metrics(
        &self,
        function: StagehandFunctionName,
        prompt_tokens: u64,
        completion_tokens: u64,
        inference_time_ms: u64,
    ) {
        if let Ok(mut guard) = self.metrics.lock() {
            guard.record(
                function,
                prompt_tokens,
                completion_tokens,
                inference_time_ms,
            );
        }
    }

    /// Create a new client using the embedded Stagehand DOM script.
    pub fn new_with_default_dom_script(
        config: StagehandConfig,
        runtime: R,
        adapter: Arc<dyn StagehandAdapter>,
    ) -> Result<Self, BrowserError> {
        Self::new(
            config,
            runtime,
            adapter,
            dom_scripts::stagehand_dom_script(),
        )
    }

    pub fn new(
        config: StagehandConfig,
        runtime: R,
        adapter: Arc<dyn StagehandAdapter>,
        dom_script: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        let http_client: Arc<dyn StagehandHttp> = Arc::new(ReqwestStagehandHttpClient::default());
        Self::new_with_http_client(config, runtime, adapter, dom_script, http_client)
    }

    pub fn new_with_http_client(
        config: StagehandConfig,
        runtime: R,
        adapter: Arc<dyn StagehandAdapter>,
        dom_script: impl Into<String>,
        http_client: Arc<dyn StagehandHttp>,
    ) -> Result<Self, BrowserError> {
        let logger = Arc::new(build_logger(&config));
        let metrics = Arc::new(Mutex::new(StagehandMetrics::default()));
        let browser = StagehandBrowser::new(config, runtime)?;
        Ok(Self {
            browser,
            adapter,
            dom_script: dom_script.into(),
            context: AsyncMutex::new(None),
            session_lock: AsyncMutex::new(()),
            logger,
            metrics,
            http_client,
            api_state: AsyncMutex::new(ApiSessionState::default()),
            target_events: AsyncMutex::new(None),
            page_switch_lock: Arc::new(AsyncMutex::new(())),
            llm_factory: default_llm_factory(),
        })
    }

    pub fn browser(&self) -> &StagehandBrowser<R> {
        &self.browser
    }

    pub fn plan(&self) -> BrowserPlan {
        self.browser.plan()
    }

    pub fn session_lock(&self) -> &AsyncMutex<()> {
        &self.session_lock
    }

    pub fn page_switch_lock(&self) -> Arc<AsyncMutex<()>> {
        Arc::clone(&self.page_switch_lock)
    }

    pub fn log_debug(&self, message: &str, category: &'static str) {
        self.logger.debug(message, Some(category), None);
        self.adapter.log_debug(message, category);
    }

    pub fn log_error(&self, message: &str, category: &'static str) {
        self.logger.error(message, Some(category), None);
        self.adapter.log_error(message, category);
    }

    pub async fn shutdown(&self) -> Result<(), StagehandClientError> {
        {
            let mut guard = self.context.lock().await;
            *guard = None;
        }
        {
            let mut guard = self.target_events.lock().await;
            *guard = None;
        }
        {
            let mut guard = self.api_state.lock().await;
            guard.session_id = None;
        }

        self.browser
            .shutdown()
            .await
            .map_err(StagehandClientError::Browser)
    }

    pub fn llm_logger_callback(&self) -> LoggerCallback {
        let logger = self.logger();
        let callback: LoggerCallback = Arc::new(move |message: &str| {
            logger.debug(message, Some("llm"), None);
        });
        callback
    }

    pub fn logger(&self) -> Arc<StagehandLogger> {
        Arc::clone(&self.logger)
    }

    pub fn metrics(&self) -> Arc<Mutex<StagehandMetrics>> {
        Arc::clone(&self.metrics)
    }

    pub(crate) async fn runtime_main_frame_id_for(
        &self,
        page_id: &str,
    ) -> Result<Option<String>, StagehandClientError> {
        match self.browser.runtime().main_frame_id(page_id).await {
            Ok(frame) => Ok(frame),
            Err(BrowserRuntimeError::Unsupported(_)) => Ok(None),
            Err(BrowserRuntimeError::NotInitialized) => Ok(None),
            Err(err) => Err(StagehandClientError::Browser(err)),
        }
    }

    pub(crate) async fn ensure_frame_registered(
        &self,
        page_id: &str,
        candidate: Option<String>,
    ) -> Result<(), StagehandClientError> {
        let frame_opt = match candidate {
            Some(value) => Some(value),
            None => self.runtime_main_frame_id_for(page_id).await?,
        };

        if let Some(frame_id) = frame_opt {
            let page_id_owned = page_id.to_string();
            let mut guard = self.context.lock().await;
            if let Some(ctx) = guard.as_mut() {
                if let Some(page) = ctx.page(&page_id_owned) {
                    let needs_update = page
                        .frame_id()
                        .map(|existing| existing != frame_id)
                        .unwrap_or(true);
                    if needs_update {
                        if let Err(err) = ctx.update_frame_id(&page_id_owned, frame_id.clone()) {
                            return Err(StagehandClientError::Context(err));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub async fn fetch_replay_metrics(&self) -> Result<StagehandMetrics, StagehandClientError> {
        if !self.use_api() {
            return Ok(match self.metrics.lock() {
                Ok(guard) => guard.clone(),
                Err(_) => StagehandMetrics::default(),
            });
        }

        let config = self.config();
        let api_key = config
            .api_key
            .clone()
            .ok_or_else(|| StagehandClientError::Api("browserbase API key is required".into()))?;
        let project_id = config.project_id.clone().ok_or_else(|| {
            StagehandClientError::Api("browserbase project id is required".into())
        })?;

        let session_id = self.ensure_session().await?;
        let url = format!("{}/sessions/{}/replay", self.base_api_url(), session_id);

        let headers = self.base_headers(&api_key, &project_id)?;
        let response = self.http_client.get_json(&url, headers).await?;

        if response.status != 200 {
            return Err(StagehandClientError::Api(format!(
                "failed to fetch replay metrics: status {}: {}",
                response.status, response.body
            )));
        }

        let body: JsonValue = serde_json::from_str(&response.body)?;
        if !body
            .get("success")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false)
        {
            let message = body
                .get("error")
                .and_then(JsonValue::as_str)
                .unwrap_or("Unknown error");
            return Err(StagehandClientError::Api(message.to_string()));
        }

        let metrics = parse_replay_metrics(body.get("data").unwrap_or(&JsonValue::Null));
        Ok(metrics)
    }

    pub fn create_llm_client(
        &self,
    ) -> Result<StagehandLlmClient<OpenAiChatProvider>, StagehandLlmError> {
        let mut client = (self.llm_factory)(self)?;
        client.set_logger(Some(self.llm_logger_callback()));
        let metrics_store = self.metrics();
        client.set_metrics_store(Some(metrics_store));
        Ok(client)
    }

    pub fn with_llm_factory(
        mut self,
        factory: Arc<
            dyn Fn(
                    &StagehandClient<R>,
                )
                    -> Result<StagehandLlmClient<OpenAiChatProvider>, StagehandLlmError>
                + Send
                + Sync,
        >,
    ) -> Self {
        self.llm_factory = factory;
        self
    }

    /// Execute the browser plan if needed and ensure the StagehandContext exists.
    pub async fn ensure_initialized(&self) -> Result<(), StagehandClientError> {
        {
            let guard = self.context.lock().await;
            if guard.is_some() {
                return Ok(());
            }
        }

        self.browser.execute().await?;

        let existing_pages = self
            .browser
            .runtime()
            .list_pages()
            .await
            .map_err(StagehandClientError::Browser)?;

        let mut guard = self.context.lock().await;
        if guard.is_none() {
            let context = StagehandContext::new(self.adapter.clone(), self.dom_script.clone());
            *guard = Some(context);
        }
        let ctx = guard
            .as_mut()
            .expect("context must be initialized after ensure_initialized");

        for page_id in existing_pages {
            ctx.register_page(page_id.clone(), None);
            ctx.ensure_dom_script(&page_id).await?;
            if ctx.active_page().is_none() {
                if let Err(err) = ctx.set_active_page(&page_id) {
                    self.log_error(
                        &format!("Failed to set active page {page_id}: {err}"),
                        "context",
                    );
                }
            }
        }

        drop(guard);
        self.ensure_target_event_receiver().await?;

        Ok(())
    }

    /// Execute the provided closure with a mutable reference to the context.
    pub async fn with_context<F, T>(&self, f: F) -> Result<T, StagehandClientError>
    where
        F: for<'ctx> FnOnce(
            &'ctx mut StagehandContext,
        ) -> BoxFuture<'ctx, Result<T, StagehandContextError>>,
    {
        self.ensure_initialized().await?;
        let mut guard = self.context.lock().await;
        let ctx = guard
            .as_mut()
            .expect("context must be initialized after ensure_initialized");
        self.sync_context_pages_locked(ctx).await?;
        f(ctx).await.map_err(StagehandClientError::Context)
    }

    async fn sync_context_pages_locked(
        &self,
        ctx: &mut StagehandContext,
    ) -> Result<(), StagehandClientError> {
        self.ensure_target_event_receiver().await?;
        self.drain_target_events(ctx).await?;

        let runtime_pages: Vec<String> = self
            .browser
            .runtime()
            .list_pages()
            .await
            .map_err(StagehandClientError::Browser)?;
        let runtime_set: HashSet<String> = runtime_pages.iter().cloned().collect();

        let existing_pages: Vec<String> = ctx.page_ids().cloned().collect();

        for page_id in existing_pages {
            if !runtime_set.contains(&page_id) {
                ctx.remove_page(&page_id);
            }
        }

        let mut frame_updates = Vec::new();

        for page_id in &runtime_pages {
            if ctx.page(page_id).is_none() {
                ctx.register_page(page_id.clone(), None);
                ctx.ensure_dom_script(page_id).await?;
                if ctx.active_page_id().is_none() {
                    if let Err(err) = ctx.set_active_page(page_id) {
                        self.log_error(
                            &format!("Failed to set active page to {page_id}: {err}"),
                            "context",
                        );
                    }
                }
                frame_updates.push(page_id.clone());
            }
        }

        for page_id in frame_updates {
            match self.runtime_main_frame_id_for(&page_id).await {
                Ok(Some(frame_id)) => {
                    if let Some(page) = ctx.page(&page_id) {
                        let needs_update = page
                            .frame_id()
                            .map(|existing| existing != frame_id)
                            .unwrap_or(true);
                        if needs_update {
                            if let Err(err) = ctx.update_frame_id(&page_id, frame_id.clone()) {
                                self.log_debug(
                                    &format!(
                                        "Failed to update frame id mapping for {page_id}: {err}"
                                    ),
                                    "context",
                                );
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    self.log_debug(
                        &format!("Skipping frame registration for {page_id}: {err}"),
                        "context",
                    );
                }
            }
        }

        Ok(())
    }

    async fn ensure_target_event_receiver(&self) -> Result<(), StagehandClientError> {
        let mut guard = self.target_events.lock().await;
        if guard.is_none() {
            match self.browser.runtime().target_event_stream().await {
                Ok(receiver) => {
                    *guard = Some(receiver);
                }
                Err(BrowserRuntimeError::Unsupported(_)) => {}
                Err(BrowserRuntimeError::NotInitialized) => {}
                Err(err) => return Err(StagehandClientError::Browser(err)),
            }
        }
        Ok(())
    }

    async fn drain_target_events(
        &self,
        ctx: &mut StagehandContext,
    ) -> Result<(), StagehandClientError> {
        let mut guard = self.target_events.lock().await;
        let mut pending = Vec::new();
        if let Some(receiver) = guard.as_mut() {
            loop {
                match receiver.try_recv() {
                    Ok(event) => pending.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Lagged(_)) => continue,
                    Err(TryRecvError::Closed) => {
                        *guard = None;
                        break;
                    }
                }
            }
        }
        drop(guard);

        if pending.is_empty() {
            return Ok(());
        }

        let frame_updates = {
            let lock = self.page_switch_lock();
            let _guard = lock.lock().await;
            let mut frame_updates = Vec::new();

            for event in pending {
                match event {
                    RuntimeTargetEvent::Attached { target_id } => {
                        let page_id = target_id.clone();
                        ctx.register_page(page_id.clone(), None);
                        ctx.ensure_dom_script(&page_id).await?;
                        if let Err(err) = ctx.set_active_page(&page_id) {
                            self.log_error(
                                &format!("Failed to set active page to {page_id}: {err}"),
                                "context",
                            );
                        }
                        frame_updates.push(target_id);
                    }
                    RuntimeTargetEvent::Detached { target_id } => {
                        let was_removed = ctx.remove_page(&target_id);
                        if was_removed && ctx.active_page_id().is_none() {
                            let next_page = ctx.page_ids().next().cloned();
                            if let Some(first) = next_page {
                                if let Err(err) = ctx.set_active_page(&first) {
                                    self.log_error(
                                        &format!("Failed to set active page to {first}: {err}"),
                                        "context",
                                    );
                                }
                            }
                        }
                    }
                }
            }

            frame_updates
        };

        for page_id in frame_updates {
            match self.runtime_main_frame_id_for(&page_id).await {
                Ok(Some(frame_id)) => {
                    if let Some(page) = ctx.page(&page_id) {
                        let needs_update = page
                            .frame_id()
                            .map(|existing| existing != frame_id)
                            .unwrap_or(true);
                        if needs_update {
                            if let Err(err) = ctx.update_frame_id(&page_id, frame_id.clone()) {
                                self.log_debug(
                                    &format!(
                                        "Failed to update frame id mapping for {page_id}: {err}"
                                    ),
                                    "context",
                                );
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    self.log_debug(
                        &format!("Skipping frame registration for {page_id}: {err}"),
                        "context",
                    );
                }
            }
        }

        Ok(())
    }

    /// Register a page and ensure the Stagehand DOM script has been injected.
    pub async fn ensure_page_ready(
        &self,
        page_id: impl Into<String>,
        frame_id: Option<String>,
    ) -> Result<(), StagehandClientError> {
        let page_id = page_id.into();
        let frame_clone = frame_id.clone();
        let page_id_for_context = page_id.clone();
        self.with_context(move |ctx| {
            let page_id = page_id_for_context.clone();
            Box::pin(async move {
                ctx.register_page(page_id.clone(), frame_clone);
                ctx.ensure_dom_script(&page_id).await?;
                Ok(())
            })
        })
        .await?;

        self.ensure_frame_registered(&page_id, frame_id).await
    }

    pub async fn set_active_page(&self, page_id: &str) -> Result<(), StagehandClientError> {
        let page_id = page_id.to_string();
        self.with_context(|ctx| {
            let page_id = page_id.clone();
            Box::pin(async move {
                ctx.set_active_page(&page_id)?;
                Ok(())
            })
        })
        .await
    }

    pub async fn active_page_id(&self) -> Result<Option<String>, StagehandClientError> {
        self.with_context(|ctx| {
            let active = ctx.active_page_id().cloned();
            Box::pin(async move { Ok(active) })
        })
        .await
    }

    /// Open a new page using the underlying browser runtime and ensure it is registered.
    pub async fn open_page(&self, url: &str) -> Result<String, StagehandClientError> {
        self.ensure_initialized().await?;
        let lock = self.page_switch_lock();
        let _guard = lock.lock().await;
        let page_id = self.browser.runtime().new_page(url).await?;
        self.ensure_page_ready(page_id.clone(), None).await?;
        self.with_context(|ctx| {
            let page_id = page_id.clone();
            Box::pin(async move {
                ctx.set_active_page(&page_id)?;
                Ok(())
            })
        })
        .await?;
        Ok(page_id)
    }

    /// Remove a page from the context. Returns `true` if the page existed.
    pub async fn remove_page(&self, page_id: &str) -> Result<bool, StagehandClientError> {
        let page_id_owned = page_id.to_string();
        self.with_context(|ctx| {
            let page_id_owned = page_id_owned.clone();
            Box::pin(async move { Ok(ctx.remove_page(&page_id_owned)) })
        })
        .await
    }

    /// Retrieve a [`StagehandPage`] wrapper for the given page identifier.
    pub fn page(&self, page_id: impl Into<String>) -> crate::page::StagehandPage<'_, R> {
        crate::page::StagehandPage::new(self, page_id.into())
    }

    async fn ensure_session(&self) -> Result<String, StagehandClientError> {
        if !self.use_api() {
            return Err(StagehandClientError::Unsupported(
                "Stagehand API usage is disabled in this configuration",
            ));
        }

        if let Some(existing) = self.config().browserbase_session_id.clone() {
            let mut guard = self.api_state.lock().await;
            guard.session_id.get_or_insert(existing.clone());
            return Ok(existing);
        }

        {
            let guard = self.api_state.lock().await;
            if let Some(existing) = guard.session_id.clone() {
                return Ok(existing);
            }
        }

        let _lock = self.session_lock.lock().await;
        {
            let guard = self.api_state.lock().await;
            if let Some(existing) = guard.session_id.clone() {
                return Ok(existing);
            }
        }
        let session_id = self.create_session().await?;
        let mut guard = self.api_state.lock().await;
        guard.session_id = Some(session_id.clone());
        Ok(session_id)
    }

    async fn create_session(&self) -> Result<String, StagehandClientError> {
        let config = self.config();
        let api_key = config
            .api_key
            .clone()
            .ok_or_else(|| StagehandClientError::Api("browserbase API key is required".into()))?;
        let project_id = config.project_id.clone().ok_or_else(|| {
            StagehandClientError::Api("browserbase project id is required".into())
        })?;
        let model_api_key = config
            .model_api_key
            .clone()
            .ok_or_else(|| StagehandClientError::Api("model API key is required".into()))?;

        let verbose = match config.verbose {
            crate::config::Verbosity::Minimal => 0u8,
            crate::config::Verbosity::Medium => 1u8,
            crate::config::Verbosity::Detailed => 2u8,
        };

        let create_params = prepare_session_create_params(
            config
                .browserbase_session_create_params()
                .map(JsonValue::Object)
                .unwrap_or_else(default_session_params),
        );

        let mut payload = json!({
            "modelName": config.model_name.as_str(),
            "verbose": verbose,
            "domSettleTimeoutMs": config.dom_settle_timeout_ms,
            "browserbaseSessionId": config.browserbase_session_id,
            "browserbaseSessionCreateParams": create_params,
            "selfHeal": config.self_heal,
            "waitForCaptchaSolves": config.wait_for_captcha_solves,
            "actTimeoutMs": config.act_timeout_ms,
            "experimental": config.experimental,
        });

        if let Some(prompt) = &config.system_prompt {
            payload["systemPrompt"] = JsonValue::String(prompt.clone());
        }
        if let Some(options) = &config.model_client_options {
            payload["modelClientOptions"] = JsonValue::Object(options.clone());
        }

        let url = format!("{}/sessions/start", self.base_api_url());
        let mut headers = self.base_headers(&api_key, &project_id)?;
        headers.insert(
            "x-model-api-key",
            header_value(&model_api_key)
                .map_err(|err| StagehandClientError::Api(err.to_string()))?,
        );
        headers.insert("x-language", HeaderValue::from_static("rust"));
        headers.insert(
            "x-sdk-version",
            header_value(env!("CARGO_PKG_VERSION"))
                .map_err(|err| StagehandClientError::Api(err.to_string()))?,
        );

        let response = self.http_client.post_json(&url, headers, &payload).await?;

        if response.status != 200 {
            return Err(StagehandClientError::Api(format!(
                "session start failed with status {}: {}",
                response.status, response.body
            )));
        }

        let body: JsonValue = serde_json::from_str(&response.body)?;
        let session_id = body
            .get("data")
            .and_then(|data| data.get("sessionId"))
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                StagehandClientError::Api("session response missing sessionId".into())
            })?;
        Ok(session_id.to_string())
    }

    pub(crate) async fn execute_api(
        &self,
        method: &str,
        payload: JsonValue,
    ) -> Result<JsonValue, StagehandClientError> {
        if !self.use_api() {
            return Err(StagehandClientError::Unsupported(
                "Stagehand API usage is disabled in this configuration",
            ));
        }

        let session_id = self.ensure_session().await?;
        let url = format!("{}/sessions/{}/{}", self.base_api_url(), session_id, method);

        let prepared_payload = self.prepare_execute_payload(payload);

        let _lock = self.session_lock.lock().await;
        let headers = self.build_execute_headers()?;
        let mut response = self
            .http_client
            .post_streaming_lines(&url, headers, &prepared_payload)
            .await?;

        let mut final_result: Option<JsonValue> = None;
        while let Some(line_result) = response.stream.next().await {
            let line = line_result?;
            let trimmed = line.trim();

            if trimmed.is_empty() || trimmed.starts_with(':') {
                continue;
            }

            let data_segment = trimmed.strip_prefix("data: ").unwrap_or(trimmed);
            if data_segment.trim().is_empty() {
                continue;
            }

            match serde_json::from_str::<JsonValue>(data_segment) {
                Ok(message) => match self.handle_stream_message(&message) {
                    Ok(Some(result)) => final_result = Some(result),
                    Ok(None) => {}
                    Err(err) => return Err(err),
                },
                Err(err) => {
                    self.logger.error(
                        format!("Could not parse SSE line: {} ({err})", data_segment),
                        Some("api"),
                        None,
                    );
                }
            }
        }

        final_result
            .ok_or_else(|| StagehandClientError::Api("API stream finished without a result".into()))
    }

    fn prepare_execute_payload(&self, mut payload: JsonValue) -> JsonValue {
        if let Some(base_url) = self.resolve_model_base_url(&payload) {
            if let Some(obj) = payload.as_object_mut() {
                let entry = obj
                    .entry("modelClientOptions".to_string())
                    .or_insert_with(|| JsonValue::Object(JsonMap::new()));
                if let Some(options) = entry.as_object_mut() {
                    options.insert("baseURL".to_string(), JsonValue::String(base_url));
                    options.remove("api_base");
                }
            }
        }

        convert_keys_to_camel_case(payload)
    }

    fn resolve_model_base_url(&self, payload: &JsonValue) -> Option<String> {
        let payload_options = payload
            .get("modelClientOptions")
            .and_then(JsonValue::as_object);
        let config_options = self.config().model_client_options.as_ref();
        lookup_base_url(payload_options, config_options)
    }

    fn handle_stream_message(
        &self,
        message: &JsonValue,
    ) -> Result<Option<JsonValue>, StagehandClientError> {
        match message.get("type").and_then(JsonValue::as_str) {
            Some("system") => {
                if let Some(data) = message.get("data") {
                    self.handle_system_message(data)
                } else {
                    Ok(None)
                }
            }
            Some("log") => {
                if let Some(payload) = message.get("data") {
                    self.handle_log_payload(payload);
                }
                Ok(None)
            }
            Some("metrics") => {
                if let Some(payload) = message.get("data") {
                    self.handle_metrics_payload(payload);
                }
                Ok(None)
            }
            Some(other) => {
                self.logger.debug(
                    format!("Unhandled API message type: {}", other),
                    Some("api"),
                    Some(message.clone()),
                );
                Ok(None)
            }
            None => Ok(None),
        }
    }

    fn handle_system_message(
        &self,
        data: &JsonValue,
    ) -> Result<Option<JsonValue>, StagehandClientError> {
        match data
            .get("status")
            .and_then(JsonValue::as_str)
            .unwrap_or_default()
        {
            "error" => {
                let error_message = data
                    .get("error")
                    .and_then(JsonValue::as_str)
                    .unwrap_or("Unknown error");
                self.logger
                    .error(error_message.to_string(), Some("api"), Some(data.clone()));
                Err(StagehandClientError::Api(error_message.to_string()))
            }
            "finished" => Ok(data.get("result").cloned()),
            _ => Ok(None),
        }
    }

    fn handle_log_payload(&self, payload: &JsonValue) {
        let logger = self.logger();
        sync_log_handler(payload, logger.as_ref());
    }

    fn handle_metrics_payload(&self, payload: &JsonValue) {
        let function_name = payload
            .get("function")
            .or_else(|| payload.get("functionName"))
            .and_then(JsonValue::as_str);
        let prompt_tokens = payload
            .get("promptTokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let completion_tokens = payload
            .get("completionTokens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);
        let inference_time_ms = payload
            .get("inferenceTimeMs")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0);

        if let Some(name) = function_name {
            if let Some(function) = stagehand_function_from_str(name) {
                self.update_metrics(
                    function,
                    prompt_tokens,
                    completion_tokens,
                    inference_time_ms,
                );
            } else if prompt_tokens > 0 || completion_tokens > 0 || inference_time_ms > 0 {
                if let Ok(mut guard) = self.metrics.lock() {
                    guard.total_prompt_tokens += prompt_tokens;
                    guard.total_completion_tokens += completion_tokens;
                    guard.total_inference_time_ms += inference_time_ms;
                }
            }
        }
    }
    fn base_api_url(&self) -> String {
        self.config().api_url.trim_end_matches('/').to_string()
    }

    fn base_headers(
        &self,
        api_key: &str,
        project_id: &str,
    ) -> Result<HeaderMap, StagehandClientError> {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            "x-bb-api-key",
            header_value(api_key).map_err(|err| StagehandClientError::Api(err.to_string()))?,
        );
        headers.insert(
            "x-bb-project-id",
            header_value(project_id).map_err(|err| StagehandClientError::Api(err.to_string()))?,
        );
        headers.insert(
            "x-sent-at",
            header_value(&Utc::now().to_rfc3339())
                .map_err(|err| StagehandClientError::Api(err.to_string()))?,
        );
        Ok(headers)
    }

    fn build_execute_headers(&self) -> Result<HeaderMap, StagehandClientError> {
        let config = self.config();
        let api_key = config
            .api_key
            .clone()
            .ok_or_else(|| StagehandClientError::Api("browserbase API key is required".into()))?;
        let project_id = config.project_id.clone().ok_or_else(|| {
            StagehandClientError::Api("browserbase project id is required".into())
        })?;
        let mut headers = self.base_headers(&api_key, &project_id)?;
        if let Some(model_key) = &config.model_api_key {
            headers.insert(
                "x-model-api-key",
                header_value(model_key)
                    .map_err(|err| StagehandClientError::Api(err.to_string()))?,
            );
        }
        headers.insert("x-stream-response", HeaderValue::from_static("true"));
        headers.insert(CONNECTION, HeaderValue::from_static("keep-alive"));
        Ok(headers)
    }
}

impl StagehandClient<Arc<ChromiumoxideRuntime>> {
    /// Convenience constructor that wires the chromiumoxide adapter automatically.
    pub fn with_chromiumoxide_runtime(
        config: StagehandConfig,
        runtime: Arc<ChromiumoxideRuntime>,
    ) -> Result<Self, BrowserError> {
        let adapter = Arc::new(crate::adapter::chromiumoxide::ChromiumoxideAdapter::new(
            runtime.clone(),
        ));
        Self::new_with_default_dom_script(config, runtime, adapter)
    }
}

fn build_logger(config: &StagehandConfig) -> StagehandLogger {
    let mut log_config = LogConfig::default();
    log_config.verbose = config.verbose;
    log_config.use_rich = config.use_rich_logging;
    log_config.env = match config.env {
        Environment::Browserbase => "BROWSERBASE".to_string(),
        Environment::Local => "LOCAL".to_string(),
    };

    if let Some(external) = config.logger.as_ref() {
        let callback = Arc::clone(external);
        let external_logger: LogCallback = Arc::new(move |record| {
            if let Some(category) = &record.category {
                callback(&format!(
                    "[{}][{}] {}",
                    record.level.label(),
                    category,
                    record.message
                ));
            } else {
                callback(&format!("[{}] {}", record.level.label(), record.message));
            }
        });
        log_config.external_logger = Some(external_logger);
    }

    StagehandLogger::with_config(log_config)
}

fn default_session_params() -> JsonValue {
    json!({
        "browserSettings": {
            "blockAds": true,
            "viewport": {
                "width": 1024,
                "height": 768
            }
        }
    })
}

fn header_value(value: &str) -> Result<HeaderValue, reqwest::header::InvalidHeaderValue> {
    HeaderValue::from_str(value)
}

fn lookup_base_url(
    payload_options: Option<&JsonMap<String, JsonValue>>,
    config_options: Option<&JsonMap<String, JsonValue>>,
) -> Option<String> {
    let candidates = [
        payload_options
            .and_then(|opts| opts.get("baseURL"))
            .and_then(JsonValue::as_str),
        payload_options
            .and_then(|opts| opts.get("api_base"))
            .and_then(JsonValue::as_str),
        config_options
            .and_then(|opts| opts.get("baseURL"))
            .and_then(JsonValue::as_str),
        config_options
            .and_then(|opts| opts.get("api_base"))
            .and_then(JsonValue::as_str),
    ];

    for candidate in candidates.into_iter().flatten() {
        if !candidate.is_empty() {
            return Some(candidate.to_string());
        }
    }

    None
}

fn stagehand_function_from_str(name: &str) -> Option<StagehandFunctionName> {
    match name.to_ascii_lowercase().as_str() {
        "act" => Some(StagehandFunctionName::Act),
        "extract" => Some(StagehandFunctionName::Extract),
        "observe" => Some(StagehandFunctionName::Observe),
        "agent" => Some(StagehandFunctionName::Agent),
        _ => None,
    }
}

fn convert_keys_to_camel_case(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Object(map) => {
            let mut converted = JsonMap::new();
            for (key, inner) in map {
                converted.insert(snake_to_camel_case(&key), convert_keys_to_camel_case(inner));
            }
            JsonValue::Object(converted)
        }
        JsonValue::Array(items) => {
            JsonValue::Array(items.into_iter().map(convert_keys_to_camel_case).collect())
        }
        other => other,
    }
}

fn snake_to_camel_case(value: &str) -> String {
    if !value.contains('_') {
        return value.to_string();
    }

    let mut segments = value.split('_');
    let mut result = segments.next().unwrap_or_default().to_string();
    for segment in segments {
        if segment.is_empty() {
            continue;
        }
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            result.push(first.to_ascii_uppercase());
            for ch in chars {
                result.push(ch.to_ascii_lowercase());
            }
        }
    }
    result
}

fn prepare_session_create_params(value: JsonValue) -> JsonValue {
    let mut converted = convert_keys_to_camel_case(value);
    if let Some(obj) = converted.as_object_mut() {
        if let Some(timeout) = obj.remove("apiTimeout") {
            obj.insert("timeout".to_string(), timeout);
        }
    }
    converted
}

fn parse_replay_metrics(data: &JsonValue) -> StagehandMetrics {
    let mut metrics = StagehandMetrics::default();

    if let Some(pages) = data.get("pages").and_then(JsonValue::as_array) {
        for page in pages {
            if let Some(actions) = page.get("actions").and_then(JsonValue::as_array) {
                for action in actions {
                    let method = action
                        .get("method")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("");
                    if let Some(usage) = action.get("tokenUsage").and_then(JsonValue::as_object) {
                        let prompt_tokens = usage
                            .get("inputTokens")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0);
                        let completion_tokens = usage
                            .get("outputTokens")
                            .and_then(JsonValue::as_u64)
                            .unwrap_or(0);
                        let inference_time_ms =
                            usage.get("timeMs").and_then(JsonValue::as_u64).unwrap_or(0);

                        if let Some(function) = stagehand_function_from_str(method) {
                            metrics.record(
                                function,
                                prompt_tokens,
                                completion_tokens,
                                inference_time_ms,
                            );
                        } else {
                            metrics.total_prompt_tokens += prompt_tokens;
                            metrics.total_completion_tokens += completion_tokens;
                            metrics.total_inference_time_ms += inference_time_ms;
                        }
                    }
                }
            }
        }
    }

    metrics
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{BrowserbasePlan, LocalPlan};
    use crate::config::{Environment, StagehandConfig};
    use crate::context::{StagehandAdapter, StagehandAdapterError};
    use crate::dom_scripts;
    use crate::types::page::ActOptions;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct RecordingRuntime {
        browserbase_calls: Mutex<usize>,
        local_calls: Mutex<usize>,
        pages: Mutex<HashMap<String, String>>,
        next_page: Mutex<u32>,
    }

    #[async_trait]
    impl BrowserRuntime for RecordingRuntime {
        async fn connect_browserbase(
            &self,
            _plan: &BrowserbasePlan,
        ) -> Result<(), BrowserRuntimeError> {
            *self.browserbase_calls.lock().unwrap() += 1;
            Ok(())
        }

        async fn launch_local(&self, _plan: &LocalPlan) -> Result<(), BrowserRuntimeError> {
            *self.local_calls.lock().unwrap() += 1;
            Ok(())
        }

        async fn new_page(&self, url: &str) -> Result<String, BrowserRuntimeError> {
            let mut next = self.next_page.lock().unwrap();
            let id = format!("page-{}", *next);
            *next += 1;
            self.pages
                .lock()
                .unwrap()
                .insert(id.clone(), format!("content:{url}"));
            Ok(id)
        }

        async fn page_content(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
            Ok(self.pages.lock().unwrap().get(page_id).cloned())
        }

        async fn list_pages(&self) -> Result<Vec<String>, BrowserRuntimeError> {
            Ok(self.pages.lock().unwrap().keys().cloned().collect())
        }

        async fn main_frame_id(
            &self,
            _page_id: &str,
        ) -> Result<Option<String>, BrowserRuntimeError> {
            Ok(None)
        }
    }

    #[derive(Default)]
    struct RecordingAdapter {
        inject_calls: Mutex<Vec<String>>,
        debug_logs: Mutex<Vec<String>>,
        error_logs: Mutex<Vec<String>>,
        active_pages: Mutex<Vec<String>>,
    }

    #[derive(Default)]
    struct MockHttpClient {
        post_json_responses: Mutex<Vec<HttpResponse>>,
        post_stream_responses: Mutex<Vec<MockStreamResponse>>,
        get_responses: Mutex<Vec<HttpResponse>>,
        requests: Mutex<Vec<(String, JsonValue)>>,
        get_requests: Mutex<Vec<String>>,
    }

    struct MockStreamResponse {
        status: u16,
        lines: Vec<String>,
        error_body: Option<String>,
    }

    impl MockStreamResponse {
        fn success(lines: Vec<String>) -> Self {
            Self {
                status: 200,
                lines,
                error_body: None,
            }
        }
    }

    impl MockHttpClient {
        fn new(post_json_responses: Vec<HttpResponse>) -> Self {
            Self {
                post_json_responses: Mutex::new(post_json_responses),
                ..Default::default()
            }
        }

        fn with_stream_and_get(
            post_json_responses: Vec<HttpResponse>,
            stream_responses: Vec<MockStreamResponse>,
            get_responses: Vec<HttpResponse>,
        ) -> Self {
            Self {
                post_json_responses: Mutex::new(post_json_responses),
                post_stream_responses: Mutex::new(stream_responses),
                get_responses: Mutex::new(get_responses),
                requests: Mutex::new(Vec::new()),
                get_requests: Mutex::new(Vec::new()),
            }
        }

        fn recorded_requests(&self) -> Vec<(String, JsonValue)> {
            self.requests.lock().unwrap().clone()
        }

        fn recorded_get_requests(&self) -> Vec<String> {
            self.get_requests.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl StagehandHttp for MockHttpClient {
        async fn post_json(
            &self,
            url: &str,
            _headers: HeaderMap,
            body: &JsonValue,
        ) -> Result<HttpResponse, StagehandClientError> {
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone()));
            let mut responses = self.post_json_responses.lock().unwrap();
            if responses.is_empty() {
                return Err(StagehandClientError::Api(
                    "no mock response available".into(),
                ));
            }
            Ok(responses.remove(0))
        }

        async fn post_streaming_lines(
            &self,
            url: &str,
            _headers: HeaderMap,
            body: &JsonValue,
        ) -> Result<StreamingHttpResponse, StagehandClientError> {
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone()));

            let mut responses = self.post_stream_responses.lock().unwrap();
            if responses.is_empty() {
                return Err(StagehandClientError::Api(
                    "no mock stream response available".into(),
                ));
            }
            let response = responses.remove(0);

            if response.status != 200 {
                return Err(StagehandClientError::Api(format!(
                    "request failed with status {}: {}",
                    response.status,
                    response.error_body.unwrap_or_default()
                )));
            }

            let stream = futures_util::stream::iter(
                response
                    .lines
                    .into_iter()
                    .map(|line| Ok::<String, StagehandClientError>(line)),
            );

            Ok(StreamingHttpResponse {
                stream: Box::pin(stream),
            })
        }

        async fn get_json(
            &self,
            url: &str,
            _headers: HeaderMap,
        ) -> Result<HttpResponse, StagehandClientError> {
            self.get_requests.lock().unwrap().push(url.to_string());

            let mut responses = self.get_responses.lock().unwrap();
            if responses.is_empty() {
                return Err(StagehandClientError::Api(
                    "no mock get response available".into(),
                ));
            }
            Ok(responses.remove(0))
        }
    }

    #[async_trait]
    impl StagehandAdapter for RecordingAdapter {
        async fn inject_dom_script(
            &self,
            page_id: &String,
            _script: &str,
        ) -> Result<(), StagehandAdapterError> {
            self.inject_calls.lock().unwrap().push(page_id.clone());
            Ok(())
        }

        fn log_debug(&self, message: &str, _category: &'static str) {
            self.debug_logs.lock().unwrap().push(message.to_string());
        }

        fn log_error(&self, message: &str, _category: &'static str) {
            self.error_logs.lock().unwrap().push(message.to_string());
        }

        fn notify_active_page(&self, page_id: &String) {
            self.active_pages.lock().unwrap().push(page_id.clone());
        }
    }

    #[tokio::test]
    async fn ensure_page_ready_initializes_browserbase_runtime_once() {
        let mut config = StagehandConfig::default();
        config.browserbase_session_id = Some("existing".into());
        let runtime = {
            let runtime = RecordingRuntime::default();
            runtime
                .pages
                .lock()
                .unwrap()
                .insert("page-1".to_string(), "content:existing".to_string());
            runtime
        };
        let adapter = Arc::new(RecordingAdapter::default());
        let http = Arc::new(MockHttpClient::new(vec![]));
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client");

        client
            .ensure_page_ready("page-1", Some("frame-1".into()))
            .await
            .unwrap();
        client.ensure_page_ready("page-1", None).await.unwrap();

        assert_eq!(
            *client.browser().runtime().browserbase_calls.lock().unwrap(),
            1
        );
        assert_eq!(
            adapter.inject_calls.lock().unwrap().as_slice(),
            &["page-1".to_string()]
        );
    }

    #[tokio::test]
    async fn remove_page_clears_context_state() {
        let config = StagehandConfig::default();
        let runtime = RecordingRuntime::default();
        runtime
            .pages
            .lock()
            .unwrap()
            .insert("page-1".to_string(), "content:page-1".to_string());
        let adapter = Arc::new(RecordingAdapter::default());
        let http = Arc::new(MockHttpClient::new(vec![]));
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client");

        client
            .ensure_page_ready("page-1", Some("frame-1".into()))
            .await
            .unwrap();
        client
            .with_context(|ctx| {
                Box::pin(async move {
                    ctx.set_active_page(&"page-1".to_string())?;
                    assert!(ctx.page_by_frame_id("frame-1").is_some());
                    Ok(())
                })
            })
            .await
            .unwrap();

        assert!(client.remove_page("page-1").await.unwrap());
        client
            .browser()
            .runtime()
            .pages
            .lock()
            .unwrap()
            .remove("page-1");
        client
            .with_context(|ctx| {
                Box::pin(async move {
                    assert!(ctx.page(&"page-1".to_string()).is_none());
                    assert!(ctx.page_by_frame_id("frame-1").is_none());
                    assert!(ctx.active_page().is_none());
                    Ok(())
                })
            })
            .await
            .unwrap();

        assert!(!client.remove_page("page-1").await.unwrap());
    }

    #[tokio::test]
    async fn ensure_initialized_registers_existing_pages() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        let runtime = RecordingRuntime::default();
        runtime
            .pages
            .lock()
            .unwrap()
            .insert("existing".into(), "content".into());
        let adapter = Arc::new(RecordingAdapter::default());
        let http = Arc::new(MockHttpClient::new(vec![]));
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client");

        client.ensure_initialized().await.unwrap();

        assert!(
            adapter
                .inject_calls
                .lock()
                .unwrap()
                .contains(&"existing".to_string())
        );

        client
            .with_context(|ctx| {
                Box::pin(async move {
                    assert!(ctx.page(&"existing".to_string()).is_some());
                    assert!(ctx.active_page().is_some());
                    Ok(())
                })
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ensure_page_ready_initializes_local_runtime() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let http = Arc::new(MockHttpClient::new(vec![]));
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client");

        client.ensure_page_ready("page-1", None).await.unwrap();

        assert_eq!(*client.browser().runtime().local_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn open_page_registers_and_returns_content() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let http = Arc::new(MockHttpClient::new(vec![]));
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client");

        let page_id = client.open_page("https://example.com").await.unwrap();

        assert!(adapter.inject_calls.lock().unwrap().contains(&page_id));
        assert!(adapter.active_pages.lock().unwrap().contains(&page_id));

        let content = client
            .browser()
            .runtime()
            .page_content(&page_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(content, "content:https://example.com");
    }

    #[test]
    fn log_helpers_delegate_to_adapter() {
        let config = StagehandConfig::default();
        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let http = Arc::new(MockHttpClient::new(vec![]));
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client");

        client.log_debug("message", "test");
        client.log_error("failure", "test");

        assert_eq!(
            adapter.debug_logs.lock().unwrap().as_slice(),
            &["message".to_string()]
        );
        assert_eq!(
            adapter.error_logs.lock().unwrap().as_slice(),
            &["failure".to_string()]
        );
    }

    #[test]
    fn log_helpers_use_config_logger() {
        let captures = Arc::new(Mutex::new(Vec::new()));
        let capture_clone = Arc::clone(&captures);
        let mut config = StagehandConfig::default();
        let callback: LoggerCallback = Arc::new(move |message: &str| {
            capture_clone.lock().unwrap().push(message.to_string());
        });
        config.logger = Some(callback);

        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let http = Arc::new(MockHttpClient::new(vec![]));
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter,
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client");

        client.log_error("failure", "test");

        let logs = captures.lock().unwrap();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("failure"));
        assert!(logs[0].contains("ERROR"));
    }

    #[tokio::test]
    async fn stagehand_page_act_calls_remote_api() {
        let post_json_responses = vec![HttpResponse {
            status: 200,
            body: r#"{"success":true,"data":{"sessionId":"sess-1"}}"#.to_string(),
        }];
        let stream_responses = vec![MockStreamResponse::success(vec![
            r#"data: {"type":"log","data":{"message":{"message":"Act completed","level":1,"category":"act"}}}"#.to_string(),
            r#"data: {"type":"system","data":{"status":"finished","result":{"success":true,"action":"click","message":"ok","metadata":{"promptTokens":10,"completionTokens":5,"inferenceTimeMs":120}}}}"#.to_string(),
        ])];
        let mock_http = Arc::new(MockHttpClient::with_stream_and_get(
            post_json_responses,
            stream_responses,
            vec![],
        ));

        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        config.use_api = true;
        config.api_key = Some("bb-key".into());
        config.project_id = Some("proj-1".into());
        config.model_api_key = Some("model-key".into());
        config.api_url = "https://example.com".into();

        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            mock_http.clone(),
        )
        .expect("client");

        client
            .ensure_page_ready("page-1", None)
            .await
            .expect("ensure");

        let options = ActOptions {
            action: "click".into(),
            variables: None,
            model_name: None,
            dom_settle_timeout_ms: None,
            timeout_ms: None,
            model_client_options: None,
        };

        let page = client.page("page-1");
        let result = page.act(options).await.expect("act result");

        assert!(result.success);
        assert_eq!(result.action, "click");

        let metrics = client.metrics().lock().unwrap().clone();
        assert_eq!(metrics.act_prompt_tokens, 10);
        assert_eq!(metrics.act_completion_tokens, 5);
        assert_eq!(metrics.act_inference_time_ms, 120);

        let requests = mock_http.recorded_requests();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].0.ends_with("/sessions/start"));
        assert!(requests[1].0.ends_with("/sessions/sess-1/act"));
        assert!(requests[1].1.to_string().contains("\"action\":\"click\""));
    }

    #[tokio::test]
    async fn fetch_replay_metrics_aggregates_usage() {
        let post_json_responses = vec![HttpResponse {
            status: 200,
            body: r#"{"success":true,"data":{"sessionId":"sess-2"}}"#.to_string(),
        }];
        let get_responses = vec![HttpResponse {
            status: 200,
            body: r#"{"success":true,"data":{"pages":[{"actions":[{"method":"act","tokenUsage":{"inputTokens":4,"outputTokens":2,"timeMs":50}},{"method":"observe","tokenUsage":{"inputTokens":1,"outputTokens":1,"timeMs":20}}]}]}}"#.to_string(),
        }];

        let mock_http = Arc::new(MockHttpClient::with_stream_and_get(
            post_json_responses,
            Vec::new(),
            get_responses,
        ));

        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        config.use_api = true;
        config.api_key = Some("bb-key".into());
        config.project_id = Some("proj-1".into());
        config.model_api_key = Some("model-key".into());
        config.api_url = "https://example.com".into();

        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let client = StagehandClient::new_with_http_client(
            config,
            runtime,
            adapter,
            dom_scripts::stagehand_dom_script(),
            mock_http.clone(),
        )
        .expect("client");

        let metrics = client.fetch_replay_metrics().await.expect("metrics");
        assert_eq!(metrics.act_prompt_tokens, 4);
        assert_eq!(metrics.act_completion_tokens, 2);
        assert_eq!(metrics.observe_prompt_tokens, 1);
        assert_eq!(metrics.observe_completion_tokens, 1);
        assert_eq!(metrics.total_prompt_tokens, 5);
        assert_eq!(metrics.total_completion_tokens, 3);
        assert_eq!(metrics.total_inference_time_ms, 70);

        let get_requests = mock_http.recorded_get_requests();
        assert_eq!(get_requests.len(), 1);
        assert!(get_requests[0].ends_with("/sessions/sess-2/replay"));
    }
}
