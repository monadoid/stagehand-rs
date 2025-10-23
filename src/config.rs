//! Strongly-typed configuration primitives for the Stagehand Rust port.
//!
//! This module mirrors the behaviour of the Python `StagehandConfig` class while
//! embracing Rust's type system.  Configuration values can be constructed from
//! defaults, loaded from environment variables (with optional `.env` support),
//! or merged with explicit overrides for ergonomic programmatic updates.

use std::env;
use std::fmt;
use std::num::ParseIntError;
use std::sync::Arc;

use dotenvy::dotenv;
use serde::de::{Deserialize, Deserializer, Error as DeError};
use serde::ser::{Serialize, Serializer};
use serde::{Deserialize as DeriveDeserialize, Serialize as DeriveSerialize};
use serde_json::{Map as JsonMap, Value as JsonValue};
use thiserror::Error;

type JsonObject = JsonMap<String, JsonValue>;

/// Default public Stagehand API endpoint.
pub const DEFAULT_API_URL: &str = "https://api.stagehand.browserbase.com/v1";

/// Shared logger callback signature used by the configuration.
pub type LoggerCallback = Arc<dyn Fn(&str) + Send + Sync + 'static>;

/// Environment that Stagehand should target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, DeriveSerialize, DeriveDeserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Environment {
    Browserbase,
    Local,
}

impl Default for Environment {
    fn default() -> Self {
        Environment::Browserbase
    }
}

impl Environment {
    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "BROWSERBASE" => Some(Environment::Browserbase),
            "LOCAL" => Some(Environment::Local),
            _ => None,
        }
    }
}

/// Verbosity level for Stagehand logging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verbosity {
    Minimal,
    Medium,
    Detailed,
}

impl Verbosity {
    fn as_u8(self) -> u8 {
        match self {
            Verbosity::Minimal => 0,
            Verbosity::Medium => 1,
            Verbosity::Detailed => 2,
        }
    }

    fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Verbosity::Minimal),
            1 => Some(Verbosity::Medium),
            2 => Some(Verbosity::Detailed),
            _ => None,
        }
    }
}

impl Default for Verbosity {
    fn default() -> Self {
        Verbosity::Medium
    }
}

impl Serialize for Verbosity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_u8(self.as_u8())
    }
}

impl<'de> Deserialize<'de> for Verbosity {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = u8::deserialize(deserializer)?;
        Verbosity::from_u8(value).ok_or_else(|| {
            DeError::custom(format!(
                "invalid verbosity value {value}; expected 0, 1, or 2"
            ))
        })
    }
}

/// Supported LLM model names for Stagehand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, DeriveSerialize, DeriveDeserialize)]
pub enum ModelName {
    #[serde(rename = "gpt-4o")]
    Gpt4o,
    #[serde(rename = "gpt-4o-mini")]
    Gpt4oMini,
    #[serde(rename = "claude-3-5-sonnet-latest")]
    Claude35SonnetLatest,
    #[serde(rename = "claude-3-7-sonnet-latest")]
    Claude37SonnetLatest,
    #[serde(rename = "computer-use-preview")]
    ComputerUsePreview,
    #[serde(rename = "gemini-2.0-flash")]
    Gemini20Flash,
}

impl Default for ModelName {
    fn default() -> Self {
        ModelName::Gpt4o
    }
}

impl ModelName {
    fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "gpt-4o" => Some(ModelName::Gpt4o),
            "gpt-4o-mini" => Some(ModelName::Gpt4oMini),
            "claude-3-5-sonnet-latest" => Some(ModelName::Claude35SonnetLatest),
            "claude-3-7-sonnet-latest" => Some(ModelName::Claude37SonnetLatest),
            "computer-use-preview" => Some(ModelName::ComputerUsePreview),
            "gemini-2.0-flash" => Some(ModelName::Gemini20Flash),
            _ => None,
        }
    }
}

