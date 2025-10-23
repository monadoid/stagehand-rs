use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[allow(deprecated)]
use async_openai::types::ChatCompletionFunctions;
use async_openai::types::{
    ChatCompletionAudio, ChatCompletionFunctionCall, ChatCompletionModalities,
    ChatCompletionRequestMessage, ChatCompletionStreamOptions, ChatCompletionTool,
    ChatCompletionToolChoiceOption, CreateChatCompletionRequest, CreateChatCompletionRequestArgs,
    CreateChatCompletionResponse, PredictionContent, ReasoningEffort, ResponseFormat, ServiceTier,
    Stop, WebSearchOptions,
};

use crate::config::{LoggerCallback, StagehandConfig};

use super::error::StagehandLlmError;
use super::openai::OpenAiChatProvider;
use super::provider::ChatCompletionProvider;

/// Callback invoked after a successful completion to capture metrics.
pub type MetricsCallback =
    Arc<dyn Fn(&CreateChatCompletionResponse, Duration, Option<&str>) + Send + Sync + 'static>;

/// Optional parameters that influence chat completion requests.
#[derive(Debug, Default, Clone)]
pub struct ChatCompletionOptions {
    pub model: Option<String>,
    pub store: Option<bool>,
    pub reasoning_effort: Option<ReasoningEffort>,
    pub metadata: Option<serde_json::Value>,
    pub frequency_penalty: Option<f32>,
    pub logit_bias: Option<HashMap<String, serde_json::Value>>,
    pub logprobs: Option<bool>,
    pub top_logprobs: Option<u8>,
    pub max_tokens: Option<u32>,
    pub max_completion_tokens: Option<u32>,
    pub n: Option<u8>,
    pub modalities: Option<Vec<ChatCompletionModalities>>,
    pub prediction: Option<PredictionContent>,
    pub audio: Option<ChatCompletionAudio>,
    pub presence_penalty: Option<f32>,
    pub response_format: Option<ResponseFormat>,
    pub seed: Option<i64>,
    pub service_tier: Option<ServiceTier>,
    pub stop: Option<Stop>,
    pub stream: Option<bool>,
    pub stream_options: Option<ChatCompletionStreamOptions>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub tools: Option<Vec<ChatCompletionTool>>,
    pub tool_choice: Option<ChatCompletionToolChoiceOption>,
    pub parallel_tool_calls: Option<bool>,
    pub user: Option<String>,
    pub web_search_options: Option<WebSearchOptions>,
    pub function_call: Option<ChatCompletionFunctionCall>,
    #[allow(deprecated)]
    pub functions: Option<Vec<ChatCompletionFunctions>>,
}

/// Provider-neutral Stagehand LLM client.
pub struct StagehandLlmClient<P: ChatCompletionProvider> {
    provider: P,
    default_model: String,
    logger: Option<LoggerCallback>,
    metrics_callback: Option<MetricsCallback>,
}

impl<P> fmt::Debug for StagehandLlmClient<P>
where
    P: ChatCompletionProvider + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StagehandLlmClient")
            .field("provider", &self.provider)
            .field("default_model", &self.default_model)
            .field("logger_attached", &self.logger.is_some())
            .field("metrics_callback", &self.metrics_callback.is_some())
            .finish()
    }
}

impl<P: ChatCompletionProvider> StagehandLlmClient<P> {
    /// Create a new Stagehand LLM client with the supplied provider and default model.
    pub fn new(default_model: impl Into<String>, provider: P) -> Self {
        Self {
            provider,
            default_model: default_model.into(),
            logger: None,
            metrics_callback: None,
        }
    }

    /// Attach a logger callback that mirrors the Python client's logging hooks.
    pub fn with_logger(mut self, logger: Option<LoggerCallback>) -> Self {
        self.logger = logger;
        self
    }

