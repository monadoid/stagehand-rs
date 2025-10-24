//! High-level Stagehand client scaffolding.
//!
//! This module stitches together the configuration-driven browser planning
//! logic with the `StagehandContext` wrapper, providing a stub implementation
//! that we can grow towards the full Python feature set.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::Utc;
use futures_util::future::BoxFuture;
use reqwest::Client as HttpClient;
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value as JsonValue, json};
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;

use crate::browser::{
    BrowserError, BrowserPlan, BrowserRuntime, BrowserRuntimeError, StagehandBrowser,
};
use crate::config::{Environment, LoggerCallback, StagehandConfig};
use crate::context::{StagehandAdapter, StagehandContext, StagehandContextError};
use crate::dom_scripts;
use crate::llm::{OpenAiChatProvider, StagehandLlmClient, StagehandLlmError};
use crate::logging::{LogCallback, LogConfig, StagehandLogger};
use crate::metrics::{StagehandFunctionName, StagehandMetrics};
use crate::runtime::ChromiumoxideRuntime;

#[derive(Debug, Clone)]
pub struct HttpResponse {
    status: u16,
    body: String,
}

#[async_trait]
pub trait StagehandHttp: Send + Sync {
    async fn post_json(
        &self,
        url: &str,
        headers: HeaderMap,
        body: &JsonValue,
    ) -> Result<HttpResponse, StagehandClientError>;
}

#[derive(Default)]
struct ReqwestStagehandHttpClient {
    client: HttpClient,
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

    pub fn log_debug(&self, message: &str, category: &'static str) {
        self.logger.debug(message, Some(category), None);
        self.adapter.log_debug(message, category);
    }

    pub fn log_error(&self, message: &str, category: &'static str) {
        self.logger.error(message, Some(category), None);
        self.adapter.log_error(message, category);
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
        f(ctx).await.map_err(StagehandClientError::Context)
    }

    /// Register a page and ensure the Stagehand DOM script has been injected.
    pub async fn ensure_page_ready(
        &self,
        page_id: impl Into<String>,
        frame_id: Option<String>,
    ) -> Result<(), StagehandClientError> {
        let page_id = page_id.into();
        let frame_clone = frame_id.clone();
        self.with_context(move |ctx| {
            let page_id = page_id.clone();
            Box::pin(async move {
                ctx.register_page(page_id.clone(), frame_clone);
                ctx.ensure_dom_script(&page_id).await?;
                Ok(())
            })
        })
        .await
    }

    /// Open a new page using the underlying browser runtime and ensure it is registered.
    pub async fn open_page(&self, url: &str) -> Result<String, StagehandClientError> {
        self.ensure_initialized().await?;
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

        let create_params = config
            .browserbase_session_create_params()
            .map(JsonValue::Object)
            .unwrap_or_else(default_session_params);

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

        let _lock = self.session_lock.lock().await;
        let headers = self.build_execute_headers()?;
        let response = self.http_client.post_json(&url, headers, &payload).await?;

        if response.status != 200 {
            return Err(StagehandClientError::Api(format!(
                "request failed with status {}: {}",
                response.status, response.body
            )));
        }

        let body: JsonValue = serde_json::from_str(&response.body)?;
        Ok(body)
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
        headers.insert("x-stream-response", HeaderValue::from_static("false"));
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
        responses: Mutex<Vec<HttpResponse>>,
        requests: Mutex<Vec<(String, JsonValue)>>,
    }

    impl MockHttpClient {
        fn new(responses: Vec<HttpResponse>) -> Self {
            Self {
                responses: Mutex::new(responses),
                requests: Mutex::new(Vec::new()),
            }
        }

        fn recorded_requests(&self) -> Vec<(String, JsonValue)> {
            self.requests.lock().unwrap().clone()
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
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                return Err(StagehandClientError::Api(
                    "no mock response available".into(),
                ));
            }
            self.requests
                .lock()
                .unwrap()
                .push((url.to_string(), body.clone()));
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
        let responses = vec![
            HttpResponse {
                status: 200,
                body: r#"{"success":true,"data":{"sessionId":"sess-1"}}"#.to_string(),
            },
            HttpResponse {
                status: 200,
                body: r#"{"success":true,"message":"ok","action":"click","metadata":{"promptTokens":10,"completionTokens":5,"inferenceTimeMs":120}}"#.to_string(),
            },
        ];
        let mock_http = Arc::new(MockHttpClient::new(responses));

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
}
