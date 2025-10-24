//! Logging utilities mirroring the Stagehand Python logger.
//!
//! This module focuses on structured logging with optional external sinks so
//! higher-level components can forward messages to API consumers or custom
//! loggers while still providing a sensible default console output.

use std::fmt;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Verbosity;

/// Convenience alias for external logging callbacks.
pub type LogCallback = Arc<dyn Fn(&StagehandLogRecord) + Send + Sync + 'static>;

/// High-level logging configuration shared across the Stagehand runtime.
#[derive(Clone)]
pub struct LogConfig {
    pub verbose: Verbosity,
    pub use_rich: bool,
    pub env: String,
    pub external_logger: Option<LogCallback>,
    pub quiet_dependencies: bool,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            verbose: Verbosity::Medium,
            use_rich: true,
            env: "LOCAL".to_string(),
            external_logger: None,
            quiet_dependencies: true,
        }
    }
}

impl LogConfig {
    pub fn new(verbose: Verbosity) -> Self {
        Self {
            verbose,
            ..Default::default()
        }
    }

    pub fn should_log(&self, level: LogLevel) -> bool {
        level == LogLevel::Error || level.as_u8() <= verbosity_to_u8(self.verbose)
    }
}

/// Log severity used across Stagehand.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Error = 0,
    Info = 1,
    Debug = 2,
}

impl LogLevel {
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    pub fn label(self) -> &'static str {
        match self {
            LogLevel::Error => "ERROR",
            LogLevel::Info => "INFO",
            LogLevel::Debug => "DEBUG",
        }
    }
}

impl From<u8> for LogLevel {
    fn from(value: u8) -> Self {
        match value {
            0 => LogLevel::Error,
            2 => LogLevel::Debug,
            _ => LogLevel::Info,
        }
    }
}

impl From<i64> for LogLevel {
    fn from(value: i64) -> Self {
        LogLevel::from(value as u8)
    }
}

fn verbosity_to_u8(verbose: Verbosity) -> u8 {
    match verbose {
        Verbosity::Minimal => 0,
        Verbosity::Medium => 1,
        Verbosity::Detailed => 2,
    }
}

/// Structured log entry shared with external callbacks.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StagehandLogRecord {
    pub timestamp: DateTime<Utc>,
    pub message: String,
    pub level: LogLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auxiliary: Option<Value>,
}

impl StagehandLogRecord {
    pub fn new(
        message: impl Into<String>,
        level: LogLevel,
        category: Option<String>,
        auxiliary: Option<Value>,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            message: message.into(),
            level,
            category,
            auxiliary,
        }
    }
}

/// Default console printer used when no external logger is configured.
pub fn default_log_handler(record: &StagehandLogRecord) {
    let timestamp = record
        .timestamp
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    if let Some(category) = &record.category {
        println!(
            "[{}] {:<5} [{}] {}",
            timestamp,
            record.level.label(),
            category,
            record.message
        );
    } else {
        println!(
            "[{}] {:<5} {}",
            timestamp,
            record.level.label(),
            record.message
        );
    }
    if let Some(aux) = &record.auxiliary {
        if !aux.is_null() {
            println!("    {}", aux);
        }
    }
}

/// Stagehand logger that mirrors the behaviour of the Python implementation.
pub struct StagehandLogger {
    config: LogConfig,
    default_handler: LogCallback,
}

impl fmt::Debug for StagehandLogger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagehandLogger")
            .field("verbosity", &self.config.verbose)
            .field("use_rich", &self.config.use_rich)
            .field("env", &self.config.env)
            .field("external_logger", &self.config.external_logger.is_some())
            .finish()
    }
}

impl StagehandLogger {
    pub fn with_config(config: LogConfig) -> Self {
        Self {
            config,
            default_handler: Arc::new(default_log_handler),
        }
    }

    pub fn new(verbose: Verbosity) -> Self {
        Self::with_config(LogConfig::new(verbose))
    }

