use crate::domain::chat::{ChatRequest, FinishReason, ToolCall};
use crate::domain::stream::StreamEvent;
use crate::domain::usage::Usage;
use crate::http::wire::{ChatCompletionChunk, ToolCallDeltaFunction, ToolCallDeltaWire};
use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use futures::StreamExt;
use serde::Serialize;
use std::convert::Infallible;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn sse_response<S>(stream: S) -> Response
where
    S: futures::Stream<Item = Result<Bytes, Infallible>> + Send + 'static,
{
    Response::builder()
        .header(http::header::CONTENT_TYPE, "text/event-stream")
        .header(http::header::CACHE_CONTROL, "no-cache")
        .header(http::header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .expect("valid SSE response")
}

pub fn encode_chunk<T: Serialize>(chunk: &T) -> Bytes {
    let json = serde_json::to_string(chunk).unwrap_or_else(|_| "{}".into());
    Bytes::from(format!("data: {json}\n\n"))
}

pub fn encode_done() -> Bytes {
    Bytes::from_static(b"data: [DONE]\n\n")
}

/// Translates provider events into the OpenAI SSE wire format.
///
/// `guard` is moved into the stream, so it drops exactly when the response body
/// does — on clean completion, and equally on a client that hangs up mid-answer.
/// That drop is what writes the usage row.
pub fn chat_stream_to_openai(
    mut events: impl futures::Stream<Item = Result<StreamEvent, crate::providers::ProviderError>>
    + Unpin
    + Send
    + 'static,
    request: ChatRequest,
    model_alias: String,
    guard: Option<crate::usage::StreamGuard>,
) -> impl futures::Stream<Item = Result<Bytes, Infallible>> + Send {
    async_stream::stream! {
        let mut guard = guard;
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let mut id = format!("chatcmpl-{}", uuid::Uuid::now_v7());
        let mut content_buf = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish_reason: Option<FinishReason> = None;
        let include_usage = request
            .stream_options
            .as_ref()
            .is_some_and(|o| o.include_usage);
        let mut usage: Option<Usage> = None;
        let mut started = false;

        while let Some(event) = events.next().await {
            match event {
                Ok(StreamEvent::Start { id: eid, model: _, created: _ }) => {
                    id = eid;
                    started = true;
                }
                Ok(StreamEvent::Delta { content }) => {
                    if !started {
                        started = true;
                    }
                    if let Some(g) = guard.as_mut() {
                        g.on_delta(&content);
                    }
                    content_buf.push_str(&content);
                    let chunk = ChatCompletionChunk::content_delta(&id, &model_alias, created, &content);
                    yield Ok(encode_chunk(&chunk));
                }
                Ok(StreamEvent::ToolCallDelta { index, id: tc_id, name, arguments }) => {
                    if let Some(g) = guard.as_mut() {
                        g.on_tool_call_delta(&arguments);
                    }
                    while tool_calls.len() <= index as usize {
                        tool_calls.push(ToolCall {
                            id: format!("call_{}", uuid::Uuid::now_v7()),
                            r#type: "function".into(),
                            function: crate::domain::chat::FunctionCall {
                                name: String::new(),
                                arguments: String::new(),
                            },
                        });
                    }
                    let tc = &mut tool_calls[index as usize];
                    if let Some(tc_id) = tc_id {
                        tc.id = tc_id;
                    }
                    if let Some(name) = name {
                        tc.function.name = name;
                    }
                    tc.function.arguments.push_str(&arguments);

                    let chunk = ChatCompletionChunk::tool_call_delta(
                        &id,
                        &model_alias,
                        created,
                        ToolCallDeltaWire {
                            index,
                            id: tc.id.clone(),
                            r#type: "function".into(),
                            function: ToolCallDeltaFunction {
                                name: tc.function.name.clone(),
                                arguments,
                            },
                        },
                    );
                    yield Ok(encode_chunk(&chunk));
                }
                Ok(StreamEvent::Finish { reason }) => {
                    if let Some(g) = guard.as_mut() {
                        g.on_finish(reason);
                    }
                    finish_reason = Some(reason);
                }
                Ok(StreamEvent::Usage(u)) => {
                    if let Some(g) = guard.as_mut() {
                        g.on_upstream_usage(u.clone());
                    }
                    usage = Some(u);
                }
                Ok(StreamEvent::ReasoningDelta { .. }) => {}
                Err(e) => {
                    tracing::error!(error = %e, "stream error");
                    if let Some(g) = guard.as_mut() {
                        g.on_error(crate::providers::registry::error_kind(&e));
                    }
                    break;
                }
            }
        }

        let reason = finish_reason.unwrap_or(FinishReason::Stop);
        let chunk = ChatCompletionChunk::finish(&id, &model_alias, created, reason);
        yield Ok(encode_chunk(&chunk));

        if include_usage {
            let u = usage.unwrap_or_else(|| {
                Usage {
                    prompt_tokens: 0,
                    completion_tokens: content_buf.len() as u32 / 4,
                    ..Default::default()
                }
                .with_total()
            });
            let chunk = ChatCompletionChunk::usage_only(&id, &model_alias, created, u);
            yield Ok(encode_chunk(&chunk));
        }

        yield Ok(encode_done());

        // Reached only if the client was still listening. Anything that drops the
        // guard before this line is billed as `client_disconnect`.
        if let Some(g) = guard.as_mut() {
            g.mark_completed();
        }
    }
}