/// Configuration values for the Stagehand client.
#[derive(DeriveSerialize, DeriveDeserialize, Clone)]
#[serde(default)]
pub struct StagehandConfig {
    pub env: Environment,
    #[serde(alias = "apiKey")]
    pub api_key: Option<String>,
    #[serde(alias = "projectId")]
    pub project_id: Option<String>,
    #[serde(alias = "apiUrl")]
    pub api_url: String,
    #[serde(alias = "browserbaseSessionCreateParams")]
    pub browserbase_session_create_params: Option<JsonObject>,
    #[serde(alias = "browserbaseSessionID")]
    pub browserbase_session_id: Option<String>,
    #[serde(alias = "modelName")]
    pub model_name: ModelName,
    #[serde(alias = "modelApiKey")]
    pub model_api_key: Option<String>,
    #[serde(alias = "modelClientOptions")]
    pub model_client_options: Option<JsonObject>,
    #[serde(skip_serializing, skip_deserializing)]
    pub logger: Option<LoggerCallback>,
    pub verbose: Verbosity,
    #[serde(alias = "useRichLogging")]
    pub use_rich_logging: bool,
    #[serde(alias = "domSettleTimeoutMs")]
    pub dom_settle_timeout_ms: Option<u64>,
    #[serde(alias = "enableCaching")]
    pub enable_caching: bool,
    #[serde(alias = "selfHeal")]
    pub self_heal: bool,
    #[serde(alias = "waitForCaptchaSolves")]
    pub wait_for_captcha_solves: bool,
    #[serde(alias = "actTimeoutMs")]
    pub act_timeout_ms: Option<u64>,
    #[serde(alias = "systemPrompt")]
    pub system_prompt: Option<String>,
    #[serde(alias = "localBrowserLaunchOptions")]
    pub local_browser_launch_options: JsonObject,
    #[serde(alias = "useApi")]
    pub use_api: bool,
    pub experimental: bool,
    pub headless: bool,
}

impl Default for StagehandConfig {
    fn default() -> Self {
        let api_url = env::var("STAGEHAND_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
        StagehandConfig {
            env: Environment::default(),
            api_key: None,
            project_id: None,
            api_url,
            browserbase_session_create_params: None,
            browserbase_session_id: None,
            model_name: ModelName::default(),
            model_api_key: None,
            model_client_options: None,
            logger: None,
            verbose: Verbosity::default(),
            use_rich_logging: true,
            dom_settle_timeout_ms: Some(3_000),
            enable_caching: false,
            self_heal: true,
            wait_for_captcha_solves: false,
            act_timeout_ms: None,
            system_prompt: None,
            local_browser_launch_options: JsonObject::new(),
            use_api: true,
            experimental: false,
            headless: false,
        }
    }
}

impl StagehandConfig {
    /// Construct a configuration by reading relevant environment variables, after
    /// loading a `.env` file if present.
    pub fn from_env() -> Result<Self, StagehandConfigError> {
        let _ = dotenv();
        let mut config = StagehandConfig::default();

        if let Some(value) = env_var("STAGEHAND_ENV") {
            config.env = Environment::parse(&value).ok_or_else(|| {
                StagehandConfigError::invalid_enum("STAGEHAND_ENV", value.clone())
            })?;
        }

        if let Some(value) = env_var("BROWSERBASE_API_KEY") {
            config.api_key = Some(value);
        }

        if let Some(value) = env_var("BROWSERBASE_PROJECT_ID") {
            config.project_id = Some(value);
        }

        if let Some(value) = env_var("STAGEHAND_API_URL") {
            config.api_url = value;
        }

        if let Some(value) = env_var("MODEL_API_KEY") {
            config.model_api_key = Some(value);
        }

        if let Some(value) = env_var("MODEL_NAME") {
            config.model_name = ModelName::parse(&value)
                .ok_or_else(|| StagehandConfigError::invalid_enum("MODEL_NAME", value.clone()))?;
        }

        if let Some(value) = env_var("STAGEHAND_VERBOSE") {
            let parsed = parse_u8("STAGEHAND_VERBOSE", &value)?;
            config.verbose = Verbosity::from_u8(parsed).ok_or_else(|| {
                StagehandConfigError::invalid_enum("STAGEHAND_VERBOSE", parsed.to_string())
            })?;
        }

        if let Some(value) = env_var("STAGEHAND_USE_RICH_LOGGING") {
            config.use_rich_logging = parse_bool("STAGEHAND_USE_RICH_LOGGING", &value)?;
        }

        if let Some(value) = env_var("STAGEHAND_DOM_SETTLE_TIMEOUT_MS") {
            config.dom_settle_timeout_ms =
                Some(parse_u64("STAGEHAND_DOM_SETTLE_TIMEOUT_MS", &value)?);
        }

        if let Some(value) = env_var("STAGEHAND_ENABLE_CACHING") {
            config.enable_caching = parse_bool("STAGEHAND_ENABLE_CACHING", &value)?;
        }

        if let Some(value) = env_var("BROWSERBASE_SESSION_ID") {
            config.browserbase_session_id = Some(value);
        }

        if let Some(value) = env_var("STAGEHAND_SELF_HEAL") {
            config.self_heal = parse_bool("STAGEHAND_SELF_HEAL", &value)?;
        }

        if let Some(value) = env_var("STAGEHAND_WAIT_FOR_CAPTCHA_SOLVES") {
            config.wait_for_captcha_solves =
                parse_bool("STAGEHAND_WAIT_FOR_CAPTCHA_SOLVES", &value)?;
        }

        if let Some(value) = env_var("STAGEHAND_ACT_TIMEOUT_MS") {
            config.act_timeout_ms = Some(parse_u64("STAGEHAND_ACT_TIMEOUT_MS", &value)?);
        }

        if let Some(value) = env_var("STAGEHAND_SYSTEM_PROMPT") {
            config.system_prompt = Some(value);
        }

        if let Some(value) = env_var("STAGEHAND_MODEL_CLIENT_OPTIONS") {
            config.model_client_options =
                Some(parse_json_object("STAGEHAND_MODEL_CLIENT_OPTIONS", &value)?);
        }

        if let Some(value) = env_var("STAGEHAND_LOCAL_BROWSER_LAUNCH_OPTIONS") {
            config.local_browser_launch_options =
                parse_json_object("STAGEHAND_LOCAL_BROWSER_LAUNCH_OPTIONS", &value)?;
        }

        if let Some(value) = env_var("BROWSERBASE_SESSION_CREATE_PARAMS") {
            config.browserbase_session_create_params = Some(parse_json_object(
                "BROWSERBASE_SESSION_CREATE_PARAMS",
                &value,
            )?);
        }

        if let Some(value) = env_var("STAGEHAND_USE_API") {
            config.use_api = parse_bool("STAGEHAND_USE_API", &value)?;
        }

        if let Some(value) = env_var("STAGEHAND_EXPERIMENTAL") {
            config.experimental = parse_bool("STAGEHAND_EXPERIMENTAL", &value)?;
        }

        if let Some(value) = env_var("STAGEHAND_HEADLESS") {
            config.headless = parse_bool("STAGEHAND_HEADLESS", &value)?;
        }

        config.ensure_session_project_id();
        Ok(config)
    }