    /// Attach a metrics callback invoked after successful completions.
    pub fn with_metrics_callback(mut self, callback: Option<MetricsCallback>) -> Self {
        self.metrics_callback = callback;
        self
    }

    /// Update the logger callback.
    pub fn set_logger(&mut self, logger: Option<LoggerCallback>) {
        self.logger = logger;
    }

    /// Update the metrics callback.
    pub fn set_metrics_callback(&mut self, callback: Option<MetricsCallback>) {
        self.metrics_callback = callback;
    }

    /// Access the default model configured for this client.
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    /// Access the underlying provider (primarily for testing).
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Construct an [`async_openai`] chat completion request using the provided messages and options.
    pub fn build_request(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        options: ChatCompletionOptions,
    ) -> Result<CreateChatCompletionRequest, StagehandLlmError> {
        let model = options
            .model
            .clone()
            .unwrap_or_else(|| self.default_model.clone());

        if model.trim().is_empty() {
            return Err(StagehandLlmError::MissingDefaultModel);
        }

        let mut builder = CreateChatCompletionRequestArgs::default();
        builder.model(model);
        builder.messages(messages);
        apply_options(&mut builder, options);

        builder
            .build()
            .map_err(|err| StagehandLlmError::InvalidRequest(err.to_string()))
    }

    /// Create a chat completion using Stagehand-flavoured options.
    pub async fn create_chat_completion(
        &self,
        messages: Vec<ChatCompletionRequestMessage>,
        options: ChatCompletionOptions,
        function_name: Option<&str>,
    ) -> Result<CreateChatCompletionResponse, StagehandLlmError> {
        let request = self.build_request(messages, options)?;
        self.execute_request(request, function_name).await
    }

    /// Create a chat completion with a fully-formed OpenAI request.
    pub async fn create_with_request(
        &self,
        mut request: CreateChatCompletionRequest,
        function_name: Option<&str>,
    ) -> Result<CreateChatCompletionResponse, StagehandLlmError> {
        if request.model.trim().is_empty() {
            if self.default_model.trim().is_empty() {
                return Err(StagehandLlmError::MissingDefaultModel);
            }
            request.model = self.default_model.clone();
        }
        self.execute_request(request, function_name).await
    }

    async fn execute_request(
        &self,
        request: CreateChatCompletionRequest,
        function_name: Option<&str>,
    ) -> Result<CreateChatCompletionResponse, StagehandLlmError> {
        let model = request.model.clone();
        self.log_debug(&format!(
            "Sending chat completion request to model={} function={}",
            model,
            function_name.unwrap_or("n/a")
        ));

        let start = Instant::now();
        match self.provider.create_chat_completion(request).await {
            Ok(response) => {
                let elapsed = start.elapsed();
                if let Some(callback) = &self.metrics_callback {
                    callback(&response, elapsed, function_name);
                }
                self.log_debug(&format!(
                    "Chat completion succeeded: model={} duration={}ms",
                    model,
                    elapsed.as_millis()
                ));
                Ok(response)
            }
            Err(err) => {
                self.log_error(&format!(
                    "Chat completion failed for model={}: {}",
                    model, err
                ));
                Err(StagehandLlmError::OpenAi(err))
            }
        }
    }

    fn log_debug(&self, message: &str) {
        if let Some(logger) = &self.logger {
            logger(&format!("[llm][debug] {message}"));
        }
    }

    fn log_error(&self, message: &str) {
        if let Some(logger) = &self.logger {
            logger(&format!("[llm][error] {message}"));
        }
    }
}

impl StagehandLlmClient<OpenAiChatProvider> {
    /// Convenience constructor that wires the OpenAI provider using `StagehandConfig`.
    pub fn from_config(
        config: &StagehandConfig,
        metrics_callback: Option<MetricsCallback>,
    ) -> Result<Self, StagehandLlmError> {
        let provider = OpenAiChatProvider::from_config(config)?;
        let mut client = StagehandLlmClient::new(config.model_name.as_str(), provider);
        client.set_logger(config.logger.clone());
        client.set_metrics_callback(metrics_callback);
        Ok(client)
    }
}

