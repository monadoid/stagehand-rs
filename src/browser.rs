//! Browser connection primitives for Stagehand.
//!
//! This module focuses on transforming the high-level configuration into
//! strongly-typed connection specifications for either Browserbase-backed
//! remote sessions or local persistent contexts.

use std::env;
use std::fmt;
use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use thiserror::Error;

use crate::config::{Environment, StagehandConfig};

type JsonObject = JsonMap<String, JsonValue>;

/// Error surfaced while constructing browser connection specifications.
#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("failed to determine downloads directory: {source}")]
    DownloadsPath {
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse Browserbase session parameters: {source}")]
    InvalidBrowserbaseParams {
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to parse local browser launch options: {source}")]
    InvalidLocalOptions {
        #[source]
        source: serde_json::Error,
    },
}

/// High-level browser connection derived from the configuration.
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserConnectionSpec {
    /// Connect to Browserbase using an existing session or create a new one.
    Browserbase(BrowserbaseConnection),
    /// Launch or attach to a local browser instance.
    Local(LocalBrowserConnection),
}

/// Details required to work with a Browserbase session.
#[derive(Debug, Clone, PartialEq)]
pub struct BrowserbaseConnection {
    pub api_key: Option<String>,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub create_params: Option<BrowserbaseSessionCreateParams>,
}

/// Details required to launch or connect to a local browser.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalBrowserConnection {
    pub cdp_url: Option<String>,
    pub headers: Option<JsonObject>,
    pub user_data_dir: Option<PathBuf>,
    pub downloads_path: PathBuf,
    pub cookies: Option<Vec<JsonValue>>,
    pub launch_options: LocalLaunchOptions,
    pub chrome_executable: Option<PathBuf>,
}