    /// Return browserbase session parameters with an injected `project_id` if
    /// the configuration defines one and the params omit it.
    pub fn browserbase_session_create_params(&self) -> Option<JsonObject> {
        let mut params = self.browserbase_session_create_params.clone()?;
        if params.get("project_id").is_none() {
            if let Some(project_id) = &self.project_id {
                params.insert(
                    "project_id".to_string(),
                    JsonValue::String(project_id.clone()),
                );
            }
        }
        Some(params)
    }

    /// Create a new configuration with explicit field overrides applied.
    pub fn with_overrides(&self, overrides: StagehandConfigOverrides) -> StagehandConfig {
        let mut next = self.clone();

        if let Some(env) = overrides.env {
            next.env = env;
        }
        if let Some(value) = overrides.api_key {
            next.api_key = value;
        }
        if let Some(value) = overrides.project_id {
            next.project_id = value;
        }
        if let Some(value) = overrides.api_url {
            next.api_url = value;
        }
        if let Some(value) = overrides.browserbase_session_create_params {
            next.browserbase_session_create_params = value;
        }
        if let Some(value) = overrides.browserbase_session_id {
            next.browserbase_session_id = value;
        }
        if let Some(value) = overrides.model_name {
            next.model_name = value;
        }
        if let Some(value) = overrides.model_api_key {
            next.model_api_key = value;
        }
        if let Some(value) = overrides.model_client_options {
            next.model_client_options = value;
        }
        if let Some(value) = overrides.logger {
            next.logger = value;
        }
        if let Some(value) = overrides.verbose {
            next.verbose = value;
        }
        if let Some(value) = overrides.use_rich_logging {
            next.use_rich_logging = value;
        }
        if let Some(value) = overrides.dom_settle_timeout_ms {
            next.dom_settle_timeout_ms = value;
        }
        if let Some(value) = overrides.enable_caching {
            next.enable_caching = value;
        }
        if let Some(value) = overrides.self_heal {
            next.self_heal = value;
        }
        if let Some(value) = overrides.wait_for_captcha_solves {
            next.wait_for_captcha_solves = value;
        }
        if let Some(value) = overrides.act_timeout_ms {
            next.act_timeout_ms = value;
        }
        if let Some(value) = overrides.system_prompt {
            next.system_prompt = value;
        }
        if let Some(value) = overrides.local_browser_launch_options {
            next.local_browser_launch_options = value;
        }
        if let Some(value) = overrides.use_api {
            next.use_api = value;
        }
        if let Some(value) = overrides.experimental {
            next.experimental = value;
        }
        if let Some(value) = overrides.headless {
            next.headless = value;
        }

        next.ensure_session_project_id();
        next
    }

