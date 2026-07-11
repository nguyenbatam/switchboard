use super::timeouts::{Timeouts, guard_stream};
use super::{CallCx, Caps, ModelProvider, ProviderError, TimeoutKind};
use crate::domain::chat::{ChatRequest, ChatResponse, FinishReason};
use crate::domain::embed::{EmbedRequest, EmbedResponse};
use crate::domain::stream::StreamEvent;
use crate::domain::usage::Usage;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};

pub struct OpenAiProvider {
    base_url: String,
    api_key: String,
    /// Separate clients: the unary one enforces `total` via reqwest, the streaming
    /// one must not, or a long generation would be killed as a timeout.
    unary: reqwest::Client,
    streaming: reqwest::Client,
    timeouts: Timeouts,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self::with_timeouts(base_url, api_key, Timeouts::default())
    }

    pub fn with_timeouts(base_url: String, api_key: String, timeouts: Timeouts) -> Self {
        Self {
            base_url,
            api_key,
            unary: timeouts.unary_client(),
            streaming: timeouts.stream_client(),
            timeouts,
        }
    }
}

mod wire {
    pub use crate::domain::chat::ChatResponse as OpenAiChatResponse;
    pub use crate::domain::embed::EmbedResponse as OpenAiEmbedResponse;
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    fn capabilities(&self, _model: &str) -> Caps {
        Caps {
            tools: true,
            parallel_tool_calls: true,
            json_schema: true,
            vision: true,
            streaming_usage: true,
            max_context: 128_000,
        }
    }

    async fn chat(
        &self,
        model: &str,
        req: &ChatRequest,
        _cx: &CallCx,
    ) -> Result<ChatResponse, ProviderError> {
        let mut body = req.clone();
        body.model = model.into();
        body.stream = false;

        let resp = self
            .unary
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(classify_transport)?;

        if !resp.status().is_success() {
            return Err(classify_status(resp.status().as_u16()));
        }

        let openai: wire::OpenAiChatResponse = resp.json().await?;
        Ok(openai)
    }

