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
use stagehand_rs::client::StagehandClient;
use stagehand_rs::config::{Environment, StagehandConfig, Verbosity};
use stagehand_rs::runtime::ChromiumoxideRuntime;
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

    let runtime = Arc::new(ChromiumoxideRuntime::new());
    let client = StagehandClient::with_chromiumoxide_runtime(config, runtime.clone())
        .context("failed to construct stagehand client")?;

    client
        .ensure_initialized()
        .await
        .context("failed to initialise runtime")?;

    let page_id = client
        .open_page("https://example.com")
        .await
        .context("failed to open example.com")?;

    // Highlight the first paragraph so the selection is visible in the launched window.
    let page = client.page(page_id.clone());
    let select_script = r#"
        (function() {
            const el = document.querySelector('p');
            if (!el) {
                return null;
            }
            const range = document.createRange();
            range.selectNodeContents(el);
            const selection = window.getSelection();
            selection.removeAllRanges();
            selection.addRange(range);
            return el.textContent || null;
        })()
    "#;

    let result = page
        .evaluate_expression(select_script)
        .await
        .context("failed to evaluate selection script")?;
    let paragraph = result
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| "<paragraph not found>".to_string());

    println!("\nSelected paragraph:\n{}\n", paragraph.trim());

    // Keep the window open briefly so the highlight is visible.
    sleep(Duration::from_secs(5)).await;

    // Close the tab before exiting.
    let _ = page
        .evaluate_expression("window.close(); true")
        .await
        .context("failed to close the tab");

    Ok(())
}
