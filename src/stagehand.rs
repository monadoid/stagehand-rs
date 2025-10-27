//! High-level Stagehand facade mirroring the Python SDK.
//!
//! This module wraps [`StagehandClient`] with a lightweight handle that exposes
//! ergonomic helpers for initialisation, page management, and the familiar
//! `page.act/observe/extract` workflow used in the Python implementation.

use std::sync::Arc;

use crate::browser::{BrowserError, BrowserRuntime};
use crate::client::{StagehandClient, StagehandClientError};
use crate::config::StagehandConfig;
use crate::page::StagehandPage;
use crate::runtime::ChromiumoxideRuntime;
use crate::types::page::{
    ActOptions, ActResult, ExtractOptions, ExtractResult, NavigateOptions, ObserveOptions,
    ObserveResult,
};

/// Lightweight helper mirroring the Python LivePage proxy.
pub struct LivePage<'stagehand, R: BrowserRuntime> {
    stagehand: &'stagehand Stagehand<R>,
}

/// Convenience error type surfaced by the [`Stagehand`] facade.
#[derive(Debug, thiserror::Error)]
pub enum StagehandError {
    #[error(transparent)]
    Browser(#[from] BrowserError),
    #[error(transparent)]
    Client(#[from] StagehandClientError),
}

/// High-level Stagehand wrapper that mirrors the Python `Stagehand` class.
pub struct Stagehand<R: BrowserRuntime> {
    client: StagehandClient<R>,
}

impl Stagehand<Arc<ChromiumoxideRuntime>> {
    /// Construct a Stagehand instance backed by the default Chromiumoxide runtime.
    pub fn new_local(
        config: StagehandConfig,
        runtime: Arc<ChromiumoxideRuntime>,
    ) -> Result<Self, StagehandError> {
        let client = StagehandClient::with_chromiumoxide_runtime(config, runtime)
            .map_err(StagehandError::Browser)?;
        Ok(Self::from_client(client))
    }
}

impl<R: BrowserRuntime + 'static> Stagehand<R> {
    /// Wrap an existing [`StagehandClient`] in the high-level facade.
    pub fn from_client(client: StagehandClient<R>) -> Self {
        Self { client }
    }

    /// Access the underlying client for advanced operations.
    pub fn client(&self) -> &StagehandClient<R> {
        &self.client
    }

    /// Convenience helper to mirror Python's live page proxy.
    pub fn live_page(&self) -> LivePage<'_, R> {
        LivePage { stagehand: self }
    }

    /// Ensure the underlying runtime (local or remote) has been initialised.
    pub async fn init(&self) -> Result<(), StagehandClientError> {
        self.client.ensure_initialized().await
    }

    /// Gracefully shut down the runtime and release resources.
    pub async fn close(&self) -> Result<(), StagehandClientError> {
        self.client.shutdown().await
    }

