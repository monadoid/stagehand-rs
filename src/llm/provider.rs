use async_trait::async_trait;

use async_openai::error::OpenAIError;
use async_openai::types::{CreateChatCompletionRequest, CreateChatCompletionResponse};

/// Abstraction over chat completion providers so the Stagehand client can be
/// tested without performing real HTTP requests.
#[async_trait]
pub trait ChatCompletionProvider: Send + Sync {
    async fn create_chat_completion(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, OpenAIError>;
}
