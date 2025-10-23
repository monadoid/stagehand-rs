use thiserror::Error;

use async_openai::error::OpenAIError;

/// Errors surfaced by the Stagehand LLM client layer.
#[derive(Debug, Error)]
pub enum StagehandLlmError {
    #[error("missing OpenAI API key; set MODEL_API_KEY or OPENAI_API_KEY")]
    MissingApiKey,
    #[error("missing default model configuration")]
    MissingDefaultModel,
    #[error("invalid chat completion request: {0}")]
    InvalidRequest(String),
    #[error(transparent)]
    OpenAi(#[from] OpenAIError),
}
