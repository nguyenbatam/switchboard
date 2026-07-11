use crate::app::AppState;
use crate::domain::chat::{ChatRequest, Message, Role};
use crate::http::error::ApiError;
use crate::http::extract::TenantCtx;
use crate::http::routes::chat::run_chat;
use crate::http::wire::LegacyCompletionRequest;
use axum::Extension;
use axum::extract::State;
use axum::http::HeaderMap;
use bytes::Bytes;

pub async fn completions(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<axum::response::Response, ApiError> {
    let legacy: LegacyCompletionRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::InvalidRequest {
            message: e.to_string(),
            param: None,
        })?;
    let chat = ChatRequest {
        model: legacy.model,
        messages: vec![Message {
            role: Role::User,
            content: Some(crate::domain::chat::MessageContent::Text(legacy.prompt)),
            name: None,
            tool_calls: None,
            tool_call_id: None,
        }],
        tools: None,
        tool_choice: None,
        temperature: legacy.temperature,
        top_p: None,
        max_tokens: legacy.max_tokens,
        seed: None,
        stop: None,
        top_k: None,
        user: legacy.user,
        stream: legacy.stream,
        stream_options: None,
        response_format: None,
        parallel_tool_calls: None,
        rag: None,
        legacy_completion: true,
        untrusted_context: false,
    };

    run_chat(state, tenant, &headers, &body, chat).await
}