    /// Open a page, mark it active, and return a [`StagehandPage`] handle.
    pub async fn open_page(&self, url: &str) -> Result<StagehandPage<'_, R>, StagehandClientError> {
        let page_id = self.client.open_page(url).await?;
        Ok(self.client.page(page_id))
    }

    /// Retrieve the currently active page, returning an error if none is set.
    pub async fn page(&self) -> Result<StagehandPage<'_, R>, StagehandClientError> {
        match self.client.active_page_id().await? {
            Some(id) => Ok(self.client.page(id)),
            None => Err(StagehandClientError::Unsupported(
                "no active page; open a page first",
            )),
        }
    }

    /// Manually set the active page identifier.
    pub async fn set_active_page(&self, page_id: String) -> Result<(), StagehandClientError> {
        self.client.set_active_page(&page_id).await
    }

    /// Navigate the active page to the provided URL.
    pub async fn goto(&self, url: &str) -> Result<(), StagehandClientError> {
        let page = self.page().await?;
        page.goto(url).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{BrowserRuntimeError, BrowserbasePlan, LocalPlan, RuntimeTargetEvent};
    use crate::client::{HttpResponse, StagehandClientError, StagehandHttp, StreamingHttpResponse};
    use crate::config::{Environment, StagehandConfig};
    use crate::context::{StagehandAdapter, StagehandAdapterError};
    use crate::dom_scripts;
    use async_trait::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Mutex;

    #[derive(Default)]
    struct MockRuntime {
        pages: Mutex<HashMap<String, String>>,
        next_page: Mutex<u32>,
    }

    #[async_trait]
    impl BrowserRuntime for Arc<MockRuntime> {
        async fn connect_browserbase(
            &self,
            _plan: &BrowserbasePlan,
        ) -> Result<(), BrowserRuntimeError> {
            Ok(())
        }

        async fn launch_local(&self, _plan: &LocalPlan) -> Result<(), BrowserRuntimeError> {
            Ok(())
        }

        async fn new_page(&self, url: &str) -> Result<String, BrowserRuntimeError> {
            let mut next = self.next_page.lock().await;
            let id = format!("page-{}", *next);
            *next += 1;
            self.pages.lock().await.insert(id.clone(), url.to_string());
            Ok(id)
        }

        async fn page_content(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
            Ok(self.pages.lock().await.get(page_id).cloned())
        }

        async fn list_pages(&self) -> Result<Vec<String>, BrowserRuntimeError> {
            Ok(self.pages.lock().await.keys().cloned().collect())
        }

        async fn target_event_stream(
            &self,
        ) -> Result<tokio::sync::broadcast::Receiver<RuntimeTargetEvent>, BrowserRuntimeError>
        {
            Err(BrowserRuntimeError::Unsupported(
                "mock runtime does not support target events".into(),
            ))
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
        injected: Mutex<Vec<String>>,
        active: StdMutex<Vec<String>>,
    }

    #[async_trait]
    impl StagehandAdapter for RecordingAdapter {
        async fn inject_dom_script(
            &self,
            page_id: &crate::context::PageId,
            _script: &str,
        ) -> Result<(), StagehandAdapterError> {
            self.injected.lock().await.push(page_id.clone());
            Ok(())
        }

        fn log_debug(&self, _message: &str, _category: &'static str) {}

        fn log_error(&self, _message: &str, _category: &'static str) {}

        fn notify_active_page(&self, page_id: &crate::context::PageId) {
            if let Ok(mut guard) = self.active.lock() {
                guard.push(page_id.clone());
            }
        }
    }

    #[derive(Default)]
    struct NoopHttpClient;

    #[async_trait]
    impl StagehandHttp for NoopHttpClient {
        async fn post_json(
            &self,
            _url: &str,
            _headers: reqwest::header::HeaderMap,
            _body: &serde_json::Value,
        ) -> Result<HttpResponse, StagehandClientError> {
            Err(StagehandClientError::Unsupported(
                "http POST not expected in stagehand tests",
            ))
        }

        async fn post_streaming_lines(
            &self,
            _url: &str,
            _headers: reqwest::header::HeaderMap,
            _body: &serde_json::Value,
        ) -> Result<StreamingHttpResponse, StagehandClientError> {
            Err(StagehandClientError::Unsupported(
                "streaming not supported in tests",
            ))
        }

        async fn get_json(
            &self,
            _url: &str,
            _headers: reqwest::header::HeaderMap,
        ) -> Result<HttpResponse, StagehandClientError> {
            Err(StagehandClientError::Unsupported(
                "http GET not expected in stagehand tests",
            ))
        }
    }

    #[tokio::test]
    async fn stagehand_open_page_tracks_active_page() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        config.use_api = false;

        let runtime = Arc::new(MockRuntime::default());
        let adapter: Arc<dyn StagehandAdapter> = Arc::new(RecordingAdapter::default());

        let http: Arc<dyn StagehandHttp> = Arc::new(NoopHttpClient::default());
        let client = StagehandClient::new_with_http_client(
            config,
            runtime.clone(),
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client construction succeeds");

        let stagehand = Stagehand::from_client(client);
        stagehand.init().await.expect("initialisation succeeds");

        let page = stagehand
            .open_page("https://example.com")
            .await
            .expect("page opens");

        assert_eq!(page.id(), "page-0");

        let active = stagehand.page().await.expect("active page available");
        assert_eq!(active.id(), "page-0");
    }

    #[tokio::test]
    async fn stagehand_page_without_active_returns_error() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        config.use_api = false;

        let runtime = Arc::new(MockRuntime::default());
        let adapter: Arc<dyn StagehandAdapter> = Arc::new(RecordingAdapter::default());

        let http: Arc<dyn StagehandHttp> = Arc::new(NoopHttpClient::default());
        let client = StagehandClient::new_with_http_client(
            config,
            runtime.clone(),
            adapter.clone(),
            dom_scripts::stagehand_dom_script(),
            http,
        )
        .expect("client construction succeeds");

        let stagehand = Stagehand::from_client(client);

        match stagehand.page().await {
            Err(StagehandClientError::Unsupported(msg)) => {
                assert_eq!(msg, "no active page; open a page first")
            }
            Ok(_) => panic!("expected an error when no page is active"),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
}

impl<'stagehand, R> LivePage<'stagehand, R>
where
    R: BrowserRuntime + 'static,
{
    async fn page(&self) -> Result<StagehandPage<'stagehand, R>, StagehandClientError> {
        self.stagehand.page().await
    }

    pub async fn goto(&self, url: &str) -> Result<(), StagehandClientError> {
        self.goto_with_options(url, None).await
    }

    pub async fn goto_with_options(
        &self,
        url: &str,
        options: Option<NavigateOptions>,
    ) -> Result<(), StagehandClientError> {
        let page = self.page().await?;
        match options {
            Some(opts) => page.goto_with_options(url, Some(opts)).await,
            None => page.goto(url).await,
        }
    }

    pub async fn act(&self, options: ActOptions) -> Result<ActResult, StagehandClientError> {
        let page = self.page().await?;
        page.act(options).await
    }

    pub async fn observe(
        &self,
        options: ObserveOptions,
    ) -> Result<Vec<ObserveResult>, StagehandClientError> {
        let page = self.page().await?;
        page.observe(options).await
    }

    pub async fn extract(
        &self,
        options: ExtractOptions,
    ) -> Result<ExtractResult, StagehandClientError> {
        let page = self.page().await?;
        page.extract(options).await
    }
}