    fn ensure_session_project_id(&mut self) {
        if let Some(project_id) = self.project_id.clone() {
            if let Some(params) = self.browserbase_session_create_params.as_mut() {
                params
                    .entry("project_id".to_string())
                    .or_insert(JsonValue::String(project_id));
            }
        }
    }
}

/// Field-level overrides for [`StagehandConfig::with_overrides`].
#[derive(Default, Clone)]
pub struct StagehandConfigOverrides {
    pub env: Option<Environment>,
    pub api_key: Option<Option<String>>,
    pub project_id: Option<Option<String>>,
    pub api_url: Option<String>,
    pub browserbase_session_create_params: Option<Option<JsonObject>>,
    pub browserbase_session_id: Option<Option<String>>,
    pub model_name: Option<ModelName>,
    pub model_api_key: Option<Option<String>>,
    pub model_client_options: Option<Option<JsonObject>>,
    pub logger: Option<Option<LoggerCallback>>,
    pub verbose: Option<Verbosity>,
    pub use_rich_logging: Option<bool>,
    pub dom_settle_timeout_ms: Option<Option<u64>>,
    pub enable_caching: Option<bool>,
    pub self_heal: Option<bool>,
    pub wait_for_captcha_solves: Option<bool>,
    pub act_timeout_ms: Option<Option<u64>>,
    pub system_prompt: Option<Option<String>>,
    pub local_browser_launch_options: Option<JsonObject>,
    pub use_api: Option<bool>,
    pub experimental: Option<bool>,
    pub headless: Option<bool>,
}

impl fmt::Debug for StagehandConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagehandConfig")
            .field("env", &self.env)
            .field("api_key", &self.api_key)
            .field("project_id", &self.project_id)
            .field("api_url", &self.api_url)
            .field(
                "browserbase_session_create_params",
                &self.browserbase_session_create_params,
            )
            .field("browserbase_session_id", &self.browserbase_session_id)
            .field("model_name", &self.model_name)
            .field("model_api_key", &self.model_api_key)
            .field("model_client_options", &self.model_client_options)
            .field("verbose", &self.verbose)
            .field("use_rich_logging", &self.use_rich_logging)
            .field("dom_settle_timeout_ms", &self.dom_settle_timeout_ms)
            .field("enable_caching", &self.enable_caching)
            .field("self_heal", &self.self_heal)
            .field("wait_for_captcha_solves", &self.wait_for_captcha_solves)
            .field("act_timeout_ms", &self.act_timeout_ms)
            .field("system_prompt", &self.system_prompt)
            .field(
                "local_browser_launch_options",
                &self.local_browser_launch_options,
            )
            .field("use_api", &self.use_api)
            .field("experimental", &self.experimental)
            .field("headless", &self.headless)
            .field("logger_present", &self.logger.is_some())
            .finish()
    }
}

impl fmt::Debug for StagehandConfigOverrides {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagehandConfigOverrides")
            .field("env", &self.env)
            .field("api_key", &self.api_key)
            .field("project_id", &self.project_id)
            .field("api_url", &self.api_url)
            .field(
                "browserbase_session_create_params",
                &self.browserbase_session_create_params,
            )
            .field("browserbase_session_id", &self.browserbase_session_id)
            .field("model_name", &self.model_name)
            .field("model_api_key", &self.model_api_key)
            .field("model_client_options", &self.model_client_options)
            .field("logger", &self.logger.as_ref().map(|inner| inner.is_some()))
            .field("verbose", &self.verbose)
            .field("use_rich_logging", &self.use_rich_logging)
            .field("dom_settle_timeout_ms", &self.dom_settle_timeout_ms)
            .field("enable_caching", &self.enable_caching)
            .field("self_heal", &self.self_heal)
            .field("wait_for_captcha_solves", &self.wait_for_captcha_solves)
            .field("act_timeout_ms", &self.act_timeout_ms)
            .field("system_prompt", &self.system_prompt)
            .field(
                "local_browser_launch_options",
                &self.local_browser_launch_options,
            )
            .field("use_api", &self.use_api)
            .field("experimental", &self.experimental)
            .field("headless", &self.headless)
            .finish()
    }
}

