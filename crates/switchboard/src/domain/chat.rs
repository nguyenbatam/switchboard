use crate::domain::schema::{StructuredOutput, ToolParameters};
use crate::rag::SearchMode;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(default)]
    pub tools: Option<Vec<Tool>>,
    #[serde(default)]
    pub tool_choice: Option<ToolChoice>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub top_p: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    /// Determinism hint. Part of the cache key: two requests differing only by
    /// `seed` are different requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<i64>,
    /// Stop sequences, forwarded upstream. Dropping this silently would let the
    /// model run past where the client asked it to stop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopSequences>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub rag: Option<RagOptions>,
    /// The caller's own identifier for the end user this request is on behalf of.
    ///
    /// OpenAI's field, and the reason it exists here: a tenant is who pays us, not who
    /// caused the spend. A bot fleet serving ten thousand people is one tenant and one
    /// invoice, and without this it cannot re-bill its own users or find the one whose
    /// runaway loop ate the month's budget.
    ///
    /// It lands on the usage row and nowhere else. It is **never** a metric label
    /// (unbounded cardinality, per docs/10) and — because `cache::key::canonical_request`
    /// is an allowlist rather than a blanket serialisation — it is **never** part of the
    /// cache key. Two users asking the same question share one cached answer, which is the
    /// entire economics of the cache.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// When true, non-stream responses use the legacy `/v1/completions` wire shape.
    #[serde(skip, default)]
    pub legacy_completion: bool,
    /// Set when retrieved (untrusted) content was spliced into `messages`. The agent
    /// loop then starts tainted, so egress and side-effecting tools are unavailable.
    #[serde(skip, default)]
    pub untrusted_context: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StopSequences {
    One(String),
    Many(Vec<String>),
}

impl StopSequences {
    pub fn as_slice(&self) -> Vec<&str> {
        match self {
            Self::One(s) => vec![s.as_str()],
            Self::Many(v) => v.iter().map(String::as_str).collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RagOptions {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_rag_k")]
    pub top_k: u32,
    #[serde(default)]
    pub mode: SearchMode,
    #[serde(default)]
    pub embed_model: Option<String>,
}

fn default_true() -> bool {
    true
}

fn default_rag_k() -> u32 {
    3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

/// `Structured` carries a whole JSON schema, so it is boxed: otherwise every
/// `ResponseFormat::Type` — the overwhelmingly common case — would occupy as much
/// space as the schema it does not have.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponseFormat {
    Type {
        r#type: String,
    },
    Structured {
        r#type: String,
        #[serde(rename = "json_schema")]
        output: Box<StructuredOutput>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: Option<MessageContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub r#type: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFunction {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub parameters: ToolParameters,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ToolChoice {
    Mode(String),
    Named {
        r#type: String,
        function: NamedFunction,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamedFunction {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub r#type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
}

impl FinishReason {
    pub fn as_openai_str(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Length => "length",
            Self::ToolCalls => "tool_calls",
            Self::ContentFilter => "content_filter",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatResponse {
    pub id: String,
    pub model: String,
    pub created: i64,
    pub choices: Vec<Choice>,
    pub usage: super::usage::Usage,
    /// The alias that actually answered. Differs from the requested alias only when
    /// a fallback fired, which is what makes "what fraction was served by the
    /// fallback" answerable from `usage_records`.
    #[serde(skip, default)]
    pub served_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub index: u32,
    pub message: Message,
    pub finish_reason: Option<FinishReason>,
}
