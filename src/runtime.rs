//! Chromiumoxide-based browser runtime.
//!
//! Provides an implementation of [`BrowserRuntime`](crate::browser::BrowserRuntime)
//! backed by the `chromiumoxide` crate. The current implementation focuses on
//! the local launch flow so the Stagehand client can exercise an end-to-end
//! path while we incrementally add Browserbase support.

use std::sync::Mutex;

use async_trait::async_trait;
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures_util::StreamExt;
use tokio::{fs, task::JoinHandle};

use crate::browser::{
    BrowserRuntime, BrowserRuntimeError, BrowserbasePlan, BrowserbaseSessionStrategy,
    LocalLaunchStrategy, LocalPlan,
};

#[derive(Default)]
pub struct ChromiumoxideRuntime {
    state: Mutex<Option<RuntimeState>>,
}

struct RuntimeState {
    _browser: Browser,
    _handler: JoinHandle<()>,
}

impl ChromiumoxideRuntime {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }
}

#[async_trait]
impl BrowserRuntime for ChromiumoxideRuntime {
    async fn connect_browserbase(&self, plan: &BrowserbasePlan) -> Result<(), BrowserRuntimeError> {
        match plan.strategy {
            BrowserbaseSessionStrategy::UseExisting { .. }
            | BrowserbaseSessionStrategy::CreateNew { .. } => Err(BrowserRuntimeError::Message(
                "Browserbase runtime integration not implemented yet".into(),
            )),
        }
    }

    async fn launch_local(&self, plan: &LocalPlan) -> Result<(), BrowserRuntimeError> {
        if self.state.lock().unwrap().is_some() {
            return Ok(());
        }

        match &plan.strategy {
            LocalLaunchStrategy::AttachCdp { url, .. } => {
                attach_to_cdp(&self.state, url).await?;
            }
            LocalLaunchStrategy::LaunchPersistent { .. } => {
                launch_persistent(&self.state, plan).await?;
            }
        }

        Ok(())
    }
}

fn build_config(plan: &LocalPlan) -> Result<BrowserConfig, BrowserRuntimeError> {
    let launch = &plan.launch_options;

    let viewport = chromiumoxide::handler::viewport::Viewport {
        width: launch.viewport.width,
        height: launch.viewport.height,
        device_scale_factor: None,
        emulating_mobile: false,
        is_landscape: launch.viewport.width >= launch.viewport.height,
        has_touch: false,
    };

    let builder = BrowserConfig::builder()
        .viewport(viewport)
        .args(launch.args.clone());

    let builder = if launch.headless {
        builder
    } else {
        builder.with_head()
    };

    let builder = if !launch.ignore_https_errors {
        builder.respect_https_errors()
    } else {
        builder
    };

    let builder = match &plan.strategy {
        LocalLaunchStrategy::AttachCdp { .. } => builder,
        LocalLaunchStrategy::LaunchPersistent { user_data_dir } => match user_data_dir {
            Some(dir) => builder.user_data_dir(dir),
            None => builder,
        },
    };

    let builder = if !launch.locale.is_empty() {
        builder.arg(format!("--lang={}", launch.locale))
    } else {
        builder
    };

    let builder = if !launch.timezone_id.is_empty() {
        builder.arg(format!("--timezone={}", launch.timezone_id))
    } else {
        builder
    };

    builder
        .build()
        .map_err(|err| BrowserRuntimeError::Message(err))
}

fn map_chromiumoxide_error<E: std::fmt::Display>(err: E) -> BrowserRuntimeError {
    BrowserRuntimeError::Message(err.to_string())
}

async fn attach_to_cdp(
    state: &Mutex<Option<RuntimeState>>,
    url: &str,
) -> Result<(), BrowserRuntimeError> {
    let (browser, mut handler) = Browser::connect(url)
        .await
        .map_err(map_chromiumoxide_error)?;

    let join = tokio::spawn(async move {
        while let Some(result) = handler.next().await {
            if let Err(err) = result {
                eprintln!("chromiumoxide handler error: {err}");
            }
        }
    });

    *state.lock().unwrap() = Some(RuntimeState {
        _browser: browser,
        _handler: join,
    });

    Ok(())
}

async fn launch_persistent(
    state: &Mutex<Option<RuntimeState>>,
    plan: &LocalPlan,
) -> Result<(), BrowserRuntimeError> {
    let config = build_config(plan)?;

    if let Some(parent) = plan.downloads_path.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|err| BrowserRuntimeError::Message(err.to_string()))?;
    }

    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(map_chromiumoxide_error)?;

    let join = tokio::spawn(async move {
        while let Some(result) = handler.next().await {
            if let Err(err) = result {
                eprintln!("chromiumoxide handler error: {err}");
            }
        }
    });

    *state.lock().unwrap() = Some(RuntimeState {
        _browser: browser,
        _handler: join,
    });

    Ok(())
}