impl StagehandConfigOverrides {
    /// Builder-style helper to set the `env` override.
    pub fn env(mut self, env: Environment) -> Self {
        self.env = Some(env);
        self
    }

    /// Builder-style helper to set the `api_key` override.
    pub fn api_key<T: Into<Option<String>>>(mut self, api_key: T) -> Self {
        self.api_key = Some(api_key.into());
        self
    }
}

/// Errors that can arise while constructing a [`StagehandConfig`].
#[derive(Debug, Error)]
pub enum StagehandConfigError {
    #[error("invalid value '{value}' for {field}")]
    InvalidEnumVariant { field: &'static str, value: String },
    #[error("invalid boolean '{value}' for {field}")]
    InvalidBool { field: &'static str, value: String },
    #[error("invalid number '{value}' for {field}: {source}")]
    InvalidNumber {
        field: &'static str,
        value: String,
        #[source]
        source: ParseIntError,
    },
    #[error("{field} must be a JSON object")]
    InvalidJsonType { field: &'static str },
    #[error("invalid JSON for {field}: {source}")]
    InvalidJson {
        field: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

impl StagehandConfigError {
    fn invalid_enum(field: &'static str, value: String) -> Self {
        StagehandConfigError::InvalidEnumVariant { field, value }
    }
}

fn env_var(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn parse_bool(field: &'static str, value: &str) -> Result<bool, StagehandConfigError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(StagehandConfigError::InvalidBool {
            field,
            value: value.to_string(),
        }),
    }
}

fn parse_u8(field: &'static str, value: &str) -> Result<u8, StagehandConfigError> {
    value
        .trim()
        .parse::<u8>()
        .map_err(|source| StagehandConfigError::InvalidNumber {
            field,
            value: value.to_string(),
            source,
        })
}

fn parse_u64(field: &'static str, value: &str) -> Result<u64, StagehandConfigError> {
    value
        .trim()
        .parse::<u64>()
        .map_err(|source| StagehandConfigError::InvalidNumber {
            field,
            value: value.to_string(),
            source,
        })
}

fn parse_json_object(field: &'static str, value: &str) -> Result<JsonObject, StagehandConfigError> {
    let parsed: JsonValue = serde_json::from_str(value)
        .map_err(|source| StagehandConfigError::InvalidJson { field, source })?;
    match parsed {
        JsonValue::Object(map) => Ok(map),
        _ => Err(StagehandConfigError::InvalidJsonType { field }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[derive(Debug)]
    struct EnvGuard {
        saved: Vec<(String, Option<String>)>,
    }

    impl EnvGuard {
        fn new(vars: &[(&str, Option<&str>)]) -> Self {
            let saved = vars
                .iter()
                .map(|(key, value)| {
                    let original = env::var(key).ok();
                    match value {
                        Some(v) => unsafe {
                            env::set_var(key, v);
                        },
                        None => unsafe {
                            env::remove_var(key);
                        },
                    };
                    ((*key).to_string(), original)
                })
                .collect();
            EnvGuard { saved }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (key, value) in self.saved.drain(..) {
                match value {
                    Some(v) => unsafe {
                        env::set_var(&key, v);
                    },
                    None => unsafe {
                        env::remove_var(&key);
                    },
                }
            }
        }
    }

    fn with_env<F, T>(vars: &[(&str, Option<&str>)], f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let lock = env_lock().lock().expect("env mutex poisoned");
        let guard = EnvGuard::new(vars);
        let result = f();
        drop(guard);
        drop(lock);
        result
    }

    #[test]
    fn defaults_align_with_python_config() {
        with_env(&[("STAGEHAND_API_URL", None)], || {
            let config = StagehandConfig::default();
            assert_eq!(config.env, Environment::Browserbase);
            assert!(config.api_key.is_none());
            assert_eq!(config.api_url, DEFAULT_API_URL);
            assert_eq!(config.model_name, ModelName::Gpt4o);
            assert_eq!(config.verbose, Verbosity::Medium);
            assert!(config.use_rich_logging);
            assert_eq!(config.dom_settle_timeout_ms, Some(3_000));
            assert!(config.local_browser_launch_options.is_empty());
            assert!(config.browserbase_session_create_params.is_none());
            assert!(config.use_api);
            assert!(!config.experimental);
        });
    }

    #[test]
    fn from_env_parses_and_normalises_values() {
        let vars = [
            ("STAGEHAND_ENV", Some("local")),
            ("BROWSERBASE_API_KEY", Some("key-123")),
            ("BROWSERBASE_PROJECT_ID", Some("proj-abc")),
            ("STAGEHAND_API_URL", Some("https://custom")),
            ("MODEL_API_KEY", Some("model-key")),
            ("MODEL_NAME", Some("claude-3-5-sonnet-latest")),
            ("STAGEHAND_VERBOSE", Some("2")),
            ("STAGEHAND_USE_RICH_LOGGING", Some("false")),
            ("STAGEHAND_DOM_SETTLE_TIMEOUT_MS", Some("5000")),
            ("STAGEHAND_ENABLE_CACHING", Some("true")),
            ("BROWSERBASE_SESSION_ID", Some("session-1")),
            ("STAGEHAND_SELF_HEAL", Some("false")),
            ("STAGEHAND_WAIT_FOR_CAPTCHA_SOLVES", Some("true")),
            ("STAGEHAND_ACT_TIMEOUT_MS", Some("1234")),
            ("STAGEHAND_SYSTEM_PROMPT", Some("custom prompt")),
            (
                "STAGEHAND_MODEL_CLIENT_OPTIONS",
                Some(r#"{"api_base":"https://foo"}"#),
            ),
            (
                "STAGEHAND_LOCAL_BROWSER_LAUNCH_OPTIONS",
                Some(r#"{"headless":true}"#),
            ),
            (
                "BROWSERBASE_SESSION_CREATE_PARAMS",
                Some(r#"{"region":"us-east-1"}"#),
            ),
            ("STAGEHAND_USE_API", Some("false")),
            ("STAGEHAND_EXPERIMENTAL", Some("true")),
            ("STAGEHAND_HEADLESS", Some("true")),
        ];

        with_env(&vars, || {
            let config = StagehandConfig::from_env().expect("config from env");
            assert_eq!(config.env, Environment::Local);
            assert_eq!(config.api_key.as_deref(), Some("key-123"));
            assert_eq!(config.project_id.as_deref(), Some("proj-abc"));
            assert_eq!(config.api_url, "https://custom");
            assert_eq!(config.model_api_key.as_deref(), Some("model-key"));
            assert_eq!(config.model_name, ModelName::Claude35SonnetLatest);
            assert_eq!(config.verbose, Verbosity::Detailed);
            assert!(!config.use_rich_logging);
            assert_eq!(config.dom_settle_timeout_ms, Some(5_000));
            assert!(config.enable_caching);
            assert_eq!(config.browserbase_session_id.as_deref(), Some("session-1"));
            assert!(!config.self_heal);
            assert!(config.wait_for_captcha_solves);
            assert_eq!(config.act_timeout_ms, Some(1_234));
            assert_eq!(config.system_prompt.as_deref(), Some("custom prompt"));
            assert!(!config.use_api);
            assert!(config.experimental);
            assert!(config.headless);

            let client_options = config
                .model_client_options
                .as_ref()
                .expect("model client options present");
            assert_eq!(
                client_options.get("api_base"),
                Some(&JsonValue::String("https://foo".to_string()))
            );

            assert_eq!(
                config
                    .local_browser_launch_options
                    .get("headless")
                    .and_then(JsonValue::as_bool),
                Some(true)
            );

            let params = config
                .browserbase_session_create_params()
                .expect("session params");
            assert_eq!(
                params.get("project_id"),
                Some(&JsonValue::String("proj-abc".to_string()))
            );
            assert_eq!(
                params.get("region"),
                Some(&JsonValue::String("us-east-1".to_string()))
            );
        });
    }

    #[test]
    fn overrides_support_setting_values_to_none() {
        let base = StagehandConfig::default();
        let overrides = StagehandConfigOverrides::default()
            .env(Environment::Local)
            .api_key(Some("overridden".to_string()));
        let overrides = StagehandConfigOverrides {
            browserbase_session_id: Some(None),
            dom_settle_timeout_ms: Some(Some(1_000)),
            use_api: Some(false),
            ..overrides
        };

        let updated = base.with_overrides(overrides);
        assert_eq!(updated.env, Environment::Local);
        assert_eq!(updated.api_key.as_deref(), Some("overridden"));
        assert!(updated.browserbase_session_id.is_none());
        assert_eq!(updated.dom_settle_timeout_ms, Some(1_000));
        assert!(!updated.use_api);
    }
}
