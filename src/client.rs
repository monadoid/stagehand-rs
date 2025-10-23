//! High-level Stagehand client scaffolding.
//!
//! This module stitches together the configuration-driven browser planning
//! logic with the `StagehandContext` wrapper, providing a stub implementation
//! that we can grow towards the full Python feature set.

use std::sync::{Arc, Mutex};

use thiserror::Error;

use crate::browser::{
    BrowserError, BrowserPlan, BrowserRuntime, BrowserRuntimeError, StagehandBrowser,
};
use crate::config::StagehandConfig;
use crate::context::{StagehandAdapter, StagehandContext, StagehandContextError};

/// Orchestrates browser planning/execution and context management.
pub struct StagehandClient<R: BrowserRuntime> {
    browser: StagehandBrowser<R>,
    adapter: Arc<dyn StagehandAdapter>,
    dom_script: String,
    context: Mutex<Option<StagehandContext>>,
    session_lock: Mutex<()>,
}

impl<R: BrowserRuntime> std::fmt::Debug for StagehandClient<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let initialized = self.context.lock().map(|c| c.is_some()).unwrap_or(false);
        f.debug_struct("StagehandClient")
            .field("plan", &self.browser.plan())
            .field("context_initialized", &initialized)
            .finish()
    }
}

/// Errors surfaced by [`StagehandClient`].
#[derive(Debug, Error)]
pub enum StagehandClientError {
    #[error(transparent)]
    Browser(#[from] BrowserRuntimeError),
    #[error(transparent)]
    Context(#[from] StagehandContextError),
    #[error("internal lock poisoned")]
    Poisoned,
}

impl<R: BrowserRuntime> StagehandClient<R> {
    pub fn new(
        config: StagehandConfig,
        runtime: R,
        adapter: Arc<dyn StagehandAdapter>,
        dom_script: impl Into<String>,
    ) -> Result<Self, BrowserError> {
        let browser = StagehandBrowser::new(config, runtime)?;
        Ok(Self {
            browser,
            adapter,
            dom_script: dom_script.into(),
            context: Mutex::new(None),
            session_lock: Mutex::new(()),
        })
    }

    pub fn browser(&self) -> &StagehandBrowser<R> {
        &self.browser
    }

    pub fn plan(&self) -> BrowserPlan {
        self.browser.plan()
    }

    pub fn session_lock(&self) -> &Mutex<()> {
        &self.session_lock
    }

    pub fn log_debug(&self, message: &str, category: &'static str) {
        self.adapter.log_debug(message, category);
    }

    pub fn log_error(&self, message: &str, category: &'static str) {
        self.adapter.log_error(message, category);
    }

    /// Execute the browser plan if needed and ensure the StagehandContext exists.
    pub async fn ensure_initialized(&self) -> Result<(), StagehandClientError> {
        {
            let guard = self
                .context
                .lock()
                .map_err(|_| StagehandClientError::Poisoned)?;
            if guard.is_some() {
                return Ok(());
            }
        }

        self.browser.execute().await?;

        let mut guard = self
            .context
            .lock()
            .map_err(|_| StagehandClientError::Poisoned)?;
        if guard.is_none() {
            let context = StagehandContext::new(self.adapter.clone(), self.dom_script.clone());
            *guard = Some(context);
        }
        Ok(())
    }

    /// Execute the provided closure with a mutable reference to the context.
    pub async fn with_context<F, T>(&self, f: F) -> Result<T, StagehandClientError>
    where
        F: FnOnce(&mut StagehandContext) -> Result<T, StagehandContextError>,
    {
        self.ensure_initialized().await?;
        let mut guard = self
            .context
            .lock()
            .map_err(|_| StagehandClientError::Poisoned)?;
        let ctx = guard
            .as_mut()
            .expect("context must be initialized after ensure_initialized");
        f(ctx).map_err(StagehandClientError::Context)
    }

    /// Register a page and ensure the Stagehand DOM script has been injected.
    pub async fn ensure_page_ready(
        &self,
        page_id: impl Into<String>,
        frame_id: Option<String>,
    ) -> Result<(), StagehandClientError> {
        let page_id = page_id.into();
        let frame_clone = frame_id.clone();
        self.with_context(|ctx| {
            ctx.register_page(page_id.clone(), frame_clone);
            ctx.ensure_dom_script(&page_id)?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{BrowserbasePlan, LocalPlan};
    use crate::config::{Environment, StagehandConfig};
    use crate::context::{StagehandAdapter, StagehandAdapterError};
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingRuntime {
        browserbase_calls: Mutex<usize>,
        local_calls: Mutex<usize>,
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
    }

    #[derive(Default)]
    struct RecordingAdapter {
        inject_calls: Mutex<Vec<String>>,
        debug_logs: Mutex<Vec<String>>,
        error_logs: Mutex<Vec<String>>,
    }

    impl StagehandAdapter for RecordingAdapter {
        fn inject_dom_script(
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

        fn notify_active_page(&self, _page_id: &String) {}
    }

    #[tokio::test]
    async fn ensure_page_ready_initializes_browserbase_runtime_once() {
        let mut config = StagehandConfig::default();
        config.browserbase_session_id = Some("existing".into());
        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let client =
            StagehandClient::new(config, runtime, adapter.clone(), "script").expect("client");

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
    async fn ensure_page_ready_initializes_local_runtime() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let client =
            StagehandClient::new(config, runtime, adapter.clone(), "script").expect("client");

        client.ensure_page_ready("page-1", None).await.unwrap();

        assert_eq!(*client.browser().runtime().local_calls.lock().unwrap(), 1);
    }

    #[test]
    fn log_helpers_delegate_to_adapter() {
        let config = StagehandConfig::default();
        let runtime = RecordingRuntime::default();
        let adapter = Arc::new(RecordingAdapter::default());
        let client =
            StagehandClient::new(config, runtime, adapter.clone(), "script").expect("client");

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
}
