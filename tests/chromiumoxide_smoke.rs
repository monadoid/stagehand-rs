use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use log::info;
use serde_json::Value;
use stagehand_rs::browser::BrowserRuntime;
use stagehand_rs::client::StagehandClient;
use stagehand_rs::config::{Environment, StagehandConfig};
use stagehand_rs::runtime::ChromiumoxideRuntime;

#[tokio::test]
async fn chromiumoxide_launches_and_executes_cdp() -> Result<()> {
    let _ = env_logger::builder().is_test(true).try_init();

    let chrome_bin = match env::var("STAGEHAND_CHROME_BIN") {
        Ok(value) if !value.trim().is_empty() => PathBuf::from(value),
        _ => {
            eprintln!("skipping chromiumoxide integration test: STAGEHAND_CHROME_BIN not set");
            return Ok(());
        }
    };

    if !chrome_bin.exists() {
        eprintln!(
            "skipping chromiumoxide integration test: chrome executable not found at {}",
            chrome_bin.display()
        );
        return Ok(());
    }

    let mut config = StagehandConfig::default();
    config.env = Environment::Local;
    config.headless = true;
    config.local_browser_launch_options.insert(
        "chromeExecutable".into(),
        Value::String(chrome_bin.to_string_lossy().into()),
    );

    let runtime = Arc::new(ChromiumoxideRuntime::new());
    let client = StagehandClient::with_chromiumoxide_runtime(config, runtime.clone())
        .context("failed to construct stagehand client")?;

    let page_id = client
        .open_page("https://example.com")
        .await
        .context("failed to open page via stagehand client")?;

    let page = runtime
        .page(&page_id)
        .await
        .context("runtime error retrieving page handle")?
        .ok_or_else(|| anyhow!("runtime returned no handle for page {page_id}"))?;

    let injected: bool = page
        .evaluate("() => typeof window.getScrollableElementXpaths === 'function'")
        .await
        .context("failed to evaluate injection check")?
        .into_value()
        .map_err(|err| anyhow!(err.to_string()))?;
    assert!(injected, "expected domScripts.js helpers to be available");

    let content = client
        .browser()
        .runtime()
        .page_content(&page_id)
        .await
        .context("runtime error fetching page content")?
        .ok_or_else(|| anyhow!("runtime returned no content for page {page_id}"))?;

    info!("Fetched page content ({} bytes)", content.len());
    assert!(
        content.contains("Example Domain"),
        "expected Example Domain in page content"
    );
    if let Some(start) = content.find("<h1>") {
        if let Some(end) = content[start..].find("</h1>") {
            let heading = &content[start + 4..start + end];
            info!("Heading text: {}", heading.trim());
            assert_eq!(heading.trim(), "Example Domain");
        }
    }

    Ok(())
}
