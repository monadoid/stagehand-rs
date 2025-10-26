//! Chromiumoxide-based browser runtime.
//!
//! Provides an implementation of [`BrowserRuntime`](crate::browser::BrowserRuntime)
//! backed by the `chromiumoxide` crate. The runtime currently supports local
//! launches and exposes helpers to open pages and fetch their content so higher
//! level components (e.g. `StagehandClient`) can drive real CDP interactions.

use std::collections::{HashMap, HashSet};
use std::convert::TryFrom;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chromiumoxide::{
    browser::{Browser, BrowserConfig},
    page::Page as ChromiumPage,
};
use futures_util::StreamExt;
use reqwest::Client as HttpClient;
use serde::Deserialize;
use tokio::{
    fs,
    sync::{Mutex, broadcast},
    task::JoinHandle,
};

use crate::browser::{
    BrowserRuntime, BrowserRuntimeError, BrowserbasePlan, BrowserbaseSessionCreateParams,
    BrowserbaseSessionStrategy, LocalLaunchStrategy, LocalPlan, RuntimeTargetEvent,
};

const DEFAULT_BROWSERBASE_API_URL: &str = "https://api.browserbase.com/v1";

pub struct ChromiumoxideRuntime {
    state: Arc<Mutex<Option<RuntimeState>>>,
    remote_config: Arc<Mutex<Option<BrowserbaseRuntimeState>>>,
    reconnect_lock: Arc<Mutex<()>>,
}

