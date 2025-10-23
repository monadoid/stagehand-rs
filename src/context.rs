//! Stagehand browser context scaffolding.
//!
//! This module mirrors a subset of the Python Stagehand context/page behaviour
//! while keeping the implementation intentionally lightweight. It focuses on
//! tracking pages, marking script injection state, and notifying an adapter
//! that will eventually bridge to a real browser runtime.

use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;
use std::sync::Arc;

use thiserror::Error;

pub type PageId = String;

/// Adapter responsible for interacting with the underlying browser/runtime.
///
/// The adapter will eventually wrap chromiumoxide or a direct CDP client. For
/// now, it only needs to support DOM script injection and basic logging so that
/// we can exercise the StagehandContext logic in isolation.
pub trait StagehandAdapter: Send + Sync {
    /// Inject the Stagehand DOM script into the specified page.
    fn inject_dom_script(
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
    active_page: Option<PageId>,
}

impl StagehandContext {
    /// Create a new context with the provided adapter and DOM script contents.
    pub fn new(adapter: Arc<dyn StagehandAdapter>, dom_script: impl Into<String>) -> Self {
        Self {
            adapter,
            dom_script: dom_script.into(),
            pages: HashMap::new(),
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
        match self.pages.entry(page_id.clone()) {
            Entry::Occupied(mut entry) => {
                if let Some(frame_id) = frame_id {
                    entry.get_mut().set_frame_id(Some(frame_id));
                }
                entry.into_mut()
            }
            Entry::Vacant(entry) => {
                let mut page = StagehandPage::new(entry.key().clone());
                if let Some(frame_id) = frame_id {
                    page.set_frame_id(Some(frame_id));
                }
                entry.insert(page)
            }
        }
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

    /// Update the stored frame identifier for a page.
    pub fn update_frame_id(
        &mut self,
        page_id: &PageId,
        frame_id: impl Into<String>,
    ) -> Result<(), StagehandContextError> {
        let page = self
            .pages
            .get_mut(page_id)
            .ok_or_else(|| StagehandContextError::PageNotFound(page_id.clone()))?;
        page.set_frame_id(Some(frame_id.into()));
        Ok(())
    }

    /// Ensure the custom DOM script has been injected for the specified page.
    ///
    /// Returns `true` if the script was injected during this call or `false` if
    /// it had already been injected previously.
    pub fn ensure_dom_script(&mut self, page_id: &PageId) -> Result<bool, StagehandContextError> {
        let page = self
            .pages
            .get_mut(page_id)
            .ok_or_else(|| StagehandContextError::PageNotFound(page_id.clone()))?;

        if page.dom_script_injected {
            return Ok(false);
        }

        self.adapter
            .inject_dom_script(page_id, &self.dom_script)
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

    fn set_frame_id(&mut self, frame_id: Option<String>) {
        self.frame_id = frame_id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct TestAdapter {
        inject_calls: Mutex<Vec<String>>,
        active_calls: Mutex<Vec<String>>,
        debug_logs: Mutex<Vec<String>>,
        error_logs: Mutex<Vec<String>>,
        fail_on: Option<String>,
    }

    impl StagehandAdapter for TestAdapter {
        fn inject_dom_script(
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

    #[test]
    fn ensure_dom_script_injects_once() {
        let (mut context, adapter) = new_context("script");
        context.register_page("page-1", None);

        let first = context
            .ensure_dom_script(&"page-1".to_string())
            .expect("first injection");
        let second = context
            .ensure_dom_script(&"page-1".to_string())
            .expect("second injection");

        assert!(first);
        assert!(!second);

        let calls = adapter.inject_calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], "page-1");
    }

    #[test]
    fn ensure_dom_script_propagates_failures() {
        let adapter = Arc::new(TestAdapter {
            fail_on: Some("page-1".to_string()),
            ..Default::default()
        });
        let mut context = StagehandContext::new(adapter, "script");
        context.register_page("page-1", None);

        let err = context
            .ensure_dom_script(&"page-1".to_string())
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
    fn ensure_dom_script_requires_registered_page() {
        let (mut context, _adapter) = new_context("script");
        let err = context
            .ensure_dom_script(&"missing".to_string())
            .expect_err("missing page");
        assert!(matches!(err, StagehandContextError::PageNotFound(_)));
    }
}
