use crate::app::AppState;
use crate::domain::embed::{EmbedRequest, EncodingFormat};
use crate::http::error::ApiError;
use crate::http::extract::TenantCtx;
use crate::http::idempotency::{self, Guard as IdemGuard};
use crate::http::wire::EmbeddingListResponse;
use crate::providers::CallCx;
use axum::Extension;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;

/// `POST /v1/embeddings`.
///
/// The body is taken raw so that the idempotency record can be keyed on its hash, for
/// the same reason `/v1/chat/completions` does it: embedding a large document costs real
/// money, clients retry on a dropped socket, and at-least-once delivery without a replay
/// record means paying twice for one set of vectors. This endpoint had no such record —
/// only chat did — which made the cheapest thing to retry the one thing you could not
/// retry safely.
pub async fn embeddings(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let embed_req: EmbedRequest =
        serde_json::from_slice(&body).map_err(|e| ApiError::InvalidRequest {
            message: e.to_string(),
            param: None,
        })?;

    let idem = match idempotency::key_from_headers(&headers) {
        Some(k) => match state.redis() {
            Some(redis) => {
                match idempotency::begin(redis, tenant.key.tenant.id, k, &body).await? {
                    // Replayed verbatim: the bytes we sent the first time, byte for byte.
                    idempotency::Outcome::Replay(stored) => return Ok(replay(stored)),
                    idempotency::Outcome::Proceed(guard) => Some(guard),
                }
            }
            // Without Redis there is no coordination between replicas, and a per-process
            // record would be worse than none: it would replay on one and re-run on another.
            None => None,
        },
        None => None,
    };

    let model = embed_req.model.clone();
    if state.registry.model_registry.get(&model).is_none() {
        return Err(ApiError::ModelNotFound { model });
    }

    if embed_req.input.is_empty() {
        return Err(ApiError::InvalidRequest {
            message: "input must not be empty".into(),
            param: Some("input".into()),
        });
    }

    // Validated before the upstream call, so a typo costs nothing.
    let format =
        EncodingFormat::parse(embed_req.encoding_format.as_deref()).map_err(|message| {
            ApiError::InvalidRequest {
                message,
                param: Some("encoding_format".into()),
            }
        })?;

    let cx = CallCx {
        request_id: tenant.request_id.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        deadline: tenant.deadline,
    };

    // Nothing has been spent yet. This is the last free moment to abandon a request whose
    // client has already stopped waiting.
    cx.check_deadline().map_err(super::chat::provider_err)?;

    // A provider that refuses the input shape (Ollama cannot embed token ids) is
    // the client's problem, not ours: it must surface as a 400, not a 500.
    let resp = state
        .registry
        .embed(&model, &embed_req, &cx)
        .await
        .map_err(embed_err)?;

    let body = EmbeddingListResponse::new(model, resp.data, resp.usage, format);
    commit_idempotent(idem, &body);
    Ok(axum::Json(body).into_response())
}

/// Stores the exact bytes we are about to return, so an identical retry replays them
/// rather than paying a provider to produce them again.
fn commit_idempotent(idem: Option<IdemGuard>, body: &EmbeddingListResponse) {
    let Some(guard) = idem else { return };
    if let Ok(json) = serde_json::to_string(body) {
        guard.commit_detached(json);
    }
}

/// Hands back a stored answer untouched.
///
/// Marked with `x-switchboard-replay`, because a replay returns the original `usage`
/// object with real token counts while the gateway bills nothing. A client that prices
/// its own spend from the body would otherwise charge itself for vectors that were free.
fn replay(stored: String) -> Response {
    let mut response = stored.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response.headers_mut().insert(
        "x-switchboard-replay",
        HeaderValue::from_static("idempotent"),
    );
    response
}

/// Delegates to the chat mapping so that `503`, `504` and `429` mean the same thing on
/// every endpoint, then re-labels the two errors whose `param` differs: an embedding
/// request's offending field is `input`, not `messages`.
fn embed_err(e: crate::providers::ProviderError) -> ApiError {
    use crate::providers::ProviderError as E;
    match e {
        E::BadRequest { param } => ApiError::InvalidRequest {
            message: "the provider for this model cannot accept that input".into(),
            param: param.or_else(|| Some("input".into())),
        },
        E::ContextLength { limit, got } => ApiError::InvalidRequest {
            message: format!("context length {got} exceeds limit {limit}"),
            param: Some("input".into()),
        },
        other => super::chat::provider_err(other),
    }
}