struct RuntimeState {
    browser: Arc<Browser>,
    _handler: JoinHandle<()>,
    pages: HashMap<String, ChromiumPage>,
    session_targets: HashMap<String, String>,
    remote: Option<BrowserbaseRuntimeState>,
    #[allow(dead_code)]
    temp_user_data_dir: Option<PathBuf>,
    #[allow(dead_code)]
    target_listener: Option<JoinHandle<()>>,
    target_events: broadcast::Sender<RuntimeTargetEvent>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
struct BrowserbaseRuntimeState {
    #[allow(dead_code)]
    session_id: String,
    connect_url: String,
    #[allow(dead_code)]
    api_key: String,
    #[allow(dead_code)]
    project_id: Option<String>,
}

#[derive(Debug, Clone)]
struct BrowserbaseSessionInfo {
    id: String,
    status: Option<String>,
    connect_url: String,
}

impl BrowserbaseSessionInfo {
    fn is_running(&self) -> bool {
        self.status
            .as_deref()
            .map(|status| status.eq_ignore_ascii_case("running"))
            .unwrap_or(false)
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserbaseSessionResponse {
    id: String,
    status: Option<String>,
    #[serde(alias = "connect_url")]
    connect_url: Option<String>,
}

impl TryFrom<BrowserbaseSessionResponse> for BrowserbaseSessionInfo {
    type Error = BrowserRuntimeError;

    fn try_from(value: BrowserbaseSessionResponse) -> Result<Self, Self::Error> {
        let connect_url = value.connect_url.ok_or_else(|| {
            BrowserRuntimeError::Message(format!(
                "Browserbase session {} did not include a connect URL",
                value.id
            ))
        })?;

        Ok(Self {
            id: value.id,
            status: value.status,
            connect_url,
        })
    }
}

struct BrowserbaseApiClient {
    client: HttpClient,
    api_key: String,
    base_url: String,
}

impl BrowserbaseApiClient {
    fn new(api_key: String) -> Result<Self, BrowserRuntimeError> {
        let base_url = std::env::var("BROWSERBASE_API_URL")
            .unwrap_or_else(|_| DEFAULT_BROWSERBASE_API_URL.to_string());
        let client = HttpClient::builder().build().map_err(|err| {
            BrowserRuntimeError::Message(format!(
                "failed to construct Browserbase HTTP client: {err}"
            ))
        })?;

        Ok(Self {
            client,
            api_key,
            base_url,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        format!("{base}/{path}")
    }

    async fn retrieve_session(
        &self,
        session_id: &str,
    ) -> Result<BrowserbaseSessionInfo, BrowserRuntimeError> {
        let url = self.endpoint(&format!("sessions/{session_id}"));
        let response = self
            .client
            .get(url)
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map_err(|err| {
                BrowserRuntimeError::Message(format!("Browserbase session retrieval failed: {err}"))
            })?;

        self.handle_session_response(response, &format!("retrieve session {session_id}"))
            .await
    }

    async fn create_session(
        &self,
        params: &BrowserbaseSessionCreateParams,
    ) -> Result<BrowserbaseSessionInfo, BrowserRuntimeError> {
        let url = self.endpoint("sessions");
        let response = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(params)
            .send()
            .await
            .map_err(|err| {
                BrowserRuntimeError::Message(format!("Browserbase session creation failed: {err}"))
            })?;

        self.handle_session_response(response, "create session")
            .await
    }

    async fn handle_session_response(
        &self,
        response: reqwest::Response,
        context: &str,
    ) -> Result<BrowserbaseSessionInfo, BrowserRuntimeError> {
        if response.status().is_success() {
            let parsed: BrowserbaseSessionResponse = response.json().await.map_err(|err| {
                BrowserRuntimeError::Message(format!(
                    "failed to parse Browserbase response while attempting to {context}: {err}"
                ))
            })?;
            BrowserbaseSessionInfo::try_from(parsed)
        } else {
            let status = response.status();
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<unavailable>".to_string());
            Err(BrowserRuntimeError::Message(format!(
                "Browserbase API call to {context} failed ({status}): {body}"
            )))
        }
    }
}

impl ChromiumoxideRuntime {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(None)),
            remote_config: Arc::new(Mutex::new(None)),
            reconnect_lock: Arc::new(Mutex::new(())),
        }
    }
    async fn store_remote_config(&self, remote: Option<BrowserbaseRuntimeState>) {
        let mut guard = self.remote_config.lock().await;
        *guard = remote;
    }

    async fn current_browser(&self) -> Option<Arc<Browser>> {
        let guard = self.state.lock().await;
        guard.as_ref().map(|state| state.browser.clone())
    }

    async fn ensure_browser_alive(&self) -> Result<(), BrowserRuntimeError> {
        let (browser, remote_snapshot) = {
            let guard = self.state.lock().await;
            match guard.as_ref() {
                Some(state) => (Some(state.browser.clone()), state.remote.clone()),
                None => (None, None),
            }
        };

        if let Some(browser) = browser {
            match browser.version().await.map_err(map_chromiumoxide_error) {
                Ok(_) => Ok(()),
                Err(err) => {
                    let mut remote = remote_snapshot;
                    if remote.is_none() {
                        remote = self.remote_config.lock().await.clone();
                    }
                    if let Some(remote_state) = remote {
                        self.reconnect_remote(remote_state).await
                    } else {
                        Err(err)
                    }
                }
            }
        } else {
            let remote = self.remote_config.lock().await.clone();
            if let Some(remote_state) = remote {
                self.reconnect_remote(remote_state).await
            } else {
                Err(BrowserRuntimeError::NotInitialized)
            }
        }
    }

    async fn reconnect_remote(
        &self,
        remote: BrowserbaseRuntimeState,
    ) -> Result<(), BrowserRuntimeError> {
        let _lock = self.reconnect_lock.lock().await;

        if let Some(browser) = self.current_browser().await {
            if browser
                .version()
                .await
                .map_err(map_chromiumoxide_error)
                .is_ok()
            {
                return Ok(());
            }
        }

        attach_to_cdp(&self.state, &remote.connect_url).await?;
        self.set_remote_state(remote.clone()).await?;
        self.populate_initial_pages().await?;
        self.ensure_target_monitor().await?;
        Ok(())
    }

    pub async fn page(&self, page_id: &str) -> Result<Option<ChromiumPage>, BrowserRuntimeError> {
        self.ensure_browser_alive().await?;
        let guard = self.state.lock().await;
        let state = guard.as_ref().ok_or(BrowserRuntimeError::NotInitialized)?;
        Ok(state.pages.get(page_id).cloned())
    }

    async fn populate_initial_pages(&self) -> Result<(), BrowserRuntimeError> {
        let browser = {
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or(BrowserRuntimeError::NotInitialized)?;
            state.browser.clone()
        };

        let pages = browser.pages().await.map_err(map_chromiumoxide_error)?;

        if pages.is_empty() {
            return Ok(());
        }

        let mut guard = self.state.lock().await;
        if let Some(state) = guard.as_mut() {
            for page in pages {
                let id = page.target_id().as_ref().to_string();
                state.pages.entry(id).or_insert(page);
            }
        }

        Ok(())
    }

    async fn ensure_target_monitor(&self) -> Result<(), BrowserRuntimeError> {
        let (browser, already_running, event_sender) = {
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or(BrowserRuntimeError::NotInitialized)?;
            (
                state.browser.clone(),
                state.target_listener.is_some(),
                state.target_events.clone(),
            )
        };

        if already_running {
            return Ok(());
        }

        let state_arc = Arc::clone(&self.state);

        let mut attached_stream = browser
            .event_listener::<chromiumoxide::cdp::browser_protocol::target::EventAttachedToTarget>()
            .await
            .map_err(map_chromiumoxide_error)?;
        let mut detached_stream = browser
            .event_listener::<chromiumoxide::cdp::browser_protocol::target::EventDetachedFromTarget>()
            .await
            .map_err(map_chromiumoxide_error)?;

        let browser_for_pages = browser.clone();
        let event_sender = event_sender.clone();
        let monitor_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    maybe_event = attached_stream.next() => {
                        if let Some(event) = maybe_event {
                            let event = (*event).clone();
                            if event.target_info.r#type == "page" {
                                let target_id = event.target_info.target_id.clone();
                                let session_id = event.session_id.as_ref().to_string();
                                match browser_for_pages.get_page(target_id.clone()).await {
                                    Ok(page) => {
                                        let mut guard = state_arc.lock().await;
                                        if let Some(state) = guard.as_mut() {
                                            let target_str = target_id.as_ref().to_string();
                                            state.pages.insert(target_str.clone(), page);
                                            state.session_targets.insert(session_id, target_str.clone());
                                            let _ = event_sender.send(RuntimeTargetEvent::Attached { target_id: target_str });
                                        }
                                    }
                                    Err(err) => {
                                        eprintln!("chromiumoxide target listener failed to fetch page: {err}");
                                    }
                                }
                            }
                        } else {
                            break;
                        }
                    }
                    maybe_event = detached_stream.next() => {
                        if let Some(event) = maybe_event {
                            let session_id = event.session_id.as_ref().to_string();
                            let mut guard = state_arc.lock().await;
                            if let Some(state) = guard.as_mut() {
                                if let Some(target_id) = state.session_targets.remove(&session_id) {
                                    state.pages.remove(&target_id);
                                    let _ = event_sender.send(RuntimeTargetEvent::Detached { target_id });
                                } else {
                                    match browser_for_pages.pages().await {
                                        Ok(pages) => {
                                            let existing: HashSet<String> = pages
                                                .into_iter()
                                                .map(|page| page.target_id().as_ref().to_string())
                                                .collect();
                                            let stale: Vec<String> = state
                                                .pages
                                                .keys()
                                                .filter(|id| !existing.contains(*id))
                                                .cloned()
                                                .collect();
                                            for target_id in stale {
                                                state.pages.remove(&target_id);
                                                let _ = event_sender
                                                    .send(RuntimeTargetEvent::Detached { target_id });
                                            }
                                        }
                                        Err(err) => {
                                            eprintln!(
                                                "chromiumoxide target listener failed to refresh pages after detach: {err}"
                                            );
                                        }
                                    }
                                }
                            }
                        } else {
                            break;
                        }
                    }
                }
            }
        });

        let mut guard = self.state.lock().await;
        if let Some(state) = guard.as_mut() {
            state.target_listener = Some(monitor_handle);
        }

        Ok(())
    }

    async fn set_remote_state(
        &self,
        remote: BrowserbaseRuntimeState,
    ) -> Result<(), BrowserRuntimeError> {
        {
            let mut guard = self.state.lock().await;
            match guard.as_mut() {
                Some(state) => {
                    state.remote = Some(remote.clone());
                }
                None => {
                    return Err(BrowserRuntimeError::Message(
                        "browser runtime not initialised after connection".into(),
                    ));
                }
            }
        }

        self.store_remote_config(Some(remote)).await;
        Ok(())
    }

    pub async fn shutdown(&self) -> Result<(), BrowserRuntimeError> {
        let state = {
            let mut guard = self.state.lock().await;
            guard.take()
        };

        if let Some(state) = state {
            let temp_dir = cleanup_state(state);
            if let Some(path) = temp_dir {
                if let Err(err) = fs::remove_dir_all(&path).await {
                    eprintln!("failed to remove temporary user data dir {:?}: {err}", path);
                }
            }
        }

        self.store_remote_config(None).await;
        Ok(())
    }
}

