//! Integration tests that mirror the Python `tests/integration/local/test_core_local.py`.
//!
//! These are marked `#[ignore]` because they require:
//! - `STAGEHAND_CHROME_BIN` pointing to a Chrome/Chromium binary.
//! - `MODEL_API_KEY` (or `OPENAI_API_KEY`) for the local LLM calls.
//! Running them exercises the full `Stagehand` facade with real browser +
//! LLM endpoints, providing parity with the Python SDK's LOCAL mode suite.

use std::env;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use serde_json::Value as JsonValue;
use stagehand_rs::client::StagehandClientError;
use stagehand_rs::config::{Environment, StagehandConfig, Verbosity};
use stagehand_rs::runtime::ChromiumoxideRuntime;
use stagehand_rs::stagehand::Stagehand;
use stagehand_rs::types::page::{ActOptions, ExtractOptions, ObserveOptions};

/// Build a LOCAL configuration that mirrors the Python integration setup.
fn build_local_config() -> Result<StagehandConfig> {
    let chrome_bin = env::var("STAGEHAND_CHROME_BIN")
        .context("STAGEHAND_CHROME_BIN must point at a Chrome/Chromium executable")?;

    let model_api_key = env::var("MODEL_API_KEY")
        .or_else(|_| env::var("OPENAI_API_KEY"))
        .context("MODEL_API_KEY or OPENAI_API_KEY must be set for local LLM calls")?;

    let mut config = StagehandConfig::default();
    config.env = Environment::Local;
    config.use_api = false;
    config.headless = true;
    config.verbose = Verbosity::Medium;
    config.dom_settle_timeout_ms = Some(2_000);
    config.self_heal = true;
    config.wait_for_captcha_solves = false;
    config.system_prompt = Some(
        "You are a browser automation assistant for testing the Rust Stagehand port.".to_string(),
    );

    // Ensure the local browser launch mirrors the Python defaults.
    config.local_browser_launch_options.insert(
        "chromeExecutable".to_string(),
        JsonValue::String(chrome_bin),
    );

    // Use a dedicated temporary user-data directory for each test run to avoid
    // Chrome's process singleton lock.
    let user_data_temp = tempfile::Builder::new()
        .prefix("stagehand-rs-local-test")
        .tempdir()
        .context("failed to create temporary user data dir")?;
    let user_data_dir = user_data_temp.path().to_path_buf();
    std::mem::forget(user_data_temp);
    config.local_browser_launch_options.insert(
        "userDataDir".to_string(),
        JsonValue::String(path_to_string(&user_data_dir)),
    );

    config.model_api_key = Some(model_api_key.clone());
    let mut client_options = serde_json::Map::new();
    client_options.insert("apiKey".to_string(), JsonValue::String(model_api_key));
    config.model_client_options = Some(client_options);

    Ok(config)
}

/// Helper to construct and initialise a Stagehand instance for LOCAL testing.
async fn init_stagehand() -> Result<Stagehand<Arc<ChromiumoxideRuntime>>> {
    let config = build_local_config()?;
    let runtime = Arc::new(ChromiumoxideRuntime::new());
    let stagehand = Stagehand::new_local(config, runtime)
        .map_err(|err| anyhow!("failed to construct Stagehand: {err}"))?;
    stagehand
        .init()
        .await
        .map_err(|err| anyhow!("failed to initialise Stagehand: {err}"))?;
    Ok(stagehand)
}

fn observe_instruction(text: &str) -> ObserveOptions {
    ObserveOptions {
        instruction: text.to_string(),
        model_name: None,
        draw_overlay: Some(false),
        dom_settle_timeout_ms: None,
        model_client_options: None,
    }
}

fn act_instruction(text: &str) -> ActOptions {
    ActOptions {
        action: text.to_string(),
        variables: None,
        model_name: None,
        dom_settle_timeout_ms: None,
        timeout_ms: None,
        model_client_options: None,
    }
}

