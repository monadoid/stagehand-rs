//! Run with:
//! `STAGEHAND_CHROME_BIN=/path/to/chrome cargo run --example local_highlight`
//!
//! The script opens example.com in a non-headless Chromium instance,
//! selects the first paragraph so the highlight is visible, prints the
//! paragraph text to stdout, waits a few seconds, and then closes the tab.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use stagehand_rs::config::{Environment, StagehandConfig, Verbosity};
use stagehand_rs::runtime::ChromiumoxideRuntime;
use stagehand_rs::stagehand::Stagehand;
use stagehand_rs::types::page::{ActOptions, ExtractOptions, ObserveOptions};
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    // Locate the Chrome/Chromium binary the example should launch.
    let chrome_bin = env::var("STAGEHAND_CHROME_BIN")
        .map_err(|_| anyhow!("STAGEHAND_CHROME_BIN must point to a Chrome/Chromium binary"))?;

    // Build a local-only configuration that keeps the browser visible.
    let mut config = StagehandConfig::default();
    config.env = Environment::Local;
    config.use_api = false; // rely on local handlers rather than the remote API
    config.headless = false;
    config.verbose = Verbosity::Detailed;
    config
        .local_browser_launch_options
        .insert("chromeExecutable".to_string(), Value::String(chrome_bin));

    // Provide the LLM key so local act/observe/extract calls can execute.
    let model_api_key = env::var("MODEL_API_KEY")
        .or_else(|_| env::var("OPENAI_API_KEY"))
        .map_err(|_| anyhow!("Set MODEL_API_KEY or OPENAI_API_KEY for local LLM calls"))?;
    config.model_api_key = Some(model_api_key.clone());
    let mut client_options = serde_json::Map::new();
    client_options.insert("apiKey".to_string(), Value::String(model_api_key));
    config.model_client_options = Some(client_options);

    let runtime = Arc::new(ChromiumoxideRuntime::new());
    let stagehand =
        Stagehand::new_local(config, runtime).context("failed to construct stagehand client")?;

    stagehand
        .init()
        .await
        .context("failed to initialise stagehand runtime")?;

    let page = stagehand
        .open_page("https://example.com")
        .await
        .context("failed to open example.com")?;

    // Ask Stagehand to observe the page and list relevant elements.
    let observe_options = ObserveOptions {
        instruction:
            "Identify the primary content on this page whose heading reads 'Example Domain'. \
            Return the heading along with the descriptive paragraph."
                .to_string(),
        model_name: None,
        draw_overlay: Some(false),
        dom_settle_timeout_ms: None,
        model_client_options: None,
    };
    let observations = page
        .observe(observe_options)
        .await
        .context("observe failed")?;

    if let Some(first) = observations.first() {
        println!(
            "First observed element: {} via {}",
            first.description,
            first
                .method
                .clone()
                .unwrap_or_else(|| "unknown".to_string())
        );
    }

    // Use extract to pull the main heading text.
    let extract_options = ExtractOptions {
        instruction: "Return the main heading text from this page as {\"heading\": \"...\"}"
            .to_string(),
        model_name: None,
        selector: None,
        schema_definition: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "heading": { "type": "string" }
            },
            "required": ["heading"]
        })),
        use_text_extract: None,
        dom_settle_timeout_ms: None,
        model_client_options: None,
    };
    let extract = page
        .extract(extract_options)
        .await
        .context("extract failed")?;
    println!("Extracted data: {}", extract.data.unwrap_or_default());

    // Ask Stagehand to select the primary paragraph.
    let act_options = ActOptions {
        action: "Select the main paragraph on this page so it is highlighted".to_string(),
        variables: None,
        model_name: None,
        dom_settle_timeout_ms: None,
        timeout_ms: None,
        model_client_options: None,
    };
    let act_result = page.act(act_options).await.context("act failed")?;
    println!("Act result: {}", act_result.message);

    // Keep the window open briefly so the highlight is visible.
    sleep(Duration::from_secs(5)).await;

    Ok(())
}