impl Default for ChromiumoxideRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl BrowserRuntime for ChromiumoxideRuntime {
    async fn connect_browserbase(&self, plan: &BrowserbasePlan) -> Result<(), BrowserRuntimeError> {
        {
            let guard = self.state.lock().await;
            if guard.is_some() {
                return Ok(());
            }
        }

        let api_key = plan.api_key.clone().ok_or_else(|| {
            BrowserRuntimeError::Message(
                "Browserbase API key is required to connect to Browserbase".into(),
            )
        })?;

        let client = BrowserbaseApiClient::new(api_key.clone())?;

        let session = match &plan.strategy {
            BrowserbaseSessionStrategy::UseExisting { session_id } => {
                let info = client.retrieve_session(session_id).await?;
                if !info.is_running() {
                    let status = info.status.clone().unwrap_or_else(|| "UNKNOWN".to_string());
                    return Err(BrowserRuntimeError::Message(format!(
                        "Browserbase session {session_id} is not running (status: {status})"
                    )));
                }
                info
            }
            BrowserbaseSessionStrategy::CreateNew { params } => {
                client.create_session(params).await?
            }
        };

        let remote_state = BrowserbaseRuntimeState {
            session_id: session.id.clone(),
            connect_url: session.connect_url.clone(),
            api_key,
            project_id: plan.project_id.clone(),
        };

        attach_to_cdp(&self.state, &remote_state.connect_url).await?;
        self.set_remote_state(remote_state).await?;
        self.populate_initial_pages().await?;
        self.ensure_target_monitor().await?;

        Ok(())
    }

