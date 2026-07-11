use crate::domain::chat::{ChatResponse, FinishReason, MessageContent};
use crate::domain::usage::Usage;
use serde::Serialize;

#[derive(Debug, serde::Deserialize)]
pub struct LegacyCompletionRequest {
    pub model: String,
    pub prompt: String,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub stream: bool,
    /// The caller's own end-user identifier. The legacy endpoint carries it too, and a
    /// tenant that migrates from `/v1/completions` should not lose its attribution.
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegacyCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<LegacyCompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegacyCompletionChoice {
    pub index: u32,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

impl LegacyCompletionResponse {
    pub fn from_domain(model_alias: &str, resp: ChatResponse) -> Self {
        Self {
            id: resp.id,
            object: "text_completion".into(),
            created: resp.created,
            model: model_alias.into(),
            choices: resp
                .choices
                .into_iter()
                .map(|c| LegacyCompletionChoice {
                    index: c.index,
                    text: message_text(&c.message.content),
                    finish_reason: c.finish_reason.map(finish_reason_str),
                })
                .collect(),
            usage: resp.usage,
        }
    }
}

fn message_text(content: &Option<MessageContent>) -> String {
    match content {
        Some(MessageContent::Text(t)) => t.clone(),
        Some(MessageContent::Parts(parts)) => parts
            .iter()
            .filter_map(|p| match p {
                crate::domain::chat::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        None => String::new(),
    }
}

fn finish_reason_str(reason: FinishReason) -> String {
    reason.as_openai_str().into()
}
