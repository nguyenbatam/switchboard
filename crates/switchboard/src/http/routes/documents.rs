use crate::app::AppState;
use crate::http::error::ApiError;
use crate::http::extract::TenantCtx;
use crate::http::wire::rag::DocumentUploadResponse;
use crate::providers::CallCx;
use crate::rag::{IngestRequest, RagError};
use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use uuid::Uuid;

pub async fn upload_document(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Json(body): Json<crate::http::wire::rag::DocumentUploadRequest>,
) -> Result<Json<DocumentUploadResponse>, ApiError> {
    let rag = state.rag.as_ref().ok_or(ApiError::Server {
        message: "RAG requires postgres".into(),
    })?;

    let cx = CallCx {
        request_id: tenant.request_id.clone(),
        cancel: tokio_util::sync::CancellationToken::new(),
        deadline: tenant.deadline,
    };

    let result = rag
        .ingest(
            tenant.key.tenant.id,
            IngestRequest {
                title: body.title,
                content: body.content,
                source_uri: body.source_uri,
                metadata: body.metadata,
                embed_model: body.model,
                chunk_tokens: body.chunk_size,
                overlap_tokens: body.chunk_overlap,
            },
            &cx,
        )
        .await
        .map_err(rag_err)?;

    Ok(Json(DocumentUploadResponse {
        id: result.id,
        chunks: result.chunks,
        deduplicated: result.deduplicated,
    }))
}

pub async fn delete_document(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Path(doc_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let rag = state.rag.as_ref().ok_or(ApiError::Server {
        message: "RAG requires postgres".into(),
    })?;

    let deleted = rag
        .delete_document(tenant.key.tenant.id, doc_id)
        .await
        .map_err(rag_err)?;

    if deleted {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound {
            message: format!("document {doc_id} not found"),
        })
    }
}

fn rag_err(e: RagError) -> ApiError {
    match e {
        RagError::NoDatabase => ApiError::Server {
            message: "database unavailable".into(),
        },
        RagError::Embed(pe) => ApiError::Server {
            message: pe.to_string(),
        },
        RagError::Db(e) => ApiError::Server {
            message: e.to_string(),
        },
        RagError::Other(m) => ApiError::Server { message: m },
    }
}