    async fn launch_local(&self, plan: &LocalPlan) -> Result<(), BrowserRuntimeError> {
        if self.state.lock().await.is_some() {
            return Ok(());
        }

        match &plan.strategy {
            LocalLaunchStrategy::AttachCdp { url, .. } => {
                attach_to_cdp(&self.state, url).await?;
            }
            LocalLaunchStrategy::LaunchPersistent { .. } => {
                launch_persistent(&self.state, plan).await?;
            }
        }

        self.populate_initial_pages().await?;
        self.ensure_target_monitor().await?;
        self.store_remote_config(None).await;

        Ok(())
    }

    async fn new_page(&self, url: &str) -> Result<String, BrowserRuntimeError> {
        self.ensure_browser_alive().await?;
        let browser = {
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or(BrowserRuntimeError::NotInitialized)?;
            state.browser.clone()
        };

        let page = browser
            .new_page(url)
            .await
            .map_err(map_chromiumoxide_error)?;
        let page_id = page.target_id().as_ref().to_string();

        let mut guard = self.state.lock().await;
        if let Some(state) = guard.as_mut() {
            state.pages.insert(page_id.clone(), page);
            let _ = state.target_events.send(RuntimeTargetEvent::Attached {
                target_id: page_id.clone(),
            });
        }

        Ok(page_id)
    }

    async fn page_content(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
        self.ensure_browser_alive().await?;
        let page = {
            let guard = self.state.lock().await;
            let state = guard.as_ref().ok_or(BrowserRuntimeError::NotInitialized)?;
            state.pages.get(page_id).cloned()
        };

        if let Some(page) = page {
            let content = page.content().await.map_err(map_chromiumoxide_error)?;
            Ok(Some(content))
        } else {
            Ok(None)
        }
    }

