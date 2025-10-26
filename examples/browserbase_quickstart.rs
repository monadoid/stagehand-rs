//! Browserbase quickstart example.
//!
//! Mirrors the Python quickstart by opening https://www.aigrant.com,
//! extracting structured company data, observing a link, and clicking it.
//! Requires the following environment variables:
//!   - BROWSERBASE_API_KEY
//!   - BROWSERBASE_PROJECT_ID
//!   - MODEL_API_KEY (or OPENAI_API_KEY)
//!
//! Run with:
//!   $ BROWSERBASE_API_KEY=... BROWSERBASE_PROJECT_ID=... MODEL_API_KEY=... \
//!       cargo run --example browserbase_quickstart

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use log::{info, warn};
use serde_json::json;
use stagehand_rs::client::StagehandClient;
use stagehand_rs::config::{LoggerCallback, StagehandConfig, Verbosity};
use stagehand_rs::runtime::ChromiumoxideRuntime;
use stagehand_rs::stagehand::Stagehand;
use stagehand_rs::types::page::{ActOptions, ExtractOptions, ObserveOptions};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    let mut config = StagehandConfig::default();
    config.use_rich_logging = false;
    config.verbose = Verbosity::Detailed;
    config.logger = Some(make_logger_callback());
    config.api_key = Some(env_var("BROWSERBASE_API_KEY")?);
    config.project_id = Some(env_var("BROWSERBASE_PROJECT_ID")?);
    config.model_api_key = Some(resolve_model_key()?);
    config.model_client_options = Some(api_key_client_options(
        config.model_api_key.as_ref().unwrap(),
    ));

    let runtime = Arc::new(ChromiumoxideRuntime::new());
    let client = StagehandClient::with_chromiumoxide_runtime(config, runtime.clone())
        .context("failed to construct stagehand client")?;
    let stagehand = Stagehand::from_client(client);

    stagehand
        .init()
        .await
        .context("failed to initialise stagehand runtime")?;

    let page = stagehand
        .open_page("https://www.aigrant.com")
        .await
        .context("failed to open https://www.aigrant.com")?;

    let extract = page
        .extract(ExtractOptions {
            instruction: "Extract names and descriptions of 5 companies in batch 3".to_string(),
            model_name: None,
            selector: None,
            schema_definition: Some(json!({
                "type": "object",
                "properties": {
                    "companies": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "name": { "type": "string" },
                                "description": { "type": "string" }
                            },
                            "required": ["name", "description"]
                        }
                    }
                },
                "required": ["companies"]
            })),
            use_text_extract: None,
            dom_settle_timeout_ms: None,
            model_client_options: None,
        })
        .await
        .context("extract failed")?;

    if let Some(data) = extract.data {
        info!("Extraction result: {}", data);
    } else {
        warn!("Extraction returned no data");
    }

    let observations = page
        .observe(ObserveOptions {
            instruction: "Find the link to the company Browserbase and describe it.".to_string(),
            model_name: None,
            draw_overlay: Some(false),
            dom_settle_timeout_ms: None,
            model_client_options: None,
        })
        .await
        .context("observe failed")?;

    if let Some(first) = observations.first() {
        info!(
            "Observe result: {} via {}",
            first.description,
            first
                .method
                .clone()
                .unwrap_or_else(|| "unknown".to_string())
        );
    } else {
        warn!("Observe did not return any elements");
    }

    let act = page
        .act(ActOptions {
            action: "Click the link to the company Browserbase.".to_string(),
            variables: None,
            model_name: None,
            dom_settle_timeout_ms: None,
            timeout_ms: None,
            model_client_options: None,
        })
        .await
        .context("act failed")?;

    info!("Act result: {}", act.message);

    // Let the navigation settle briefly before shutting down.
    sleep(Duration::from_secs(2)).await;

    runtime
        .shutdown()
        .await
        .context("failed to shutdown runtime")?;
    Ok(())
}

fn api_key_client_options(api_key: &str) -> serde_json::Map<String, serde_json::Value> {
    let mut map = HashMap::new();
    map.insert(
        "apiKey".to_string(),
        serde_json::Value::String(api_key.to_string()),
    );
    map.into_iter().collect()
}

fn resolve_model_key() -> Result<String> {
    env::var("MODEL_API_KEY")
        .or_else(|_| env::var("OPENAI_API_KEY"))
        .map_err(|_| anyhow!("MODEL_API_KEY or OPENAI_API_KEY must be set"))
}

fn env_var(name: &str) -> Result<String> {
    let value =
        env::var(name).with_context(|| format!("{name} environment variable must be set"))?;
    if value.trim().is_empty() {
        return Err(anyhow!("{name} environment variable cannot be empty"));
    }
    Ok(value)
}

fn make_logger_callback() -> LoggerCallback {
    Arc::new(|line: &str| log::info!("{line}"))
}

fn init_logging() {
    if env::var("RUST_LOG").is_err() {
        unsafe {
            env::set_var("RUST_LOG", "info");
        }
    }
    let _ = env_logger::Builder::from_env(env_logger::Env::default())
        .format_timestamp_secs()
        .try_init();
}
