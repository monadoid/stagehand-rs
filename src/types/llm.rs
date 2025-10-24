use serde::{Deserialize, Serialize};

/// Supported chat roles for Stagehand messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ChatRole {
    System,
    User,
    Assistant,
}

/// Chat message sent to or returned from the LLM provider.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: ChatMessageContent,
}

/// Individual content part used for multimodal chat messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatMessageContentPart {
    Text(ChatMessageTextContent),
    ImageUrl(ChatMessageImageContent),
}

/// Textual content inside a chat message content part.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessageTextContent {
    pub text: String,
}

/// Metadata for an image URL payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatMessageImageUrl {
    pub url: String,
}

/// Inline binary source information supplied to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    #[serde(rename = "media_type")]
    pub media_type: String,
    pub data: String,
}

/// Image payload for multimodal chat messages.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ChatMessageImageContent {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<ChatMessageImageUrl>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ChatMessageSource>,
}

/// Chat message content can be either a simple string or a sequence of parts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum ChatMessageContent {
    Text(String),
    Parts(Vec<ChatMessageContentPart>),
}

impl ChatMessageContent {
    /// Convenience helper for constructing text-only content.
    pub fn text<T: Into<String>>(value: T) -> Self {
        ChatMessageContent::Text(value.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    #[test]
    fn serialize_text_message() {
        let msg = ChatMessage {
            role: ChatRole::User,
            content: ChatMessageContent::text("Hello"),
        };

        let value = serde_json::to_value(&msg).expect("serialize");
        assert_eq!(
            value,
            json!({
                "role": "user",
                "content": "Hello"
            })
        );
    }

    #[test]
    fn deserialize_multimodal_message() {
        let raw: Value = json!({
            "role": "assistant",
            "content": [
                { "type": "text", "text": "Here is an image" },
                {
                    "type": "image_url",
                    "image_url": { "url": "https://example.com/img.png" }
                }
            ]
        });

        let message: ChatMessage = serde_json::from_value(raw).expect("deserialize");
        assert!(matches!(message.content, ChatMessageContent::Parts(parts) if parts.len() == 2));
    }
}
