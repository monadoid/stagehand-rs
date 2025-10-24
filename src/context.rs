//! Stagehand browser context scaffolding.
//!
//! This module mirrors a subset of the Python Stagehand context/page behaviour
//! while keeping the implementation intentionally lightweight. It focuses on
//! tracking pages, marking script injection state, and notifying an adapter
//! that will eventually bridge to a real browser runtime.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;

pub type PageId = String;

/// Adapter responsible for interacting with the underlying browser/runtime.
///
/// The adapter will eventually wrap chromiumoxide or a direct CDP client. For
/// now, it only needs to support DOM script injection and basic logging so that
/// we can exercise the StagehandContext logic in isolation.
#[async_trait]
pub trait StagehandAdapter: Send + Sync {
    /// Inject the Stagehand DOM script into the specified page.
    async fn inject_dom_script(
        &self,
        page_id: &PageId,
        script: &str,
    ) -> Result<(), StagehandAdapterError>;

    /// Emit a structured debug message.
    fn log_debug(&self, message: &str, category: &'static str);

    /// Emit a structured error message.
    fn log_error(&self, message: &str, category: &'static str);

    /// Notify the adapter that the active page changed.
    fn notify_active_page(&self, page_id: &PageId);
}

/// High-level errors surfaced by [`StagehandContext`].
#[derive(Debug, Error)]
pub enum StagehandContextError {
    #[error("page '{0}' is not registered")]
    PageNotFound(PageId),
    #[error("failed to inject DOM script for '{page_id}'")]
    ScriptInjection {
        page_id: PageId,
        #[source]
        source: StagehandAdapterError,
    },
}

/// Errors returned by [`StagehandAdapter`] operations.
#[derive(Debug, Error)]
pub enum StagehandAdapterError {
    #[error("{0}")]
    Message(String),
}

/// Wrapper around the set of known browser pages/tabs in the Stagehand session.
pub struct StagehandContext {
    adapter: Arc<dyn StagehandAdapter>,
    dom_script: String,
    pages: HashMap<PageId, StagehandPage>,
    frame_index: HashMap<String, PageId>,
    active_page: Option<PageId>,
}

impl StagehandContext {
    /// Create a new context with the provided adapter and DOM script contents.
    pub fn new(adapter: Arc<dyn StagehandAdapter>, dom_script: impl Into<String>) -> Self {
        Self {
            adapter,
            dom_script: dom_script.into(),
            pages: HashMap::new(),
            frame_index: HashMap::new(),
            active_page: None,
        }
    }

    /// Register a new page with an optional initial frame identifier.
    ///
    /// Registration is idempotent—re-registering the same page only updates its
    /// frame identifier.
    pub fn register_page(
        &mut self,
        page_id: impl Into<PageId>,
        frame_id: Option<String>,
    ) -> &StagehandPage {
        let page_id = page_id.into();
        let frame_to_apply = frame_id;
        let mut previous_frame: Option<String> = None;

        let page_ref = self
            .pages
            .entry(page_id.clone())
            .and_modify(|page| {
                if let Some(ref frame) = frame_to_apply {
                    previous_frame = page.replace_frame_id(Some(frame.clone()));
                }
            })
            .or_insert_with(|| {
                let mut page = StagehandPage::new(page_id.clone());
                if let Some(ref frame) = frame_to_apply {
                    previous_frame = page.replace_frame_id(Some(frame.clone()));
                }
                page
            });

        if let Some(ref new_frame) = frame_to_apply {
            if let Some(old) = previous_frame {
                self.frame_index.remove(&old);
            }
            self.frame_index
                .insert(new_frame.clone(), page_ref.id().clone());
        }

        page_ref
    }

    /// Retrieve a page by id.
    pub fn page(&self, page_id: &PageId) -> Option<&StagehandPage> {
        self.pages.get(page_id)
    }

    /// Update the active page and inform the adapter.
    pub fn set_active_page(&mut self, page_id: &PageId) -> Result<(), StagehandContextError> {
        if !self.pages.contains_key(page_id) {
            return Err(StagehandContextError::PageNotFound(page_id.clone()));
        }
        self.active_page = Some(page_id.clone());
        self.adapter.notify_active_page(page_id);
        self.adapter
            .log_debug(&format!("Set active page to {}", page_id), "context");
        Ok(())
    }

    /// Return the currently active page if one has been selected.
    pub fn active_page(&self) -> Option<&StagehandPage> {
        self.active_page
            .as_ref()
            .and_then(|page_id| self.pages.get(page_id))
    }

    /// Register a frame identifier for a page and update the index accordingly.
    pub fn register_frame_id(
        &mut self,
        page_id: &PageId,
        frame_id: impl Into<String>,
    ) -> Result<(), StagehandContextError> {
        let frame_id = frame_id.into();
        let page = self
            .pages
            .get_mut(page_id)
            .ok_or_else(|| StagehandContextError::PageNotFound(page_id.clone()))?;
        if let Some(old) = page.replace_frame_id(Some(frame_id.clone())) {
            self.frame_index.remove(&old);
        }
        self.frame_index.insert(frame_id.clone(), page.id().clone());
        Ok(())
    }

    /// Remove a frame identifier mapping. Returns `true` if something was removed.
    pub fn unregister_frame_id(&mut self, frame_id: &str) -> bool {
        if let Some(page_id) = self.frame_index.remove(frame_id) {
            if let Some(page) = self.pages.get_mut(&page_id) {
                if page.frame_id().map(|id| id == frame_id).unwrap_or(false) {
                    page.replace_frame_id(None);
                }
            }
            true
        } else {
            false
        }
    }

    /// Look up a page by its registered frame identifier.
    pub fn page_by_frame_id(&self, frame_id: &str) -> Option<&StagehandPage> {
        let page_id = self.frame_index.get(frame_id)?;
        self.pages.get(page_id)
    }

    /// Update the stored frame identifier for a page.
    pub fn update_frame_id(
        &mut self,
        page_id: &PageId,
        frame_id: impl Into<String>,
    ) -> Result<(), StagehandContextError> {
        self.register_frame_id(page_id, frame_id)
    }

    /// Remove a page and associated frame mappings. Returns `true` if a page was removed.
    pub fn remove_page(&mut self, page_id: &PageId) -> bool {
        if let Some(page) = self.pages.remove(page_id) {
            if let Some(frame_id) = page.frame_id {
                self.frame_index.remove(&frame_id);
            }
            if self.active_page.as_ref() == Some(page_id) {
                self.active_page = None;
            }
            true
        } else {
            false
        }
    }

    /// Retrieve the frame identifier associated with a page, if known.
    pub fn frame_id_for(&self, page_id: &PageId) -> Option<String> {
        self.pages
            .get(page_id)
            .and_then(|page| page.frame_id())
            .map(|id| id.to_string())
    }

    /// Ensure the custom DOM script has been injected for the specified page.
    ///
    /// Returns `true` if the script was injected during this call or `false` if
    /// it had already been injected previously.
    pub async fn ensure_dom_script(
        &mut self,
        page_id: &PageId,
    ) -> Result<bool, StagehandContextError> {
        let page = self
            .pages
            .get_mut(page_id)
            .ok_or_else(|| StagehandContextError::PageNotFound(page_id.clone()))?;

        if page.dom_script_injected {
            return Ok(false);
        }

        self.adapter
            .inject_dom_script(page_id, &self.dom_script)
            .await
            .map_err(|source| StagehandContextError::ScriptInjection {
                page_id: page_id.clone(),
                source,
            })?;
        page.dom_script_injected = true;
        self.adapter
            .log_debug(&format!("Injected DOM script into {}", page_id), "context");
        Ok(true)
    }
}

impl fmt::Debug for StagehandContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagehandContext")
            .field("page_count", &self.pages.len())
            .field("frame_count", &self.frame_index.len())
            .field("active_page", &self.active_page)
            .finish()
    }
}