    async fn stream(
        &self,
        model: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<BoxStream<'static, Result<StreamEvent, ProviderError>>, ProviderError> {
        let mut body = req.clone();
        body.model = model.into();
        body.stream = true;

        let resp = self
            .streaming
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(classify_transport)?;

        if !resp.status().is_success() {
            return Err(classify_status(resp.status().as_u16()));
        }

        let cancel = cx.cancel.clone();
        let stream = async_stream::stream! {
            let mut lines = resp.bytes_stream();
            let mut buffer = String::new();
            let mut announced = false;

            while let Some(chunk) = lines.next().await {
                if cancel.is_cancelled() {
                    break;
                }
                let chunk = chunk.map_err(|e| ProviderError::Other(e.to_string()))?;
                buffer.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(pos) = buffer.find('\n') {
                    let line = buffer.drain(..=pos).collect::<String>();
                    let line = line.trim();
                    if line.is_empty() || line == "data: [DONE]" {
                        continue;
                    }
                    let Some(json) = line.strip_prefix("data: ") else { continue };
                    let parsed: StreamChunk = match serde_json::from_str(json) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    if !announced {
                        announced = true;
                        yield Ok(StreamEvent::Start {
                            id: parsed.id.clone().unwrap_or_default(),
                            model: parsed.model.clone().unwrap_or_default(),
                            created: parsed.created.unwrap_or(0),
                        });
                    }

                    if let Some(usage) = parsed.usage {
                        yield Ok(StreamEvent::Usage(usage));
                        continue;
                    }

                    for choice in parsed.choices {
                        if let Some(reasoning) = choice.delta.reasoning_content
                            && !reasoning.is_empty()
                        {
                            yield Ok(StreamEvent::ReasoningDelta { content: reasoning });
                        }
                        if let Some(content) = choice.delta.content
                            && !content.is_empty()
                        {
                            yield Ok(StreamEvent::Delta { content });
                        }
                        // OpenAI streams tool calls as partial JSON in `arguments`,
                        // keyed by `index`. Dropping these loses the entire tool call
                        // whenever a client streams a function-calling request.
                        for tc in choice.delta.tool_calls {
                            yield Ok(StreamEvent::ToolCallDelta {
                                index: tc.index,
                                id: tc.id,
                                name: tc.function.as_ref().and_then(|f| f.name.clone()),
                                arguments: tc
                                    .function
                                    .and_then(|f| f.arguments)
                                    .unwrap_or_default(),
                            });
                        }
                        if let Some(reason) = choice.finish_reason {
                            yield Ok(StreamEvent::Finish {
                                reason: map_finish_reason(&reason),
                            });
                        }
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
        let mut body = req.clone();
        body.model = model.into();
        // We always want floats back; base64 re-encoding happens at the edge.
        body.encoding_format = Some("float".into());

        let resp = self
            .unary
            .post(format!("{}/embeddings", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(classify_transport)?;

        if !resp.status().is_success() {
            return Err(classify_status(resp.status().as_u16()));
        }

        let openai: wire::OpenAiEmbedResponse = resp.json().await?;
        Ok(openai)
    }

    async fn healthy(&self) -> bool {
        self.unary
            .get(format!("{}/models", self.base_url))
            .bearer_auth(&self.api_key)
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

#[derive(serde::Deserialize)]
struct StreamChunk {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    created: Option<i64>,
    #[serde(default)]
    choices: Vec<StreamChoice>,
    usage: Option<Usage>,
}

#[derive(serde::Deserialize)]
struct StreamChoice {
    delta: StreamDelta,
    finish_reason: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct StreamDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<StreamToolCall>,
}

#[derive(serde::Deserialize)]
struct StreamToolCall {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<StreamToolCallFunction>,
}

#[derive(serde::Deserialize)]
struct StreamToolCallFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Stop,
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

fn classify_status(status: u16) -> ProviderError {
    match status {
        401 | 403 => ProviderError::Auth,
        429 => ProviderError::RateLimited { retry_after: None },
        // "Busy, come back" is a different fact from "I am broken". 529 is
        // Anthropic's spelling of the same thing.
        503 | 529 => ProviderError::Overloaded,
        500..=599 => ProviderError::Upstream5xx { status },
        _ => ProviderError::BadRequest { param: None },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> StreamChunk {
        serde_json::from_str(json).expect("chunk parses")
    }

    #[test]
    fn tool_call_deltas_are_decoded() {
        let chunk = parse(
            r#"{"id":"c1","choices":[{"delta":{"tool_calls":[
                {"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{\"ci"}}
            ]},"finish_reason":null}]}"#,
        );
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert_eq!(tc.index, 0);
        assert_eq!(tc.id.as_deref(), Some("call_1"));
        let f = tc.function.as_ref().unwrap();
        assert_eq!(f.name.as_deref(), Some("get_weather"));
        assert_eq!(f.arguments.as_deref(), Some("{\"ci"));
    }

    #[test]
    fn a_continuation_delta_carries_arguments_but_no_name() {
        let chunk = parse(
            r#"{"choices":[{"delta":{"tool_calls":[
                {"index":0,"function":{"arguments":"ty\": \"SF\"}"}}
            ]},"finish_reason":null}]}"#,
        );
        let tc = &chunk.choices[0].delta.tool_calls[0];
        assert!(tc.id.is_none());
        let f = tc.function.as_ref().unwrap();
        assert!(f.name.is_none());
        assert_eq!(f.arguments.as_deref(), Some("ty\": \"SF\"}"));
    }

    #[test]
    fn a_plain_content_delta_still_parses() {
        let chunk = parse(r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#);
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("hi"));
        assert!(chunk.choices[0].delta.tool_calls.is_empty());
    }

    #[test]
    fn overload_is_distinguished_from_a_broken_upstream() {
        assert!(matches!(classify_status(503), ProviderError::Overloaded));
        assert!(matches!(classify_status(529), ProviderError::Overloaded));
        assert!(matches!(
            classify_status(500),
            ProviderError::Upstream5xx { status: 500 }
        ));
    }
}
