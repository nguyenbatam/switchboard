use super::timeouts::{Timeouts, guard_stream};
use super::{CallCx, Caps, ModelProvider, ProviderError, TimeoutKind};
use crate::domain::chat::{
    ChatRequest, ChatResponse, Choice, FinishReason, Message, MessageContent, Role, ToolCall,
};
use crate::domain::embed::{EmbedRequest, EmbedResponse, EmbeddingData};
use crate::domain::stream::StreamEvent;
use crate::domain::usage::Usage;
use crate::domain::values::ParsedToolArguments;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct OllamaProvider {
    base_url: String,
    unary: reqwest::Client,
    streaming: reqwest::Client,
    timeouts: Timeouts,
}

impl OllamaProvider {
    pub fn new(base_url: String) -> Self {
        Self::with_timeouts(base_url, Timeouts::default())
    }

    pub fn with_timeouts(base_url: String, timeouts: Timeouts) -> Self {
        Self {
            base_url,
            unary: timeouts.unary_client(),
            streaming: timeouts.stream_client(),
            timeouts,
        }
    }
}

mod wire {
    use crate::domain::chat::Tool;
    use crate::domain::values::ParsedToolArguments;
    use serde::{Deserialize, Serialize};

    #[derive(Serialize)]
    pub struct ChatRequest {
        pub model: String,
        pub messages: Vec<Message>,
        pub stream: bool,
        /// Disable thinking/reasoning traces so output lands in `content` (OpenAI-compatible).
        pub think: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tools: Option<Vec<Tool>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub options: Option<Options>,
    }

    #[derive(Serialize, Debug, PartialEq)]
    pub struct Options {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub temperature: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub num_predict: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub top_p: Option<f32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub top_k: Option<u32>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub seed: Option<i64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub stop: Option<Vec<String>>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Message {
        pub role: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_calls: Option<Vec<ToolCall>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub tool_call_id: Option<String>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct ToolCall {
        pub function: FunctionCall,
    }

    #[derive(Serialize, Deserialize)]
    pub struct FunctionCall {
        pub name: String,
        pub arguments: ParsedToolArguments,
    }

    #[derive(Deserialize)]
    pub struct ChatResponse {
        pub message: Message,
        pub done: bool,
        #[serde(default)]
        pub done_reason: Option<String>,
        #[serde(default)]
        pub prompt_eval_count: Option<u32>,
        #[serde(default)]
        pub eval_count: Option<u32>,
    }

    #[derive(Serialize)]
    pub struct EmbedRequest {
        pub model: String,
        pub input: Vec<String>,
    }