/// Lightweight wrapper around a single browser page/tab.
#[derive(Debug, Clone)]
pub struct StagehandPage {
    id: PageId,
    frame_id: Option<String>,
    dom_script_injected: bool,
}

impl StagehandPage {
    fn new(id: PageId) -> Self {
        Self {
            id,
            frame_id: None,
            dom_script_injected: false,
        }
    }

    /// Unique identifier for this page within the session.
    pub fn id(&self) -> &PageId {
        &self.id
    }

    /// Current root frame identifier, if known.
    pub fn frame_id(&self) -> Option<&str> {
        self.frame_id.as_deref()
    }

    /// Whether the Stagehand DOM script has been injected already.
    pub fn dom_script_injected(&self) -> bool {
        self.dom_script_injected
    }

    fn replace_frame_id(&mut self, frame_id: Option<String>) -> Option<String> {
        std::mem::replace(&mut self.frame_id, frame_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TestAdapter {
        inject_calls: Mutex<Vec<String>>,
        active_calls: Mutex<Vec<String>>,
        debug_logs: Mutex<Vec<String>>,
        error_logs: Mutex<Vec<String>>,
        fail_on: Option<String>,
    }

    #[async_trait]
    impl StagehandAdapter for TestAdapter {
        async fn inject_dom_script(
            &self,
            page_id: &PageId,
            _script: &str,
        ) -> Result<(), StagehandAdapterError> {
            if self.fail_on.as_deref() == Some(page_id) {
                return Err(StagehandAdapterError::Message(format!(
                    "failed for {}",
                    page_id
                )));
            }
            self.inject_calls.lock().unwrap().push(page_id.clone());
            Ok(())
        }

        fn log_debug(&self, message: &str, _category: &'static str) {
            self.debug_logs.lock().unwrap().push(message.to_string());
        }

        fn log_error(&self, message: &str, _category: &'static str) {
            self.error_logs.lock().unwrap().push(message.to_string());
        }

        fn notify_active_page(&self, page_id: &PageId) {
            self.active_calls.lock().unwrap().push(page_id.clone());
        }
    }

    fn new_context(dom_script: &str) -> (StagehandContext, Arc<TestAdapter>) {
        let adapter = Arc::new(TestAdapter::default());
        let context = StagehandContext::new(adapter.clone(), dom_script.to_string());
        (context, adapter)
    }

    #[test]
    fn register_page_is_idempotent() {
        let (mut context, _adapter) = new_context("script");
        context.register_page("page-1", Some("frame-a".into()));
        context.register_page("page-1", Some("frame-b".into()));

        let page = context.page(&"page-1".to_string()).unwrap();
        assert_eq!(page.id(), "page-1");
        assert_eq!(page.frame_id(), Some("frame-b"));
    }

    #[tokio::test]
    async fn ensure_dom_script_injects_once() {
        let (mut context, adapter) = new_context("script");
        context.register_page("page-1", None);

        let first = context
            .ensure_dom_script(&"page-1".to_string())
            .await
            .expect("first injection");
        let second = context
            .ensure_dom_script(&"page-1".to_string())
            .await
            .expect("second injection");

        assert!(first);
        assert!(!second);

        let calls = adapter.inject_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "page-1");
    }

    #[tokio::test]
    async fn ensure_dom_script_propagates_failures() {
        let adapter = Arc::new(TestAdapter {
            fail_on: Some("page-1".to_string()),
            ..Default::default()
        });
        let mut context = StagehandContext::new(adapter, "script");
        context.register_page("page-1", None);

        let err = context
            .ensure_dom_script(&"page-1".to_string())
            .await
            .expect_err("should fail");
        match err {
            StagehandContextError::ScriptInjection { page_id, .. } => {
                assert_eq!(page_id, "page-1");
            }
            _ => panic!("unexpected error variant"),
        }
    }

    #[test]
    fn set_active_page_notifies_adapter() {
        let (mut context, adapter) = new_context("script");
        context.register_page("page-1", None);
        context
            .set_active_page(&"page-1".to_string())
            .expect("set active");

        let active = adapter.active_calls.lock().unwrap();
        assert_eq!(active.as_slice(), &["page-1".to_string()]);
        assert_eq!(context.active_page().unwrap().id(), "page-1");
    }

    #[test]
    fn update_frame_id_updates_page() {
        let (mut context, _adapter) = new_context("script");
        context.register_page("page-1", None);
        context
            .update_frame_id(&"page-1".to_string(), "frame-xyz")
            .expect("update frame");

        let page = context.page(&"page-1".to_string()).unwrap();
        assert_eq!(page.frame_id(), Some("frame-xyz"));
    }

    #[test]
    fn register_page_tracks_frame_index() {
        let (mut context, _adapter) = new_context("script");
        let page_id = "page-1".to_string();

        context.register_page(page_id.clone(), Some("frame-1".into()));
        assert_eq!(
            context
                .page_by_frame_id("frame-1")
                .map(|page| page.id().to_string()),
            Some(page_id.clone())
        );

        context.register_page(page_id.clone(), Some("frame-2".into()));
        assert!(context.page_by_frame_id("frame-1").is_none());
        assert_eq!(
            context
                .page_by_frame_id("frame-2")
                .map(|page| page.id().to_string()),
            Some(page_id.clone())
        );
    }

    #[test]
    fn register_and_update_frame_id_refreshes_mapping() {
        let (mut context, _adapter) = new_context("script");
        let page_id = "page-1".to_string();

        context.register_page(page_id.clone(), None);
        context
            .register_frame_id(&page_id, "frame-1")
            .expect("register frame");
        assert_eq!(
            context
                .page_by_frame_id("frame-1")
                .map(|page| page.id().to_string()),
            Some(page_id.clone())
        );

        context
            .update_frame_id(&page_id, "frame-2")
            .expect("update frame");
        assert!(context.page_by_frame_id("frame-1").is_none());
        assert_eq!(
            context
                .page_by_frame_id("frame-2")
                .map(|page| page.id().to_string()),
            Some(page_id.clone())
        );
    }

    #[test]
    fn unregister_frame_id_clears_mapping_and_page_state() {
        let (mut context, _adapter) = new_context("script");
        let page_id = "page-1".to_string();

        context.register_page(page_id.clone(), Some("frame-3".into()));
        assert!(context.unregister_frame_id("frame-3"));
        assert!(context.page_by_frame_id("frame-3").is_none());
        let page = context.page(&page_id).expect("page present");
        assert!(page.frame_id().is_none());

        assert!(!context.unregister_frame_id("frame-3"));
    }

    #[test]
    fn remove_page_drops_state_and_active_marker() {
        let (mut context, _adapter) = new_context("script");
        let page_id = "page-1".to_string();

        context.register_page(page_id.clone(), Some("frame-9".into()));
        context
            .set_active_page(&page_id)
            .expect("set active page succeeds");

        assert!(context.remove_page(&page_id));
        assert!(context.page(&page_id).is_none());
        assert!(context.page_by_frame_id("frame-9").is_none());
        assert!(context.active_page().is_none());
        assert!(!context.remove_page(&page_id));
    }

    #[tokio::test]
    async fn ensure_dom_script_requires_registered_page() {
        let (mut context, _adapter) = new_context("script");
        let err = context
            .ensure_dom_script(&"missing".to_string())
            .await
            .expect_err("missing page");
        assert!(matches!(err, StagehandContextError::PageNotFound(_)));
    }
}
