use crate::domain::chat::{ChatResponse, FinishReason, Message};
use crate::domain::usage::Usage;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionResponse {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatCompletionChoice>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChoice {
    pub index: u32,
    pub message: Message,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

impl ChatCompletionResponse {
    pub fn from_domain(model_alias: &str, resp: ChatResponse) -> Self {
        Self {
            id: resp.id,
            object: "chat.completion".into(),
            created: resp.created,
            model: model_alias.into(),
            choices: resp
                .choices
                .into_iter()
                .map(|c| ChatCompletionChoice {
                    index: c.index,
                    message: c.message,
                    finish_reason: c.finish_reason.map(finish_reason_str),
                })
                .collect(),
            usage: resp.usage,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunk {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<ChatCompletionChunkChoice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletionChunkChoice {
    pub index: u32,
    pub delta: ChatCompletionDelta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ChatCompletionDelta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDeltaWire>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallDeltaWire {
    pub index: u32,
    pub id: String,
    pub r#type: String,
    pub function: ToolCallDeltaFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallDeltaFunction {
    pub name: String,
    pub arguments: String,
}

impl ChatCompletionChunk {
    pub fn content_delta(id: &str, model: &str, created: i64, content: &str) -> Self {
        Self {
            id: id.into(),
            object: "chat.completion.chunk".into(),
            created,
            model: model.into(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionDelta {
                    content: Some(content.into()),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    pub fn tool_call_delta(
        id: &str,
        model: &str,
        created: i64,
        tool_call: ToolCallDeltaWire,
    ) -> Self {
        Self {
            id: id.into(),
            object: "chat.completion.chunk".into(),
            created,
            model: model.into(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionDelta {
                    tool_calls: Some(vec![tool_call]),
                    ..Default::default()
                },
                finish_reason: None,
            }],
            usage: None,
        }
    }

    pub fn finish(id: &str, model: &str, created: i64, reason: FinishReason) -> Self {
        Self {
            id: id.into(),
            object: "chat.completion.chunk".into(),
            created,
            model: model.into(),
            choices: vec![ChatCompletionChunkChoice {
                index: 0,
                delta: ChatCompletionDelta {
                    role: Some("assistant".into()),
                    ..Default::default()
                },
                finish_reason: Some(finish_reason_str(reason)),
            }],
            usage: None,
        }
    }

    pub fn usage_only(id: &str, model: &str, created: i64, usage: Usage) -> Self {
        Self {
            id: id.into(),
            object: "chat.completion.chunk".into(),
            created,
            model: model.into(),
            choices: vec![],
            usage: Some(usage),
        }
    }
}

fn finish_reason_str(reason: FinishReason) -> String {
    reason.as_openai_str().into()
}
