use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use chromiumoxide::{
    browser::{Browser, BrowserConfig},
    error::{CdpError, Result as OxideResult},
};
use futures_util::StreamExt;
use log::info;

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

    let config = BrowserConfig::builder()
        .chrome_executable(&chrome_bin)
        .build()
        .map_err(|err| anyhow!("failed to build chromium config: {err}"))?;

    let (mut browser, mut handler) = Browser::launch(config)
        .await
        .context("failed to launch chromium")?;

    let handler_task: tokio::task::JoinHandle<OxideResult<()>> = tokio::spawn(async move {
        while let Some(result) = handler.next().await {
            match result {
                Ok(_) => {}
                Err(CdpError::ChannelSendError(_)) | Err(CdpError::NoResponse) => break,
                Err(err) => return Err(err),
            }
        }
        Ok(())
    });

    let page = browser
        .new_page("https://example.com")
        .await
        .context("failed to open new page")?;

    let page = page
        .wait_for_navigation()
        .await
        .context("navigation to example.com failed")?;

    let html = page
        .content()
        .await
        .context("failed to fetch page content")?;
    assert!(
        html.contains("Example Domain"),
        "expected Example Domain in page content"
    );
    info!("Fetched page content ({} bytes)", html.len());

    let heading = page
        .find_element("h1")
        .await
        .context("failed to find h1 element")?
        .inner_text()
        .await
        .context("failed to read heading text")?
        .unwrap_or_default();
    assert_eq!(heading.trim(), "Example Domain");
    info!("Heading text: {}", heading.trim());

    browser
        .close()
        .await
        .context("failed to close chromium instance")?;

    match handler_task.await {
        Ok(Ok(())) => {}
        Ok(Err(err)) if matches!(err, CdpError::ChannelSendError(_) | CdpError::NoResponse) => {
            info!("handler finished with benign error: {err:?}");
        }
        Ok(Err(err)) => return Err(err.into()),
        Err(join_err) => return Err(join_err.into()),
    }

    Ok(())
}
