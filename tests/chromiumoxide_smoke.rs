use std::env;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow};
use log::info;
use serde_json::Value;
use stagehand_rs::browser::BrowserRuntime;
use stagehand_rs::client::StagehandClient;
use stagehand_rs::config::{Environment, StagehandConfig};
use stagehand_rs::context::{StagehandAdapter, StagehandAdapterError};
use stagehand_rs::runtime::ChromiumoxideRuntime;

#[derive(Default)]
struct TrackingAdapter {
    injections: Mutex<Vec<String>>,
    active: Mutex<Vec<String>>,
    debug_logs: Mutex<Vec<String>>,
    error_logs: Mutex<Vec<String>>,
}

impl StagehandAdapter for TrackingAdapter {
    fn inject_dom_script(
        &self,
        page_id: &String,
        _script: &str,
    ) -> Result<(), StagehandAdapterError> {
        self.injections.lock().unwrap().push(page_id.clone());
        Ok(())
    }

    fn log_debug(&self, message: &str, _category: &'static str) {
        self.debug_logs.lock().unwrap().push(message.to_string());
    }

    fn log_error(&self, message: &str, _category: &'static str) {
        self.error_logs.lock().unwrap().push(message.to_string());
    }

    fn notify_active_page(&self, page_id: &String) {
        self.active.lock().unwrap().push(page_id.clone());
    }
}

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

    let runtime = ChromiumoxideRuntime::new();
    let adapter = Arc::new(TrackingAdapter::default());
    let client = StagehandClient::new(
        config,
        runtime,
        adapter.clone(),
        "window.__stagehand_injected = true;",
    )
    .context("failed to construct stagehand client")?;

    let page_id = client
        .open_page("https://example.com")
        .await
        .context("failed to open page via stagehand client")?;

    assert!(
        adapter.injections.lock().unwrap().contains(&page_id),
        "expected dom script injection for page"
    );
    assert!(
        adapter.active.lock().unwrap().contains(&page_id),
        "expected active page notification"
    );

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
