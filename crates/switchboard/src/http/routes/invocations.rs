use crate::agent::invocations::InvocationSummary;
use crate::app::AppState;
use crate::http::error::ApiError;
use crate::http::extract::TenantCtx;
use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct InvocationsResponse {
    pub object: &'static str,
    pub request_id: String,
    pub data: Vec<InvocationSummary>,
}

/// The tool calls the agent made while serving one request.
///
/// Scoped to the caller's tenant, so a request id guessed from someone else's logs
/// reveals nothing. Read-only. Tool *results* are withheld — they can be large and
/// they can contain retrieved third-party content — but the arguments are the
/// caller's own request, and without them "did it call the right tool" cannot be
/// distinguished from "did it call the right tool correctly".
pub async fn get_invocations(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Path(request_id): Path<String>,
) -> Result<Json<InvocationsResponse>, ApiError> {
    let data = state
        .tool_invocations
        .for_request(tenant.key.tenant.id, &request_id)
        .await
        .map_err(|e| ApiError::Server {
            message: e.to_string(),
        })?;

    Ok(Json(InvocationsResponse {
        object: "list",
        request_id,
        data,
    }))
}