/// Persistent context launch options loosely mirroring Playwright's API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalLaunchOptions {
    pub headless: bool,
    #[serde(rename = "acceptDownloads")]
    pub accept_downloads: bool,
    pub args: Vec<String>,
    pub viewport: Viewport,
    pub locale: String,
    #[serde(rename = "timezoneId")]
    pub timezone_id: String,
    #[serde(rename = "bypassCSP")]
    pub bypass_csp: bool,
    pub proxy: Option<JsonObject>,
    #[serde(rename = "ignoreHTTPSErrors")]
    pub ignore_https_errors: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct LocalLaunchOverrides {
    #[serde(alias = "cdp_url")]
    pub cdp_url: Option<String>,
    pub headers: Option<JsonObject>,
    #[serde(alias = "user_data_dir")]
    pub user_data_dir: Option<String>,
    #[serde(alias = "downloads_path")]
    pub downloads_path: Option<String>,
    pub cookies: Option<Vec<JsonValue>>,
    pub headless: Option<bool>,
    #[serde(alias = "accept_downloads")]
    pub accept_downloads: Option<bool>,
    #[serde(alias = "bypass_csp")]
    pub bypass_csp: Option<bool>,
    #[serde(alias = "ignore_https_errors")]
    pub ignore_https_errors: Option<bool>,
    pub locale: Option<String>,
    #[serde(alias = "timezone_id")]
    pub timezone_id: Option<String>,
    pub proxy: Option<JsonObject>,
    pub args: Option<Vec<String>>,
    pub viewport: Option<Viewport>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserbaseSessionCreateParams {
    #[serde(rename = "project_id")]
    pub project_id: Option<String>,
    #[serde(rename = "browser_settings")]
    pub browser_settings: BrowserSettings,
    #[serde(flatten)]
    pub extra: JsonObject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrowserSettings {
    pub viewport: Viewport,
    #[serde(flatten)]
    pub extra: JsonObject,
}

/// Viewport dimensions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Viewport {
    pub width: u32,
    pub height: u32,
}

/// Normalised execution plan derived from a [`BrowserConnectionSpec`].
#[derive(Debug, Clone, PartialEq)]
pub enum BrowserPlan {
    Browserbase(BrowserbasePlan),
    Local(LocalPlan),
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrowserbasePlan {
    pub api_key: Option<String>,
    pub project_id: Option<String>,
    pub strategy: BrowserbaseSessionStrategy,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserbaseSessionStrategy {
    UseExisting {
        session_id: String,
    },
    CreateNew {
        params: BrowserbaseSessionCreateParams,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LocalPlan {
    pub strategy: LocalLaunchStrategy,
    pub launch_options: LocalLaunchOptions,
    pub downloads_path: PathBuf,
    pub cookies: Option<Vec<JsonValue>>,
    pub chrome_executable: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum RuntimeTargetEvent {
    Attached { target_id: String },
    Detached { target_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum LocalLaunchStrategy {
    AttachCdp {
        url: String,
        headers: Option<JsonObject>,
    },
    LaunchPersistent {
        user_data_dir: Option<PathBuf>,
    },
}

/// Adapter that bridges `BrowserPlan` to actual browser runtime.
#[async_trait]
pub trait BrowserRuntime: Send + Sync {
    async fn connect_browserbase(&self, plan: &BrowserbasePlan) -> Result<(), BrowserRuntimeError>;

    async fn launch_local(&self, plan: &LocalPlan) -> Result<(), BrowserRuntimeError>;

    async fn shutdown(&self) -> Result<(), BrowserRuntimeError> {
        Err(BrowserRuntimeError::Unsupported(
            "runtime shutdown not implemented".to_string(),
        ))
    }

    async fn new_page(&self, url: &str) -> Result<String, BrowserRuntimeError>;

    async fn page_content(&self, page_id: &str) -> Result<Option<String>, BrowserRuntimeError>;

    async fn list_pages(&self) -> Result<Vec<String>, BrowserRuntimeError>;

    async fn target_event_stream(
        &self,
    ) -> Result<tokio::sync::broadcast::Receiver<RuntimeTargetEvent>, BrowserRuntimeError> {
        Err(BrowserRuntimeError::Unsupported(
            "target event stream not supported".to_string(),
        ))
    }

    async fn main_frame_id(&self, _page_id: &str) -> Result<Option<String>, BrowserRuntimeError> {
        Err(BrowserRuntimeError::Unsupported(
            "main frame lookup not supported".to_string(),
        ))
    }
}

#[derive(Debug, Error)]
pub enum BrowserRuntimeError {
    #[error("browser runtime error: {0}")]
    Message(String),
    #[error("browser runtime not initialized")]
    NotInitialized,
    #[error("browser runtime feature unsupported: {0}")]
    Unsupported(String),
}

/// High-level browser client that owns planning and runtime dispatch.
pub struct StagehandBrowser<R: BrowserRuntime> {
    manager: BrowserManager,
    runtime: R,
}

/// Entry point for preparing browser connection plans based on configuration.
#[derive(Debug, Clone)]
pub struct BrowserManager {
    config: StagehandConfig,
    spec: BrowserConnectionSpec,
}

impl Default for Viewport {
    fn default() -> Self {
        Viewport {
            width: 1288,
            height: 711,
        }
    }
}

impl Default for BrowserSettings {
    fn default() -> Self {
        BrowserSettings {
            viewport: Viewport::default(),
            extra: JsonObject::new(),
        }
    }
}

impl Default for BrowserbaseSessionCreateParams {
    fn default() -> Self {
        BrowserbaseSessionCreateParams {
            project_id: None,
            browser_settings: BrowserSettings::default(),
            extra: JsonObject::new(),
        }
    }
}

impl BrowserConnectionSpec {
    /// Build a browser connection specification from the Stagehand configuration.
    pub fn from_config(config: &StagehandConfig) -> Result<Self, BrowserError> {
        match config.env {
            Environment::Browserbase => {
                let conn = BrowserbaseConnection::from_config(config)?;
                Ok(BrowserConnectionSpec::Browserbase(conn))
            }
            Environment::Local => {
                LocalBrowserConnection::from_config(config).map(BrowserConnectionSpec::Local)
            }
        }
    }
}

impl BrowserbaseConnection {
    pub fn from_config(config: &StagehandConfig) -> Result<Self, BrowserError> {
        let session_id = config.browserbase_session_id.clone();
        let project_id = config.project_id.clone();

        let create_params = match (
            session_id.is_some(),
            config.browserbase_session_create_params(),
        ) {
            (true, None) => None,
            (_, maybe_params) => {
                let mut params = default_browserbase_params(project_id.as_deref());
                if let Some(custom) = maybe_params {
                    params = merge_json_objects(params, &JsonValue::Object(custom));
                }

                let mut typed: BrowserbaseSessionCreateParams =
                    serde_json::from_value(JsonValue::Object(params))
                        .map_err(|source| BrowserError::InvalidBrowserbaseParams { source })?;

                if typed.project_id.is_none() {
                    typed.project_id = project_id.clone();
                }

                Some(typed)
            }
        };

        Ok(BrowserbaseConnection {
            api_key: config.api_key.clone(),
            project_id,
            session_id,
            create_params,
        })
    }
}

impl BrowserbaseConnection {
    pub fn plan(&self) -> BrowserbasePlan {
        let strategy = match (&self.session_id, &self.create_params) {
            (Some(session_id), _) => BrowserbaseSessionStrategy::UseExisting {
                session_id: session_id.clone(),
            },
            (None, Some(params)) => BrowserbaseSessionStrategy::CreateNew {
                params: params.clone(),
            },
            (None, None) => BrowserbaseSessionStrategy::CreateNew {
                params: BrowserbaseSessionCreateParams::default(),
            },
        };

        BrowserbasePlan {
            api_key: self.api_key.clone(),
            project_id: self.project_id.clone(),
            strategy,
        }
    }
}

impl LocalBrowserConnection {
    pub fn from_config(config: &StagehandConfig) -> Result<Self, BrowserError> {
        let map = &config.local_browser_launch_options;
        let overrides: LocalLaunchOverrides =
            serde_json::from_value(JsonValue::Object(map.clone()))
                .map_err(|source| BrowserError::InvalidLocalOptions { source })?;

        let cdp_url = overrides.cdp_url;
        let headers = overrides.headers;
        let user_data_dir = overrides.user_data_dir.map(PathBuf::from);

        let downloads_path = overrides
            .downloads_path
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(|| {
                env::current_dir()
                    .map(|dir| dir.join("downloads"))
                    .map_err(|source| BrowserError::DownloadsPath { source })
            })?;

        let cookies = overrides.cookies;

        let headless = overrides.headless.unwrap_or(config.headless);
        let accept_downloads = overrides.accept_downloads.unwrap_or(true);
        let bypass_csp = overrides.bypass_csp.unwrap_or(true);
        let ignore_https_errors = overrides.ignore_https_errors.unwrap_or(true);
        let locale = overrides.locale.unwrap_or_else(|| "en-US".to_string());
        let timezone_id = overrides
            .timezone_id
            .unwrap_or_else(|| "America/New_York".to_string());
        let proxy = overrides.proxy;

        let args = overrides
            .args
            .unwrap_or_else(|| vec!["--disable-blink-features=AutomationControlled".to_string()]);

        let viewport = overrides.viewport.unwrap_or_default();

        let chrome_executable =
            string_from(map, &["chrome_executable", "chromeExecutable"]).map(PathBuf::from);

        let launch_options = LocalLaunchOptions {
            headless,
            accept_downloads,
            args,
            viewport,
            locale,
            timezone_id,
            bypass_csp,
            proxy,
            ignore_https_errors,
        };

        Ok(LocalBrowserConnection {
            cdp_url,
            headers,
            user_data_dir,
            downloads_path,
            cookies,
            launch_options,
            chrome_executable,
        })
    }
}

impl LocalBrowserConnection {
    pub fn plan(&self) -> LocalPlan {
        let strategy = if let Some(url) = &self.cdp_url {
            LocalLaunchStrategy::AttachCdp {
                url: url.clone(),
                headers: self.headers.clone(),
            }
        } else {
            LocalLaunchStrategy::LaunchPersistent {
                user_data_dir: self.user_data_dir.clone(),
            }
        };

        LocalPlan {
            strategy,
            launch_options: self.launch_options.clone(),
            downloads_path: self.downloads_path.clone(),
            cookies: self.cookies.clone(),
            chrome_executable: self.chrome_executable.clone(),
        }
    }
}

impl BrowserManager {
    pub fn new(config: StagehandConfig) -> Result<Self, BrowserError> {
        let spec = BrowserConnectionSpec::from_config(&config)?;
        Ok(Self { config, spec })
    }

    pub fn config(&self) -> &StagehandConfig {
        &self.config
    }

    pub fn spec(&self) -> &BrowserConnectionSpec {
        &self.spec
    }

    pub fn plan(&self) -> BrowserPlan {
        match &self.spec {
            BrowserConnectionSpec::Browserbase(conn) => BrowserPlan::Browserbase(conn.plan()),
            BrowserConnectionSpec::Local(conn) => BrowserPlan::Local(conn.plan()),
        }
    }
}

fn default_browserbase_params(project_id: Option<&str>) -> JsonObject {
    let mut params = JsonObject::new();
    if let Some(id) = project_id {
        params.insert("project_id".to_string(), JsonValue::String(id.to_string()));
    }
    params.insert(
        "browser_settings".to_string(),
        json!({"viewport": {"width": 1288, "height": 711}}),
    );
    params
}

fn merge_json_objects(base: JsonObject, overlay: &JsonValue) -> JsonObject {
    let mut base_value = JsonValue::Object(base);
    merge_json_values(&mut base_value, overlay);
    match base_value {
        JsonValue::Object(map) => map,
        _ => unreachable!("merge_json_values should keep object shape"),
    }
}

fn merge_json_values(base: &mut JsonValue, overlay: &JsonValue) {
    match (base, overlay) {
        (JsonValue::Object(base_map), JsonValue::Object(overlay_map)) => {
            for (key, value) in overlay_map {
                match base_map.get_mut(key) {
                    Some(existing) => merge_json_values(existing, value),
                    None => {
                        base_map.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (base_slot, overlay_value) => {
            *base_slot = overlay_value.clone();
        }
    }
}

fn string_from(map: &JsonObject, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        map.get(*key).and_then(|value| match value {
            JsonValue::String(val) => Some(val.to_string()),
            _ => None,
        })
    })
}

impl<R: BrowserRuntime> StagehandBrowser<R> {
    pub fn new(mut config: StagehandConfig, runtime: R) -> Result<Self, BrowserError> {
        if config.env == Environment::Local {
            config.use_api = false;
        } else if let Some(params) = config.browserbase_session_create_params() {
            let should_disable = params
                .get("region")
                .and_then(JsonValue::as_str)
                .map(|region| !region.eq_ignore_ascii_case("us-west-2"))
                .unwrap_or(false);
            if should_disable {
                config.use_api = false;
            }
        }

        let manager = BrowserManager::new(config)?;
        Ok(Self { manager, runtime })
    }

    pub fn manager(&self) -> &BrowserManager {
        &self.manager
    }

    pub fn runtime(&self) -> &R {
        &self.runtime
    }

    pub fn config(&self) -> &StagehandConfig {
        self.manager.config()
    }

    pub fn plan(&self) -> BrowserPlan {
        self.manager.plan()
    }

    pub async fn execute(&self) -> Result<(), BrowserRuntimeError> {
        match self.plan() {
            BrowserPlan::Browserbase(plan) => self.runtime.connect_browserbase(&plan).await,
            BrowserPlan::Local(plan) => self.runtime.launch_local(&plan).await,
        }
    }

    pub async fn shutdown(&self) -> Result<(), BrowserRuntimeError> {
        self.runtime.shutdown().await
    }
}

impl<R: BrowserRuntime> fmt::Debug for StagehandBrowser<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagehandBrowser")
            .field("plan", &self.plan())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::Mutex;

    #[test]
    fn browserbase_defaults_include_project_and_viewport() {
        let mut config = StagehandConfig::default();
        config.project_id = Some("proj-123".to_string());
        let spec = BrowserConnectionSpec::from_config(&config).unwrap();
        match spec {
            BrowserConnectionSpec::Browserbase(conn) => {
                assert!(conn.session_id.is_none());
                let params = conn.create_params.expect("create params");
                assert_eq!(params.project_id.as_deref(), Some("proj-123"));
                assert_eq!(params.browser_settings.viewport.width, 1288);
                assert_eq!(params.browser_settings.viewport.height, 711);
            }
            BrowserConnectionSpec::Local(_) => panic!("expected browserbase"),
        }
    }

    #[test]
    fn local_connection_parses_overrides() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        let mut local_opts = JsonObject::new();
        local_opts.insert("headless".into(), JsonValue::Bool(true));
        local_opts.insert(
            "downloads_path".into(),
            JsonValue::String("/tmp/downloads".into()),
        );
        local_opts.insert(
            "args".into(),
            JsonValue::Array(vec![JsonValue::String("--foo".into())]),
        );
        local_opts.insert("viewport".into(), json!({"width": 1024, "height": 768}));
        local_opts.insert(
            "cookies".into(),
            JsonValue::Array(vec![json!({"name": "auth", "value": "123"})]),
        );

        config.local_browser_launch_options = local_opts;

        let spec = BrowserConnectionSpec::from_config(&config).unwrap();
        match spec {
            BrowserConnectionSpec::Local(conn) => {
                assert!(conn.cdp_url.is_none());
                assert_eq!(conn.launch_options.headless, true);
                assert_eq!(conn.launch_options.args, vec!["--foo".to_string()]);
                assert_eq!(
                    conn.launch_options.viewport,
                    Viewport {
                        width: 1024,
                        height: 768
                    }
                );
                assert_eq!(conn.downloads_path, Path::new("/tmp/downloads"));
                assert!(conn.cookies.is_some());
            }
            BrowserConnectionSpec::Browserbase(_) => panic!("expected local"),
        }
    }

    #[test]
    fn browser_manager_plan_for_existing_session() {
        let mut config = StagehandConfig::default();
        config.browserbase_session_id = Some("session-123".to_string());
        config.api_key = Some("api-key".to_string());
        config.project_id = Some("proj-1".to_string());

        let manager = BrowserManager::new(config).expect("manager");
        match manager.plan() {
            BrowserPlan::Browserbase(plan) => {
                assert_eq!(plan.api_key.as_deref(), Some("api-key"));
                assert_eq!(plan.project_id.as_deref(), Some("proj-1"));
                match plan.strategy {
                    BrowserbaseSessionStrategy::UseExisting { session_id } => {
                        assert_eq!(session_id, "session-123");
                    }
                    _ => panic!("expected use existing strategy"),
                }
            }
            _ => panic!("expected browserbase plan"),
        }
    }

    #[test]
    fn browser_manager_plan_for_local_launch() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        config.headless = true;

        let manager = BrowserManager::new(config).expect("manager");
        match manager.plan() {
            BrowserPlan::Local(plan) => match plan.strategy {
                LocalLaunchStrategy::LaunchPersistent { user_data_dir } => {
                    assert!(user_data_dir.is_none());
                    assert!(plan.cookies.is_none());
                    assert!(plan.launch_options.headless);
                }
                _ => panic!("expected launch strategy"),
            },
            _ => panic!("expected local plan"),
        }
    }

    #[test]
    fn local_env_disables_use_api() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        config.use_api = true;

        let runtime = RecordingRuntime::default();
        let browser = StagehandBrowser::new(config, runtime).expect("browser");
        assert!(!browser.config().use_api);
    }

    #[test]
    fn non_us_west_region_disables_use_api() {
        let mut config = StagehandConfig::default();
        config.api_key = Some("key".into());
        config.project_id = Some("proj".into());
        let mut params = JsonObject::new();
        params.insert(
            "region".into(),
            JsonValue::String("eu-central-1".to_string()),
        );
        config.browserbase_session_create_params = Some(params);

        let runtime = RecordingRuntime::default();
        let browser = StagehandBrowser::new(config, runtime).expect("browser");
        assert!(!browser.config().use_api);
    }

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

    #[tokio::test]
    async fn stagehand_browser_executes_browserbase_plan() {
        let mut config = StagehandConfig::default();
        config.browserbase_session_id = Some("existing".into());
        let runtime = RecordingRuntime::default();
        let browser = StagehandBrowser::new(config, runtime).expect("browser");
        browser.execute().await.expect("execute");
        assert_eq!(*browser.runtime().browserbase_calls.lock().unwrap(), 1);
        assert_eq!(*browser.runtime().local_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn stagehand_browser_executes_local_plan() {
        let mut config = StagehandConfig::default();
        config.env = Environment::Local;
        let runtime = RecordingRuntime::default();
        let browser = StagehandBrowser::new(config, runtime).expect("browser");
        browser.execute().await.expect("execute");
        assert_eq!(*browser.runtime().local_calls.lock().unwrap(), 1);
        assert_eq!(*browser.runtime().browserbase_calls.lock().unwrap(), 0);
    }
}
