//! Chromiumoxide-backed [`StagehandAdapter`] implementation.
//!
//! This adapter delegates DOM script injection to the shared
//! [`ChromiumoxideRuntime`], ensuring the real browser page receives the
//! embedded Stagehand helpers before higher-level automation runs.

use std::sync::Arc;

use async_trait::async_trait;
use chromiumoxide::page::Page;

use crate::browser::BrowserRuntimeError;
use crate::context::{PageId, StagehandAdapter, StagehandAdapterError};
use crate::runtime::ChromiumoxideRuntime;

fn map_runtime_error(err: BrowserRuntimeError) -> StagehandAdapterError {
    StagehandAdapterError::Message(err.to_string())
}

fn map_page_error(err: impl std::fmt::Display) -> StagehandAdapterError {
    StagehandAdapterError::Message(err.to_string())
}

/// Adapter that bridges Stagehand context operations to chromiumoxide.
#[derive(Clone)]
pub struct ChromiumoxideAdapter {
    runtime: Arc<ChromiumoxideRuntime>,
}

impl ChromiumoxideAdapter {
    /// Construct a new adapter from the shared chromiumoxide runtime.
    pub fn new(runtime: Arc<ChromiumoxideRuntime>) -> Self {
        Self { runtime }
    }

    async fn resolve_page(&self, page_id: &PageId) -> Result<Page, StagehandAdapterError> {
        let page = self
            .runtime
            .page(page_id)
            .await
            .map_err(map_runtime_error)?
            .ok_or_else(|| {
                StagehandAdapterError::Message(format!("page '{page_id}' not found in runtime"))
            })?;
        Ok(page)
    }
}

#[async_trait]
impl StagehandAdapter for ChromiumoxideAdapter {
    async fn inject_dom_script(
        &self,
        page_id: &PageId,
        script: &str,
    ) -> Result<(), StagehandAdapterError> {
        let page = self.resolve_page(page_id).await?;
        page.evaluate_on_new_document(script)
            .await
            .map_err(map_page_error)?;
        // Evaluate immediately so the helpers are available right away; this
        // mirrors the Python implementation.
        page.evaluate(script).await.map_err(map_page_error)?;
        Ok(())
    }

    fn log_debug(&self, _message: &str, _category: &'static str) {}

    fn log_error(&self, _message: &str, _category: &'static str) {}

    fn notify_active_page(&self, _page_id: &PageId) {}
}