    async fn list_pages(&self) -> Result<Vec<String>, BrowserRuntimeError> {
        self.ensure_browser_alive().await?;
        let guard = self.state.lock().await;
        let state = guard.as_ref().ok_or(BrowserRuntimeError::NotInitialized)?;
        Ok(state.pages.keys().cloned().collect())
    }

    async fn target_event_stream(
        &self,
    ) -> Result<broadcast::Receiver<RuntimeTargetEvent>, BrowserRuntimeError> {
        self.ensure_browser_alive().await?;
        self.ensure_target_monitor().await?;
        let guard = self.state.lock().await;
        let state = guard.as_ref().ok_or(BrowserRuntimeError::NotInitialized)?;
        Ok(state.target_events.subscribe())
    }

    async fn main_frame_id(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
        let page = self.page(page_id).await?;
        if let Some(page) = page {
            let frame = page.mainframe().await.map_err(map_chromiumoxide_error)?;
            Ok(frame.map(|id| id.as_ref().to_string()))
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl BrowserRuntime for Arc<ChromiumoxideRuntime> {
    async fn connect_browserbase(&self, plan: &BrowserbasePlan) -> Result<(), BrowserRuntimeError> {
        (**self).connect_browserbase(plan).await
    }

    async fn launch_local(&self, plan: &LocalPlan) -> Result<(), BrowserRuntimeError> {
        (**self).launch_local(plan).await
    }

    async fn new_page(&self, url: &str) -> Result<String, BrowserRuntimeError> {
        (**self).new_page(url).await
    }

    async fn page_content(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
        (**self).page_content(page_id).await
    }

    async fn list_pages(&self) -> Result<Vec<String>, BrowserRuntimeError> {
        (**self).list_pages().await
    }

    async fn target_event_stream(
        &self,
    ) -> Result<broadcast::Receiver<RuntimeTargetEvent>, BrowserRuntimeError> {
        (**self).target_event_stream().await
    }

    async fn main_frame_id(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
        (**self).main_frame_id(page_id).await
    }
}

fn build_config(plan: &LocalPlan) -> Result<BrowserConfig, BrowserRuntimeError> {
    let launch = &plan.launch_options;

    let viewport = chromiumoxide::handler::viewport::Viewport {
        width: launch.viewport.width,
        height: launch.viewport.height,
        device_scale_factor: None,
        emulating_mobile: false,
        is_landscape: launch.viewport.width >= launch.viewport.height,
        has_touch: false,
    };

    let mut builder = BrowserConfig::builder();

    if let Some(path) = &plan.chrome_executable {
        builder = builder.chrome_executable(path);
    }

    let builder = builder.viewport(viewport).args(launch.args.clone());

    let builder = if launch.headless {
        builder
    } else {
        builder.with_head()
    };

    let builder = if !launch.ignore_https_errors {
        builder.respect_https_errors()
    } else {
        builder
    };

    let builder = match &plan.strategy {
        LocalLaunchStrategy::AttachCdp { .. } => builder,
        LocalLaunchStrategy::LaunchPersistent { user_data_dir } => match user_data_dir {
            Some(dir) => builder.user_data_dir(dir),
            None => builder,
        },
    };

    let builder = if !launch.locale.is_empty() {
        builder.arg(format!("--lang={}", launch.locale))
    } else {
        builder
    };

    let builder = if !launch.timezone_id.is_empty() {
        builder.arg(format!("--timezone={}", launch.timezone_id))
    } else {
        builder
    };

    builder.build().map_err(BrowserRuntimeError::Message)
}

fn map_chromiumoxide_error<E: std::fmt::Display>(err: E) -> BrowserRuntimeError {
    BrowserRuntimeError::Message(err.to_string())
}

async fn attach_to_cdp(
    state: &Mutex<Option<RuntimeState>>,
    url: &str,
) -> Result<(), BrowserRuntimeError> {
    let (browser, handler) = Browser::connect(url)
        .await
        .map_err(map_chromiumoxide_error)?;

    let browser = Arc::new(browser);
    let join = spawn_handler(handler);
    let (event_tx, _) = broadcast::channel(32);

    let new_state = RuntimeState {
        browser,
        _handler: join,
        pages: HashMap::new(),
        session_targets: HashMap::new(),
        remote: None,
        temp_user_data_dir: None,
        target_listener: None,
        target_events: event_tx,
    };

    let old_state = {
        let mut guard = state.lock().await;
        let previous = guard.take();
        *guard = Some(new_state);
        previous
    };

    if let Some(state) = old_state {
        let temp_dir = cleanup_state(state);
        if let Some(path) = temp_dir {
            if let Err(err) = fs::remove_dir_all(&path).await {
                eprintln!("failed to remove temporary user data dir {:?}: {err}", path);
            }
        }
    }

    Ok(())
}

async fn launch_persistent(
    state: &Mutex<Option<RuntimeState>>,
    plan: &LocalPlan,
) -> Result<(), BrowserRuntimeError> {
    let config = build_config(plan)?;

    if let Some(parent) = plan.downloads_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|err| BrowserRuntimeError::Message(err.to_string()))?;
    }

    let (browser, handler) = Browser::launch(config)
        .await
        .map_err(map_chromiumoxide_error)?;

    let browser = Arc::new(browser);
    let join = spawn_handler(handler);
    let (event_tx, _) = broadcast::channel(32);

    let new_state = RuntimeState {
        browser,
        _handler: join,
        pages: HashMap::new(),
        session_targets: HashMap::new(),
        remote: None,
        temp_user_data_dir: None,
        target_listener: None,
        target_events: event_tx,
    };

    let old_state = {
        let mut guard = state.lock().await;
        let previous = guard.take();
        *guard = Some(new_state);
        previous
    };

    if let Some(state) = old_state {
        let temp_dir = cleanup_state(state);
        if let Some(path) = temp_dir {
            if let Err(err) = fs::remove_dir_all(&path).await {
                eprintln!("failed to remove temporary user data dir {:?}: {err}", path);
            }
        }
    }

    Ok(())
}

fn spawn_handler(mut handler: chromiumoxide::handler::Handler) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(result) = handler.next().await {
            if let Err(err) = result {
                eprintln!("chromiumoxide handler error: {err}");
            }
        }
    })
}

