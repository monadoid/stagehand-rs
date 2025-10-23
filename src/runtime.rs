//! Chromiumoxide-based browser runtime.
//!
//! Provides an implementation of [`BrowserRuntime`](crate::browser::BrowserRuntime)
//! backed by the `chromiumoxide` crate. The runtime currently supports local
//! launches and exposes helpers to open pages and fetch their content so higher
//! level components (e.g. `StagehandClient`) can drive real CDP interactions.

use std::collections::HashMap;
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
use tokio::{fs, sync::Mutex, task::JoinHandle};

use crate::browser::{
    BrowserRuntime, BrowserRuntimeError, BrowserbasePlan, BrowserbaseSessionCreateParams,
    BrowserbaseSessionStrategy, LocalLaunchStrategy, LocalPlan,
};

const DEFAULT_BROWSERBASE_API_URL: &str = "https://api.browserbase.com/v1";

#[derive(Default)]
pub struct ChromiumoxideRuntime {
    state: Mutex<Option<RuntimeState>>,
}

struct RuntimeState {
    browser: Arc<Browser>,
    _handler: JoinHandle<()>,
    pages: HashMap<String, ChromiumPage>,
    remote: Option<BrowserbaseRuntimeState>,
    #[allow(dead_code)]
    temp_user_data_dir: Option<PathBuf>,
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
            state: Mutex::new(None),
        }
    }

    async fn set_remote_state(
        &self,
        remote: BrowserbaseRuntimeState,
    ) -> Result<(), BrowserRuntimeError> {
        let mut guard = self.state.lock().await;
        match guard.as_mut() {
            Some(state) => {
                state.remote = Some(remote);
                Ok(())
            }
            None => Err(BrowserRuntimeError::Message(
                "browser runtime not initialised after connection".into(),
            )),
        }
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

        Ok(())
    }

    async fn new_page(&self, url: &str) -> Result<String, BrowserRuntimeError> {
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
        }

        Ok(page_id)
    }

    async fn page_content(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
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

    let mut guard = state.lock().await;
    *guard = Some(RuntimeState {
        browser,
        _handler: join,
        pages: HashMap::new(),
        remote: None,
        temp_user_data_dir: None,
    });

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

    let mut guard = state.lock().await;
    *guard = Some(RuntimeState {
        browser,
        _handler: join,
        pages: HashMap::new(),
        remote: None,
        temp_user_data_dir: None,
    });

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