    #[derive(Deserialize)]
    pub struct EmbedResponse {
        pub embeddings: Vec<Vec<f32>>,
    }
}

use wire as ollama_wire;

fn map_request(model: &str, req: &ChatRequest) -> ollama_wire::ChatRequest {
    let messages = req
        .messages
        .iter()
        .map(|m| {
            let content = m.content.as_ref().map(|c| match c {
                MessageContent::Text(t) => t.clone(),
                MessageContent::Parts(parts) => parts
                    .iter()
                    .filter_map(|p| match p {
                        crate::domain::chat::ContentPart::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            });
            ollama_wire::Message {
                role: format!("{:?}", m.role).to_lowercase(),
                content,
                tool_calls: m.tool_calls.as_ref().map(|tcs| {
                    tcs.iter()
                        .map(|tc| ollama_wire::ToolCall {
                            function: ollama_wire::FunctionCall {
                                name: tc.function.name.clone(),
                                arguments: ParsedToolArguments::parse_json(&tc.function.arguments)
                                    .unwrap_or_else(|_| ParsedToolArguments::empty_object()),
                            },
                        })
                        .collect()
                }),
                tool_call_id: m.tool_call_id.clone(),
            }
        })
        .collect();

    ollama_wire::ChatRequest {
        model: model.into(),
        messages,
        stream: req.stream,
        think: false,
        tools: req.tools.clone(),
        options: Some(ollama_wire::Options {
            temperature: req.temperature,
            num_predict: req.max_tokens,
            top_p: req.top_p,
            top_k: req.top_k,
            seed: req.seed,
            stop: req
                .stop
                .as_ref()
                .map(|s| s.as_slice().into_iter().map(str::to_string).collect()),
        }),
    }
}

fn finish_reason(done_reason: Option<&str>, has_tools: bool) -> FinishReason {
    if has_tools {
        return FinishReason::ToolCalls;
    }
    match done_reason {
        Some("length") => FinishReason::Length,
        _ => FinishReason::Stop,
    }
}

#[async_trait]
impl ModelProvider for OllamaProvider {
    fn name(&self) -> &'static str {
        "ollama"
    }

    fn capabilities(&self, _model: &str) -> Caps {
        Caps {
            tools: true,
            parallel_tool_calls: false,
            ..Default::default()
        }
    }

    async fn chat(
        &self,
        model: &str,
        req: &ChatRequest,
        _cx: &CallCx,
    ) -> Result<ChatResponse, ProviderError> {
        let body = map_request(model, req);
        let url = format!("{}/api/chat", self.base_url);
        let resp = self
            .unary
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(classify_transport)?;

        if !resp.status().is_success() {
            return Err(classify_status(resp.status().as_u16()));
        }

        let ollama: ollama_wire::ChatResponse = resp.json().await?;
        let created = now_secs();
        let id = format!("chatcmpl-{}", uuid::Uuid::now_v7());

        let has_tools = ollama.message.tool_calls.is_some();
        let assistant = Message {
            role: Role::Assistant,
            content: ollama.message.content.clone().map(MessageContent::Text),
            name: None,
            tool_calls: ollama.message.tool_calls.map(|tcs| {
                tcs.into_iter()
                    .map(|tc| ToolCall {
                        id: format!("call_{}", uuid::Uuid::now_v7()),
                        r#type: "function".into(),
                        function: crate::domain::chat::FunctionCall {
                            name: tc.function.name,
                            arguments: tc.function.arguments.to_string(),
                        },
                    })
                    .collect()
            }),
            tool_call_id: None,
        };

        Ok(ChatResponse {
            id,
            model: model.into(),
            served_by: None,
            created,
            choices: vec![Choice {
                index: 0,
                message: assistant,
                finish_reason: Some(finish_reason(ollama.done_reason.as_deref(), has_tools)),
            }],
            usage: Usage {
                prompt_tokens: ollama.prompt_eval_count.unwrap_or(0),
                completion_tokens: ollama.eval_count.unwrap_or(0),
                ..Default::default()
            }
            .with_total(),
        })
    }

    async fn stream(
        &self,
        model: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let mut body = map_request(model, req);
        body.stream = true;
        let url = format!("{}/api/chat", self.base_url);
        let resp = self
            .streaming
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(classify_transport)?;

        if !resp.status().is_success() {
            return Err(classify_status(resp.status().as_u16()));
        }

        let model = model.to_string();
        let id = format!("chatcmpl-{}", uuid::Uuid::now_v7());
        let created = now_secs();
        let cancel = cx.cancel.clone();

        let stream = async_stream::stream! {
            yield Ok(StreamEvent::Start { id: id.clone(), model: model.clone(), created });

            let mut byte_stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut saw_tool_calls = false;

            while let Some(chunk) = byte_stream.next().await {
                if cancel.is_cancelled() {
                    break;
                }
                let chunk = chunk.map_err(|e| ProviderError::Other(e.to_string()))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer.drain(..=pos).collect::<String>();
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    let parsed: ollama_wire::ChatResponse = match serde_json::from_str(line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if let Some(content) = parsed.message.content
                        && !content.is_empty()
                    {
                        yield Ok(StreamEvent::Delta { content });
                    }

                    if let Some(tcs) = parsed.message.tool_calls {
                        saw_tool_calls = true;
                        for (i, tc) in tcs.into_iter().enumerate() {
                            let args = tc.function.arguments.to_string();
                            yield Ok(StreamEvent::ToolCallDelta {
                                index: i as u32,
                                id: Some(format!("call_{}", uuid::Uuid::now_v7())),
                                name: Some(tc.function.name),
                                arguments: args,
                            });
                        }
                    }

                    if parsed.done {
                        // Ollama sends tool calls in an earlier chunk than `done`, so
                        // the terminal chunk alone cannot tell us. A stream that
                        // emitted tool calls but finishes `stop` tells the client SDK
                        // to render the answer instead of executing the tool.
                        yield Ok(StreamEvent::Finish {
                            reason: finish_reason(parsed.done_reason.as_deref(), saw_tool_calls),
                        });
                        let usage = Usage {
                            prompt_tokens: parsed.prompt_eval_count.unwrap_or(0),
                            completion_tokens: parsed.eval_count.unwrap_or(0),
                            ..Default::default()
                        }.with_total();
                        yield Ok(StreamEvent::Usage(usage));
                    }
                }
            }
        };

        Ok(guard_stream(Box::pin(stream), self.timeouts))
    }

    async fn embed(
        &self,
        model: &str,
        req: &EmbedRequest,
        _cx: &CallCx,
    ) -> Result<EmbedResponse, ProviderError> {
        // Ollama's embed API takes text. Detokenizing the client's ids would need
        // the model's vocabulary, so say plainly that this input shape is not
        // supported here rather than embedding something the client did not send.
        let inputs = req
            .input
            .as_texts()
            .ok_or_else(|| ProviderError::BadRequest {
                param: Some("input".into()),
            })?;
        let body = ollama_wire::EmbedRequest {
            model: model.into(),
            input: inputs.clone(),
        };
        let url = format!("{}/api/embed", self.base_url);
        let resp = self
            .unary
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(classify_transport)?;

        if !resp.status().is_success() {
            return Err(classify_status(resp.status().as_u16()));
        }

        let ollama: ollama_wire::EmbedResponse = resp.json().await?;
        let data = ollama
            .embeddings
            .into_iter()
            .enumerate()
            .map(|(i, embedding)| EmbeddingData {
                index: i as u32,
                embedding,
                object: Some("embedding".into()),
            })
            .collect();

        Ok(EmbedResponse {
            model: model.into(),
            data,
            usage: Usage {
                prompt_tokens: inputs.iter().map(|s| s.len() as u32 / 4).sum(),
                ..Default::default()
            }
            .with_total(),
        })
    }

    async fn healthy(&self) -> bool {
        self.unary
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

fn classify_status(status: u16) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimited { retry_after: None },
        // "Busy, come back" is a different fact from "I am broken", and the two
        // deserve different backoff.
        503 => ProviderError::Overloaded,
        500..=599 => ProviderError::Upstream5xx { status },
        _ => ProviderError::BadRequest { param: None },
    }
}

/// `reqwest` collapses several failures into one error type; the caller needs to
/// know whether we never connected or connected and then timed out.
fn classify_transport(e: reqwest::Error) -> ProviderError {
    if e.is_connect() {
        ProviderError::Timeout(TimeoutKind::Connect)
    } else if e.is_timeout() {
        ProviderError::Timeout(TimeoutKind::Total)
    } else {
        ProviderError::Http(e)
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::chat::{Message as DomainMessage, StopSequences};

    #[test]
    fn a_response_carrying_tool_calls_finishes_with_tool_calls() {
        assert_eq!(finish_reason(None, true), FinishReason::ToolCalls);
        assert_eq!(finish_reason(Some("stop"), true), FinishReason::ToolCalls);
    }

    #[test]
    fn a_plain_response_maps_its_done_reason() {
        assert_eq!(finish_reason(Some("stop"), false), FinishReason::Stop);
        assert_eq!(finish_reason(Some("length"), false), FinishReason::Length);
        assert_eq!(finish_reason(None, false), FinishReason::Stop);
    }

    #[test]
    fn sampling_parameters_reach_the_upstream() {
        let req = ChatRequest {
            model: "m".into(),
            messages: vec![DomainMessage {
                role: Role::User,
                content: Some(MessageContent::Text("hi".into())),
                name: None,
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: None,
            tool_choice: None,
            temperature: Some(0.0),
            top_p: Some(0.9),
            max_tokens: Some(128),
            seed: Some(42),
            stop: Some(StopSequences::Many(vec!["END".into(), "\n\n".into()])),
            top_k: Some(40),
            stream: false,
            stream_options: None,
            response_format: None,
            parallel_tool_calls: None,
            rag: None,
            user: None,
            legacy_completion: false,
            untrusted_context: false,
        };

        let body = map_request("m", &req);
        let opts = body.options.expect("options are sent");

        assert_eq!(opts.seed, Some(42));
        assert_eq!(opts.top_k, Some(40));
        assert_eq!(opts.top_p, Some(0.9));
        assert_eq!(
            opts.stop,
            Some(vec!["END".to_string(), "\n\n".to_string()]),
            "a dropped stop sequence lets the model run past where the client asked it to halt"
        );
    }

    #[test]
    fn overload_is_retryable_and_distinct_from_a_broken_upstream() {
        assert!(matches!(classify_status(503), ProviderError::Overloaded));
        assert!(matches!(
            classify_status(500),
            ProviderError::Upstream5xx { status: 500 }
        ));
        assert!(matches!(classify_status(401), ProviderError::Auth));
    }
}
