use crate::app::AppState;
use crate::http::error::ApiError;
use crate::http::extract::TenantCtx;
use crate::http::wire::rag::{SearchHitWire, SearchResponse};
use crate::providers::CallCx;
use crate::rag::RagError;
use axum::Extension;
use axum::Json;
use axum::extract::State;

pub async fn search(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Json(body): Json<crate::http::wire::rag::SearchRequest>,
) -> Result<Json<SearchResponse>, ApiError> {
    let rag = state.rag.as_ref().ok_or(ApiError::Server {
        message: "RAG requires postgres".into(),
    })?;

    let cx = CallCx {
        request_id: tenant.request_id.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        deadline: tenant.deadline,
    };

    let hits = rag
        .search(
            tenant.key.tenant.id,
            &body.query,
            &body.model,
            body.k,
            body.mode,
            &cx,
        )
        .await
        .map_err(|e| match e {
            RagError::Embed(pe) => ApiError::Server {
                message: pe.to_string(),
            },
            other => ApiError::Server {
                message: other.to_string(),
            },
        })?;

    Ok(Json(SearchResponse {
        hits: hits.into_iter().map(SearchHitWire::from).collect(),
    }))
}