fn extract_instruction(text: &str) -> ExtractOptions {
    ExtractOptions {
        instruction: text.to_string(),
        model_name: None,
        selector: None,
        schema_definition: None,
        use_text_extract: None,
        dom_settle_timeout_ms: None,
        model_client_options: None,
    }
}

#[tokio::test]
#[ignore = "Requires Chrome + MODEL_API_KEY/OPENAI_API_KEY"]
#[serial_test::serial]
async fn local_stagehand_initialises() -> Result<()> {
    let stagehand = init_stagehand()
        .await
        .context("failed to initialise local Stagehand")?;

    // Without opening a page, requesting the active page should fail.
    match stagehand.page().await {
        Err(StagehandClientError::Unsupported(_)) => {}
        Err(other) => return Err(anyhow!("unexpected error: {other}")),
        Ok(_) => return Err(anyhow!("expected no active page")),
    }

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Chrome + MODEL_API_KEY/OPENAI_API_KEY"]
#[serial_test::serial]
async fn local_observe_and_act_workflow() -> Result<()> {
    let stagehand = init_stagehand()
        .await
        .context("failed to initialise local Stagehand")?;

    stagehand
        .open_page("https://httpbin.org/forms/post")
        .await
        .context("failed to open form page")?;
    let page = stagehand.page().await.context("no active page")?;

    let observations = page
        .observe(observe_instruction(
            "Find all form input elements and describe their purpose.",
        ))
        .await
        .context("observe failed")?;
    assert!(
        !observations.is_empty(),
        "expected to observe at least one form element"
    );

    page.act(act_instruction(
        "Fill the customer name field with 'Local Integration Test'",
    ))
    .await
    .context("act (customer name) failed")?;
    page.act(act_instruction(
        "Fill the telephone field with '555-LOCAL-RUST'",
    ))
    .await
    .context("act (telephone) failed")?;
    let act_result = page
        .act(act_instruction(
            "Fill the email field with 'local-rust-port@test.example'",
        ))
        .await
        .context("act (email) failed")?;
    assert!(
        act_result.success,
        "expected final act call to report success"
    );

    let filled_fields = page
        .observe(observe_instruction(
            "Find all form input fields that now contain text.",
        ))
        .await
        .context("observe for filled fields failed")?;
    assert!(
        !filled_fields.is_empty(),
        "expected to observe filled form fields"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Chrome + MODEL_API_KEY/OPENAI_API_KEY"]
#[serial_test::serial]
async fn local_basic_navigation_and_observe() -> Result<()> {
    let stagehand = init_stagehand()
        .await
        .context("failed to initialise local Stagehand")?;

    stagehand
        .open_page("https://example.com")
        .await
        .context("failed to open example.com")?;
    let page = stagehand.page().await.context("no active page")?;

    let observations = page
        .observe(observe_instruction(
            "List all links visible on the page along with short descriptions.",
        ))
        .await
        .context("observe failed")?;
    assert!(
        !observations.is_empty(),
        "expected to observe at least one link"
    );

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Chrome + MODEL_API_KEY/OPENAI_API_KEY"]
#[serial_test::serial]
async fn local_extraction_functionality() -> Result<()> {
    let stagehand = init_stagehand()
        .await
        .context("failed to initialise local Stagehand")?;

    stagehand
        .open_page("https://news.ycombinator.com")
        .await
        .context("failed to open news.ycombinator.com")?;
    let page = stagehand.page().await.context("no active page")?;

    let extract = page
        .extract(extract_instruction(
            "Extract the titles of the first 3 visible posts as {\"articles\": [\"...\"]}",
        ))
        .await
        .context("extract failed")?;

    let data = extract
        .data
        .context("expected extract response to include data")?;
    assert!(
        data.is_object(),
        "expected extract payload to be a JSON object but got {data}"
    );

    Ok(())
}

fn path_to_string(path: &PathBuf) -> String {
    path.to_string_lossy().into_owned()
}