fn cleanup_state(mut state: RuntimeState) -> Option<PathBuf> {
    state._handler.abort();
    if let Some(listener) = state.target_listener.take() {
        listener.abort();
    }
    state.pages.clear();
    state.session_targets.clear();
    state.remote = None;
    state.temp_user_data_dir.take()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::{BrowserbasePlan, BrowserbaseSessionStrategy};

    #[tokio::test]
    async fn connect_browserbase_requires_api_key() {
        let runtime = ChromiumoxideRuntime::new();

        let plan = BrowserbasePlan {
            api_key: None,
            project_id: None,
            strategy: BrowserbaseSessionStrategy::CreateNew {
                params: BrowserbaseSessionCreateParams::default(),
            },
        };

        let err = runtime
            .connect_browserbase(&plan)
            .await
            .expect_err("should fail without API key");
        assert!(
            err.to_string()
                .contains("Browserbase API key is required to connect to Browserbase")
        );
    }

    #[test]
    fn browserbase_session_info_running_detection() {
        let mut info = BrowserbaseSessionInfo {
            id: "sess-123".into(),
            status: Some("RUNNING".into()),
            connect_url: "ws://example".into(),
        };
        assert!(info.is_running());
        info.status = Some("stopped".into());
        assert!(!info.is_running());
        info.status = None;
        assert!(!info.is_running());
    }

    #[test]
    fn browserbase_session_info_requires_connect_url() {
        let response = BrowserbaseSessionResponse {
            id: "sess-456".into(),
            status: Some("RUNNING".into()),
            connect_url: None,
        };
        let err = BrowserbaseSessionInfo::try_from(response).expect_err("should error");
        assert!(err.to_string().contains("did not include a connect URL"));
    }
}