    pub fn config(&self) -> &LogConfig {
        &self.config
    }

    pub fn set_verbose(&mut self, verbose: Verbosity) {
        self.config.verbose = verbose;
    }

    pub fn set_external_logger(&mut self, logger: Option<LogCallback>) {
        self.config.external_logger = logger;
    }

    pub fn log(
        &self,
        message: impl Into<String>,
        level: LogLevel,
        category: Option<&str>,
        auxiliary: Option<Value>,
    ) {
        if !self.config.should_log(level) {
            return;
        }

        let record =
            StagehandLogRecord::new(message, level, category.map(|c| c.to_string()), auxiliary);

        if let Some(callback) = &self.config.external_logger {
            callback(&record);
        } else {
            (self.default_handler)(&record);
        }
    }

    pub fn error(
        &self,
        message: impl Into<String>,
        category: Option<&str>,
        auxiliary: Option<Value>,
    ) {
        self.log(message, LogLevel::Error, category, auxiliary);
    }

    pub fn info(
        &self,
        message: impl Into<String>,
        category: Option<&str>,
        auxiliary: Option<Value>,
    ) {
        self.log(message, LogLevel::Info, category, auxiliary);
    }

    pub fn debug(
        &self,
        message: impl Into<String>,
        category: Option<&str>,
        auxiliary: Option<Value>,
    ) {
        self.log(message, LogLevel::Debug, category, auxiliary);
    }
}

/// Helper to handle log payloads produced by the Stagehand API.
pub fn sync_log_handler(payload: &Value, logger: &StagehandLogger) {
    let message_obj = payload.get("message").unwrap_or(payload);
    let level_value = message_obj
        .get("level")
        .and_then(Value::as_i64)
        .unwrap_or(1);
    let message = message_obj
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let category = message_obj
        .get("category")
        .and_then(Value::as_str)
        .map(|s| s.to_string());
    let auxiliary = message_obj.get("auxiliary").cloned();

    logger.log(
        message.to_string(),
        LogLevel::from(level_value),
        category.as_deref(),
        auxiliary,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn respects_verbosity() {
        let logger = StagehandLogger::new(Verbosity::Minimal);
        assert!(logger.config.should_log(LogLevel::Error));
        assert!(!logger.config.should_log(LogLevel::Debug));
    }

    #[test]
    fn external_logger_is_invoked() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&records);
        let callback: LogCallback = Arc::new(move |record| {
            capture.lock().unwrap().push(record.clone());
        });

        let mut config = LogConfig::default();
        config.verbose = Verbosity::Detailed;
        config.external_logger = Some(callback);
        let logger = StagehandLogger::with_config(config);

        logger.info("hello", Some("test"), None);

        let values = records.lock().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].message, "hello");
        assert_eq!(values[0].category.as_deref(), Some("test"));
        assert_eq!(values[0].level, LogLevel::Info);
    }

    #[test]
    fn sync_log_handler_parses_payload() {
        let records = Arc::new(Mutex::new(Vec::new()));
        let capture = Arc::clone(&records);
        let callback: LogCallback = Arc::new(move |record| {
            capture.lock().unwrap().push(record.clone());
        });

        let mut logger = StagehandLogger::new(Verbosity::Detailed);
        logger.set_external_logger(Some(callback));

        let payload = serde_json::json!({
            "message": {
                "message": "Act completed",
                "level": 1,
                "category": "action",
                "auxiliary": { "success": true }
            }
        });

        sync_log_handler(&payload, &logger);

        let values = records.lock().unwrap();
        assert_eq!(values.len(), 1);
        assert_eq!(values[0].message, "Act completed");
        assert_eq!(values[0].category.as_deref(), Some("action"));
        assert_eq!(values[0].level, LogLevel::Info);
        assert_eq!(
            values[0].auxiliary.as_ref().unwrap(),
            &serde_json::json!({ "success": true })
        );
    }
}
