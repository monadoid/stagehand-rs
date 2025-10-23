use std::env;

use async_openai::error::OpenAIError;
use async_openai::types::{CreateChatCompletionRequest, CreateChatCompletionResponse};
use async_openai::{Client, config::OpenAIConfig};
use async_trait::async_trait;
use serde_json::Value;

use crate::config::StagehandConfig;

use super::error::StagehandLlmError;
use super::provider::ChatCompletionProvider;

/// Implementation of [`ChatCompletionProvider`] backed by OpenAI-compatible APIs.
#[derive(Clone, Debug)]
pub struct OpenAiChatProvider {
    client: Client<OpenAIConfig>,
}

impl OpenAiChatProvider {
    /// Wrap an existing `async-openai` client instance.
    pub fn new(client: Client<OpenAIConfig>) -> Self {
        Self { client }
    }

    /// Construct an OpenAI client using Stagehand configuration values.
    pub fn from_config(config: &StagehandConfig) -> Result<Self, StagehandLlmError> {
        let api_key = config
            .model_api_key
            .clone()
            .or_else(|| env::var("MODEL_API_KEY").ok())
            .or_else(|| env::var("OPENAI_API_KEY").ok())
            .ok_or(StagehandLlmError::MissingApiKey)?;

        let mut openai_config = OpenAIConfig::new().with_api_key(api_key);

        if let Some(options) = config.model_client_options.as_ref() {
            if let Some(api_base) =
                extract_string(options, &["api_base", "apiBase", "base_url", "baseURL"])
            {
                openai_config = openai_config.with_api_base(api_base);
            }

            if let Some(org_id) = extract_string(options, &["organization", "org_id", "orgId"]) {
                openai_config = openai_config.with_org_id(org_id);
            }

            if let Some(project_id) =
                extract_string(options, &["project", "project_id", "projectId"])
            {
                openai_config = openai_config.with_project_id(project_id);
            }
        }

        Ok(Self::new(Client::with_config(openai_config)))
    }
}

#[async_trait]
impl ChatCompletionProvider for OpenAiChatProvider {
    async fn create_chat_completion(
        &self,
        request: CreateChatCompletionRequest,
    ) -> Result<CreateChatCompletionResponse, OpenAIError> {
        self.client.chat().create(request).await
    }
}

fn extract_string(options: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| options.get(*key).and_then(Value::as_str))
        .map(|value| value.to_string())
}
