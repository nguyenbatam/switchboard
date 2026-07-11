use crate::app::AppState;
use crate::http::error::ApiError;
use crate::http::extract::TenantCtx;
use crate::http::wire::{UsageRecordsResponse, UsageResponse};
use axum::Extension;
use axum::Json;
use axum::extract::{Query, State};
use serde::Deserialize;
use uuid::Uuid;

pub async fn get_usage(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
) -> Result<Json<UsageResponse>, ApiError> {
    let tenant_id = tenant.key.tenant.id;

    let summary = state
        .usage
        .tenant_summary(tenant_id)
        .await
        .unwrap_or_default();
    let month_spend = state.usage.month_spend(tenant_id).await.unwrap_or(0);

    Ok(Json(UsageResponse::new(
        tenant_id,
        summary.requests,
        summary.prompt_tokens,
        summary.completion_tokens,
        summary.cost_nano,
        tenant.key.tenant.monthly_budget_nano,
        month_spend,
    )))
}

/// A page size large enough to be useful and small enough that nobody can ask the fact
/// table for a million rows and call it an API call.
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 500;

#[derive(Debug, Deserialize)]
pub struct RecordsQuery {
    limit: Option<i64>,
    /// Keyset cursor: return rows strictly older than this id.
    before: Option<Uuid>,
    /// Narrow to one of the caller's own end users — the `user` they sent on the request.
    ///
    /// It narrows *within* the tenant, and cannot widen past it: a tenant asking for
    /// another tenant's `user` gets an empty page, not somebody else's invoice.
    user: Option<String>,
}

/// `GET /v1/usage/records` — which calls were made, and which of them cost money.
///
/// The aggregate in `/v1/usage` cannot answer "why is my bill this number". This can:
/// every row carries the model that answered, the status, and a `billing` field that says
/// in words whether it was charged and, if not, why not.
///
/// With `?user=`, it answers the question one level down: *which of my users spent this*.
/// A partner re-billing their own customers needs that, and the alternative — keeping a
/// second ledger of their own and hoping it agrees with ours — is how two systems come to
/// disagree about money.
///
/// Scoped to the calling tenant by `WHERE tenant_id = $1`. There is no parameter to widen
/// it, and there must not be.
pub async fn get_usage_records(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Query(q): Query<RecordsQuery>,
) -> Result<Json<UsageRecordsResponse>, ApiError> {
    let limit = q.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::InvalidRequest {
            message: format!("limit must be between 1 and {MAX_LIMIT}"),
            param: Some("limit".into()),
        });
    }

    // Validated with the same rule the write path uses, so `?user=` can never match rows
    // that `POST /v1/chat/completions` would have refused to write.
    let end_user = crate::http::routes::chat::validate_end_user(q.user.as_deref())?;

    let (rows, has_more) = state
        .usage
        .recent_records(tenant.key.tenant.id, limit, q.before, end_user.as_deref())
        .await
        .ok_or_else(|| ApiError::Server {
            message: "usage history is unavailable".into(),
        })?;

    Ok(Json(UsageRecordsResponse::new(rows, has_more)))
}
