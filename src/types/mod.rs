//! Core data structures mirrored from the Python Stagehand implementation.
//!
//! These strongly-typed models provide a shared vocabulary for browser
//! automation, LLM payloads, accessibility trees, and agent execution.

pub mod a11y;
pub mod agent;
pub mod llm;
pub mod page;

pub use a11y::{
    AccessibilityNode, AxNode, AxProperty, AxValue, CdpSession, Locator, PlaywrightCommandError,
    PlaywrightMethodNotSupportedError, TreeResult,
};
pub use agent::{
    ActionExecutionResult, AgentAction, AgentActionPayload, AgentClientOptions, AgentConfig,
    AgentExecuteOptions, AgentHandlerOptions, AgentResult, AgentUsage, EnvState,
};
pub use llm::{
    ChatMessage, ChatMessageContent, ChatMessageContentPart, ChatMessageImageContent,
    ChatMessageImageUrl, ChatMessageSource, ChatMessageTextContent, ChatRole,
};
pub use page::{
    ActOptions, ActResult, DefaultExtractSchema, EmptyExtractSchema, ExtractOptions, ExtractResult,
    MetadataSchema, NavigateOptions, ObserveElementSchema, ObserveInferenceSchema, ObserveOptions,
    ObserveResult,
};
