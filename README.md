# Stagehand Rust Quickstart

This crate contains the in-progress Rust port of the Stagehand automation SDK.
It already supports the core observe / act / extract flows for both local
Chromiumoxide sessions and Browserbase-backed remote sessions. The examples and
CLI mirror the Python quickstart so you can exercise the port end-to-end.

## Prerequisites

- Rust 1.78 or newer
- A Chrome/Chromium binary (for local runs) – point `STAGEHAND_CHROME_BIN` at it
- LLM credentials exposed via `MODEL_API_KEY` (or `OPENAI_API_KEY`)
- For Browserbase runs: `BROWSERBASE_API_KEY` and `BROWSERBASE_PROJECT_ID`

Install dependencies once:

```bash
cargo fetch
```

## CLI Usage

The `stagehand` binary reproduces the quickstart workflow, opening
`https://www.aigrant.com`, extracting company data, observing the Browserbase
link, and clicking it.

### Browserbase (remote) workflow

```bash
BROWSERBASE_API_KEY=... \
BROWSERBASE_PROJECT_ID=... \
MODEL_API_KEY=... \
cargo run --bin stagehand -- quickstart --mode browserbase
```

### Local Chromiumoxide workflow

```bash
STAGEHAND_CHROME_BIN=/Applications/Google\ Chrome.app/Contents/MacOS/Google\ Chrome \
MODEL_API_KEY=... \
cargo run --bin stagehand -- quickstart --mode local --show-browser
```

Use `--verbose` (repeat for more detail) to surface Stagehand’s structured
logging through `env_logger`.

## Examples

- `browserbase_quickstart.rs` – mirrors the Python quickstart using the remote
  API and Server-Sent Events:

  ```bash
  BROWSERBASE_API_KEY=... BROWSERBASE_PROJECT_ID=... MODEL_API_KEY=... \
  cargo run --example browserbase_quickstart
  ```

- `local_highlight.rs` – launches a local Chrome instance, highlights the main
  paragraph on example.com, and keeps the window open briefly:

  ```bash
  STAGEHAND_CHROME_BIN=/path/to/chrome MODEL_API_KEY=... \
  cargo run --example local_highlight
  ```

## Observability

Stagehand’s logger forwards structured events through an optional callback.
The CLI examples set `StagehandConfig.logger` so every Stagehand event is
routed through the standard `log` facade.  You can integrate the runtime into
your own telemetry stack by providing an `Arc<dyn Fn(&str)>` callback:

```rust
let mut config = StagehandConfig::default();
config.logger = Some(Arc::new(|line| tracing::info!("{line}")));
```

Set `RUST_LOG=stagehand_rs=debug` (or toggle the `--verbose` flag in the CLI)
to surface the detailed diagnostics during observe/act/extract flows.
