//! Stagehand Rust CLI.
//!
//! This binary provides a quickstart workflow that mirrors the Python
//! examples: it initialises a Stagehand client, opens a page, performs a
//! structured extract, issues an observe call, and executes a follow-up act.
//!
//! Usage examples:
//!   Browserbase (remote):
//!     $ BROWSERBASE_API_KEY=... BROWSERBASE_PROJECT_ID=... MODEL_API_KEY=... \
//!       cargo run --bin stagehand -- quickstart --mode browserbase
//!   Local (Chromiumoxide):
//!     $ STAGEHAND_CHROME_BIN=/path/to/chrome MODEL_API_KEY=... \
//!       cargo run --bin stagehand -- quickstart --mode local --show-browser

use std::collections::HashMap;
use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};
use log::{info, warn};
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use stagehand_rs::config::{Environment, LoggerCallback, ModelName, StagehandConfig, Verbosity};
use stagehand_rs::runtime::ChromiumoxideRuntime;
use stagehand_rs::stagehand::Stagehand;
use stagehand_rs::types::page::{ActOptions, ExtractOptions, ObserveOptions};
use tokio::time::sleep;

#[derive(Parser)]
#[command(
    name = "stagehand",
    author,
    version,
    about = "Stagehand Rust CLI utilities"
)]
struct Cli {
    /// Increase log verbosity (pass multiple times for DEBUG).
    #[arg(long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the quickstart workflow (extract, observe, act).
    Quickstart(QuickstartArgs),
}

#[derive(Clone, Copy, ValueEnum, Debug)]
enum Mode {
    Browserbase,
    Local,
}

#[derive(Args)]
struct QuickstartArgs {
    /// Target environment for the workflow.
    #[arg(long, value_enum, default_value_t = Mode::Browserbase)]
    mode: Mode,

    /// Page URL to open before running extract/observe/act.
    #[arg(long, default_value = "https://www.aigrant.com")]
    url: String,

    /// Override the extract instruction.
    #[arg(
        long,
        default_value = "Extract names and descriptions of 5 companies in batch 3"
    )]
    extract_instruction: String,

    /// Override the observe instruction.
    #[arg(
        long,
        default_value = "Find the link to the company Browserbase and describe it."
    )]
    observe_instruction: String,

    /// Override the act instruction.
    #[arg(long, default_value = "Click the link to the company Browserbase.")]
    act_instruction: String,

    /// Keep the browser window open for N seconds after acting (local mode only).
    #[arg(long, default_value_t = 3)]
    dwell_seconds: u64,

    /// Show the launched browser window (local mode only).
    #[arg(long)]
    show_browser: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_env_logger();

    let cli = Cli::parse();
    let verbosity = verbosity_from_count(cli.verbose);

    match cli.command {
        Command::Quickstart(args) => {
            run_quickstart(args, verbosity).await?;
        }
    }

    Ok(())
}

async fn run_quickstart(args: QuickstartArgs, verbosity: Verbosity) -> Result<()> {
    info!("Starting Stagehand quickstart in {:?} mode", args.mode);

    let logger_callback = make_logger_callback();
    let config = match args.mode {
        Mode::Browserbase => build_browserbase_config(verbosity, logger_callback.clone())?,
        Mode::Local => build_local_config(&args, verbosity, logger_callback.clone())?,
    };

    let runtime = Arc::new(ChromiumoxideRuntime::new());
    let client =
        stagehand_rs::client::StagehandClient::with_chromiumoxide_runtime(config, runtime.clone())
            .context("failed to construct Stagehand client")?;
    let stagehand = Stagehand::from_client(client);

    stagehand
        .init()
        .await
        .context("failed to initialise Stagehand runtime")?;

    let page = stagehand
        .open_page(&args.url)
        .await
        .with_context(|| format!("failed to open {}", args.url))?;
    info!("Opened {}", args.url);

    let companies = page
        .extract(ExtractOptions {
            instruction: args.extract_instruction.clone(),
            model_name: Some(ModelName::Gpt4o.as_str().to_string()),
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

    if let Some(data) = companies.data.as_ref() {
        info!("Extraction result: {}", data);
    } else {
        warn!("Extraction returned no data");
    }

    let observations = page
        .observe(ObserveOptions {
            instruction: args.observe_instruction.clone(),
            model_name: Some(ModelName::Gpt4o.as_str().to_string()),
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

    let act_result = page
        .act(ActOptions {
            action: args.act_instruction.clone(),
            variables: None,
            model_name: Some(ModelName::Gpt4o.as_str().to_string()),
            dom_settle_timeout_ms: None,
            timeout_ms: None,
            model_client_options: None,
        })
        .await
        .context("act failed")?;

    info!("Act result: {}", act_result.message);

    if matches!(args.mode, Mode::Local) && args.show_browser {
        sleep(Duration::from_secs(args.dwell_seconds)).await;
    }

    runtime
        .shutdown()
        .await
        .context("failed to shutdown Chromiumoxide runtime")?;

    info!("Quickstart completed");
    Ok(())
}

fn build_browserbase_config(
    verbosity: Verbosity,
    logger: LoggerCallback,
) -> Result<StagehandConfig> {
    let mut config = StagehandConfig::default();
    config.verbose = verbosity;
    config.use_rich_logging = false;
    config.logger = Some(logger);
    config.env = Environment::Browserbase;
    config.api_key = Some(env_var("BROWSERBASE_API_KEY")?);
    config.project_id = Some(env_var("BROWSERBASE_PROJECT_ID")?);
    config.model_api_key = Some(resolve_model_key()?);
    config.model_client_options = Some(api_key_client_options(
        config.model_api_key.as_ref().unwrap(),
    ));

    if let Ok(session_id) = env::var("BROWSERBASE_SESSION_ID") {
        if !session_id.trim().is_empty() {
            config.browserbase_session_id = Some(session_id);
        }
    }

    Ok(config)
}

fn build_local_config(
    args: &QuickstartArgs,
    verbosity: Verbosity,
    logger: LoggerCallback,
) -> Result<StagehandConfig> {
    let chrome_bin = env_var("STAGEHAND_CHROME_BIN")?;
    let mut config = StagehandConfig::default();
    config.verbose = verbosity;
    config.use_rich_logging = false;
    config.logger = Some(logger);
    config.env = Environment::Local;
    config.use_api = false;
    config.headless = !args.show_browser;

    config.model_api_key = Some(resolve_model_key()?);
    config.model_client_options = Some(api_key_client_options(
        config.model_api_key.as_ref().unwrap(),
    ));

    config
        .local_browser_launch_options
        .insert("chromeExecutable".into(), JsonValue::String(chrome_bin));
    config
        .local_browser_launch_options
        .insert("headless".into(), JsonValue::Bool(!args.show_browser));

    Ok(config)
}

fn api_key_client_options(api_key: &str) -> JsonMap<String, JsonValue> {
    let mut options = HashMap::new();
    options.insert("apiKey".to_string(), JsonValue::String(api_key.to_string()));
    options.into_iter().collect()
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
    Arc::new(|line: &str| {
        log::info!("{line}");
    })
}

fn verbosity_from_count(count: u8) -> Verbosity {
    match count {
        0 => Verbosity::Medium,
        1 => Verbosity::Detailed,
        _ => Verbosity::Detailed,
    }
}

fn init_env_logger() {
    if env::var("RUST_LOG").is_err() {
        unsafe {
            env::set_var("RUST_LOG", "info");
        }
    }

    let _ = env_logger::Builder::from_env(env_logger::Env::default())
        .format_timestamp_secs()
        .try_init();
}