fn apply_options(builder: &mut CreateChatCompletionRequestArgs, options: ChatCompletionOptions) {
    let ChatCompletionOptions {
        model: _,
        store,
        reasoning_effort,
        metadata,
        frequency_penalty,
        logit_bias,
        logprobs,
        top_logprobs,
        max_tokens,
        max_completion_tokens,
        n,
        modalities,
        prediction,
        audio,
        presence_penalty,
        response_format,
        seed,
        service_tier,
        stop,
        stream,
        stream_options,
        temperature,
        top_p,
        tools,
        tool_choice,
        parallel_tool_calls,
        user,
        web_search_options,
        function_call,
        functions,
    } = options;

    if let Some(store) = store {
        builder.store(store);
    }
    if let Some(reasoning_effort) = reasoning_effort {
        builder.reasoning_effort(reasoning_effort);
    }
    if let Some(metadata) = metadata {
        builder.metadata(metadata);
    }
    if let Some(frequency_penalty) = frequency_penalty {
        builder.frequency_penalty(frequency_penalty);
    }
    if let Some(logit_bias) = logit_bias {
        builder.logit_bias(logit_bias);
    }
    if let Some(logprobs) = logprobs {
        builder.logprobs(logprobs);
    }
    if let Some(top_logprobs) = top_logprobs {
        builder.top_logprobs(top_logprobs);
    }
    if let Some(max_tokens) = max_tokens {
        builder.max_tokens(max_tokens);
    }
    if let Some(max_completion_tokens) = max_completion_tokens {
        builder.max_completion_tokens(max_completion_tokens);
    }
    if let Some(n) = n {
        builder.n(n);
    }
    if let Some(modalities) = modalities {
        builder.modalities(modalities);
    }
    if let Some(prediction) = prediction {
        builder.prediction(prediction);
    }
    if let Some(audio) = audio {
        builder.audio(audio);
    }
    if let Some(presence_penalty) = presence_penalty {
        builder.presence_penalty(presence_penalty);
    }
    if let Some(response_format) = response_format {
        builder.response_format(response_format);
    }
    if let Some(seed) = seed {
        builder.seed(seed);
    }
    if let Some(service_tier) = service_tier {
        builder.service_tier(service_tier);
    }
    if let Some(stop) = stop {
        builder.stop(stop);
    }
    if let Some(stream) = stream {
        builder.stream(stream);
    }
    if let Some(stream_options) = stream_options {
        builder.stream_options(stream_options);
    }
    if let Some(temperature) = temperature {
        builder.temperature(temperature);
    }
    if let Some(top_p) = top_p {
        builder.top_p(top_p);
    }
    if let Some(tools) = tools {
        builder.tools(tools);
    }
    if let Some(tool_choice) = tool_choice {
        builder.tool_choice(tool_choice);
    }
    if let Some(parallel_tool_calls) = parallel_tool_calls {
        builder.parallel_tool_calls(parallel_tool_calls);
    }
    if let Some(user) = user {
        builder.user(user);
    }
    if let Some(web_search_options) = web_search_options {
        builder.web_search_options(web_search_options);
    }
    if let Some(function_call) = function_call {
        builder.function_call(function_call);
    }
    if let Some(functions) = functions {
        builder.functions(functions);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_openai::error::{ApiError, OpenAIError};
    use async_openai::types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessageArgs,
        ChatCompletionRequestSystemMessageContent, ChatCompletionRequestUserMessageArgs,
        ChatCompletionRequestUserMessageContent, CreateChatCompletionResponse,
    };
    use async_trait::async_trait;
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::super::provider::ChatCompletionProvider;

    #[derive(Debug, Default)]
    struct RecordingProvider {
        requests: Mutex<Vec<CreateChatCompletionRequest>>,
        response: Mutex<Option<Result<CreateChatCompletionResponse, OpenAIError>>>,
    }

    impl RecordingProvider {
        fn with_response(response: CreateChatCompletionResponse) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                response: Mutex::new(Some(Ok(response))),
            }
        }

        fn with_error(error: OpenAIError) -> Self {
            Self {
                requests: Mutex::new(Vec::new()),
                response: Mutex::new(Some(Err(error))),
            }
        }
    }

    #[async_trait]
    impl ChatCompletionProvider for RecordingProvider {
        async fn create_chat_completion(
            &self,
            request: CreateChatCompletionRequest,
        ) -> Result<CreateChatCompletionResponse, OpenAIError> {
            self.requests.lock().await.push(request);
            self.response.lock().await.take().unwrap_or_else(|| {
                Err(OpenAIError::ApiError(ApiError {
                    message: "no response configured".into(),
                    r#type: None,
                    param: None,
                    code: None,
                }))
            })
        }
    }

    fn sample_messages() -> Vec<ChatCompletionRequestMessage> {
        vec![
            ChatCompletionRequestMessage::System(
                ChatCompletionRequestSystemMessageArgs::default()
                    .content(ChatCompletionRequestSystemMessageContent::Text(
                        "You are Stagehand.".to_string(),
                    ))
                    .build()
                    .unwrap(),
            ),
            ChatCompletionRequestMessage::User(
                ChatCompletionRequestUserMessageArgs::default()
                    .content(ChatCompletionRequestUserMessageContent::Text(
                        "Say hello.".to_string(),
                    ))
                    .build()
                    .unwrap(),
            ),
        ]
    }

    fn sample_response() -> CreateChatCompletionResponse {
        serde_json::from_value(json!({
            "id": "cmpl-test",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "logprobs": null
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            },
            "system_fingerprint": null
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn build_request_uses_default_model() {
        let provider = RecordingProvider::with_response(sample_response());
        let client = StagehandLlmClient::new("gpt-4o", provider);

        let request = client
            .build_request(sample_messages(), ChatCompletionOptions::default())
            .expect("build request");

        assert_eq!(request.model, "gpt-4o");
        assert_eq!(request.messages.len(), 2);
    }

    #[tokio::test]
    async fn metrics_callback_receives_duration() {
        let provider = RecordingProvider::with_response(sample_response());
        let metrics_invocations: Arc<std::sync::Mutex<Vec<(Option<String>, Duration)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let metrics_clone = Arc::clone(&metrics_invocations);

        let client = StagehandLlmClient::new("gpt-4o", provider).with_metrics_callback(Some(
            Arc::new(move |_, duration, function| {
                metrics_clone
                    .lock()
                    .unwrap()
                    .push((function.map(|f| f.to_string()), duration));
            }),
        ));

        client
            .create_chat_completion(
                sample_messages(),
                ChatCompletionOptions::default(),
                Some("act"),
            )
            .await
            .expect("completion succeeds");

        let calls = metrics_invocations.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0.as_deref(), Some("act"));
        assert!(calls[0].1 >= Duration::ZERO);
    }

    #[tokio::test]
    async fn propagates_provider_error() {
        let expected_message = "bad request".to_string();
        let provider = RecordingProvider::with_error(OpenAIError::ApiError(ApiError {
            message: expected_message.clone(),
            r#type: None,
            param: None,
            code: None,
        }));
        let client = StagehandLlmClient::new("gpt-4o", provider);

        let err = client
            .create_chat_completion(sample_messages(), ChatCompletionOptions::default(), None)
            .await
            .expect_err("should propagate error");

        match err {
            StagehandLlmError::OpenAi(inner) => match inner {
                OpenAIError::ApiError(api_err) => {
                    assert_eq!(api_err.message, expected_message);
                }
                other => panic!("unexpected OpenAI error variant: {other:?}"),
            },
            other => panic!("unexpected error variant: {other:?}"),
        }
    }
}
