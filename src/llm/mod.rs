//! Language model client abstractions for the Stagehand Rust port.
//!
//! This module houses the provider-agnostic client interface along with an
//! OpenAI-backed implementation powered by the `async-openai` crate.

pub mod client;
pub mod error;
pub mod openai;
pub mod prompts;
pub mod provider;

pub use client::{ChatCompletionOptions, MetricsCallback, StagehandLlmClient};
pub use error::StagehandLlmError;
pub use openai::OpenAiChatProvider;
pub use provider::ChatCompletionProvider;
