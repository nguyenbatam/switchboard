//! `/v1/keys` — a tenant managing its own API keys.
//!
//! Until now the only way to mint, rotate, or revoke a key was `sbctl`, on an operator's
//! machine. That makes the most time-critical security operation a partner ever performs —
//! *"a key leaked, kill it"* — into a support ticket with a human on the other end and a
//! response time measured in whatever timezone that human is in.
//!
//! Three rules hold this together, and each one prevents a specific way of getting it
//! wrong:
//!
//! - **The secret is returned exactly once.** `api_keys` stores an argon2 hash. There is no
//!   path back, and a `GET` that could recover a secret would make every backup a breach.
//! - **A caller may only grant scopes it already holds.** Otherwise a key with `keys` is a
//!   privilege-escalation primitive, not an administrative one.
//! - **Every statement is scoped by `tenant_id`.** Not as a filter — as the isolation
//!   boundary. There is no parameter that widens it.

use crate::app::AppState;
use crate::auth::key;
use crate::domain::tenant::SCOPE_KEYS;
use crate::http::error::ApiError;
use crate::http::extract::TenantCtx;
use crate::http::wire::keys::{
    CreateKeyRequest, CreatedKey, KeyListResponse, KeyObject, RotateKeyRequest, RotatedKey,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::{Extension, Json};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use sqlx::PgPool;
use uuid::Uuid;

const DEFAULT_SCOPES: [&str; 2] = ["chat", "embeddings"];
const MAX_KEY_NAME_LEN: usize = 128;

/// A key with the `keys` scope, or `403`.
///
/// `401` would be wrong: the caller *is* authenticated. Telling them to re-authenticate
/// sends them round a loop that cannot terminate.
fn require_keys_scope(tenant: &TenantCtx) -> Result<(), ApiError> {
    if tenant.key.has_scope(SCOPE_KEYS) {
        return Ok(());
    }
    Err(ApiError::Permission {
        message: format!(
            "this API key does not carry the `{SCOPE_KEYS}` scope, which /v1/keys requires"
        ),
    })
}

fn db(state: &AppState) -> Result<&PgPool, ApiError> {
    state.db.as_ref().ok_or_else(|| ApiError::Server {
        message: "key management requires postgres".into(),
    })
}

/// `POST /v1/keys`
pub async fn create_key(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Json(req): Json<CreateKeyRequest>,
) -> Result<impl IntoResponse, ApiError> {
    require_keys_scope(&tenant)?;
    let pool = db(&state)?;

    let name = validate_name(&req.name)?;
    let scopes = resolve_scopes(req.scopes, &tenant.key.scopes)?;
    let expires_at = resolve_expiry(req.expires_in_ms)?;

    let created = mint(pool, tenant.key.tenant.id, &name, &scopes, expires_at).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

/// `GET /v1/keys` — including revoked ones.
///
/// A revoked key is not deleted, and this lists it. "When was this revoked, and had it been
/// used since?" is the first question of any incident, and a list that hides its answer is
/// a list that makes you go to the database.
pub async fn list_keys(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
) -> Result<Json<KeyListResponse>, ApiError> {
    require_keys_scope(&tenant)?;
    let pool = db(&state)?;

    let rows = sqlx::query_as::<_, KeyObject>(
        r#"
        SELECT '' AS object, id, name, key_prefix, scopes, last_used_at, expires_at, revoked_at
        FROM api_keys
        WHERE tenant_id = $1
        ORDER BY id DESC
        "#,
    )
    .bind(tenant.key.tenant.id)
    .fetch_all(pool)
    .await
    .map_err(server)?;

    Ok(Json(KeyListResponse::new(rows)))
}

/// `DELETE /v1/keys/{id}`
///
/// Idempotent by design: revoking an already-revoked key is `204`, not `404`. A client
/// retrying a revocation after a dropped socket must not be told the key does not exist —
/// it would go looking for a key it had already successfully killed.
pub async fn revoke_key(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Path(key_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_keys_scope(&tenant)?;
    let pool = db(&state)?;

    if tenant.key.api_key_id == Some(key_id) {
        // Locking yourself out is not an operation this endpoint is for. `sbctl` still can.
        return Err(ApiError::InvalidRequest {
            message: "a key cannot revoke itself; use another key, or sbctl".into(),
            param: Some("id".into()),
        });
    }

    let found = sqlx::query(
        r#"
        UPDATE api_keys
        SET revoked_at = COALESCE(revoked_at, now())
        WHERE id = $1 AND tenant_id = $2
        "#,
    )
    .bind(key_id)
    .bind(tenant.key.tenant.id)
    .execute(pool)
    .await
    .map_err(server)?
    .rows_affected();

    if found == 0 {
        return Err(ApiError::NotFound {
            message: "no such key for this tenant".into(),
        });
    }

    // The verified-key cache is keyed on a token fingerprint we do not have. Without this,
    // this replica would keep accepting the key it just told the caller it had revoked.
    state.auth.mark_revoked(key_id).await;
    tracing::info!(tenant = %tenant.key.tenant.id, %key_id, "api key revoked");

    Ok(StatusCode::NO_CONTENT)
}

/// `POST /v1/keys/{id}/rotate`
///
/// Mints a replacement carrying the old key's name and scopes, then retires the old one.
///
/// `grace_ms` is the caller's deploy window, and only they know it. Zero — the default —
/// revokes immediately, which is what a leak calls for and an outage for every process
/// still holding the old key. A non-zero grace sets `expires_at` instead, so the old key
/// keeps working for exactly that long and then stops without anyone having to remember.
pub async fn rotate_key(
    State(state): State<AppState>,
    Extension(tenant): Extension<TenantCtx>,
    Path(key_id): Path<Uuid>,
    Json(req): Json<RotateKeyRequest>,
) -> Result<Json<RotatedKey>, ApiError> {
    require_keys_scope(&tenant)?;
    let pool = db(&state)?;

    let old = sqlx::query_as::<_, KeyObject>(
        r#"
        SELECT '' AS object, id, name, key_prefix, scopes, last_used_at, expires_at, revoked_at
        FROM api_keys
        WHERE id = $1 AND tenant_id = $2 AND revoked_at IS NULL
        "#,
    )
    .bind(key_id)
    .bind(tenant.key.tenant.id)
    .fetch_optional(pool)
    .await
    .map_err(server)?
    .ok_or_else(|| ApiError::NotFound {
        message: "no such active key for this tenant".into(),
    })?;

    let grace = ChronoDuration::milliseconds(req.grace_ms.unwrap_or(0).min(MAX_GRACE_MS) as i64);
    let old_key_expires_at = Utc::now() + grace;

    let new_key = mint(
        pool,
        tenant.key.tenant.id,
        &old.name,
        &old.scopes,
        old.expires_at,
    )
    .await?;

    if grace.is_zero() {
        sqlx::query("UPDATE api_keys SET revoked_at = now() WHERE id = $1 AND tenant_id = $2")
            .bind(key_id)
            .bind(tenant.key.tenant.id)
            .execute(pool)
            .await
            .map_err(server)?;
        state.auth.mark_revoked(key_id).await;
    } else {
        // Never *extend* an expiry that was already sooner than the grace we were asked for.
        sqlx::query(
            r#"
            UPDATE api_keys
            SET expires_at = LEAST(COALESCE(expires_at, $3), $3)
            WHERE id = $1 AND tenant_id = $2
            "#,
        )
        .bind(key_id)
        .bind(tenant.key.tenant.id)
        .bind(old_key_expires_at)
        .execute(pool)
        .await
        .map_err(server)?;
    }

    tracing::info!(
        tenant = %tenant.key.tenant.id,
        old = %key_id,
        new = %new_key.id,
        grace_ms = req.grace_ms.unwrap_or(0),
        "api key rotated"
    );

    Ok(Json(RotatedKey {
        object: "key_rotation".into(),
        new_key,
        revoked_key_id: key_id,
        old_key_expires_at,
    }))
}

/// A week. Long enough for any deploy, short enough that a forgotten rotation still ends.
const MAX_GRACE_MS: u64 = 7 * 24 * 3600 * 1000;

/// The longest expiry a key may be given, so `expires_in_ms` cannot overflow a timestamp.
const MAX_EXPIRY_MS: u64 = 10 * 365 * 24 * 3600 * 1000;

async fn mint(
    pool: &PgPool,
    tenant_id: Uuid,
    name: &str,
    scopes: &[String],
    expires_at: Option<DateTime<Utc>>,
) -> Result<CreatedKey, ApiError> {
    let (prefix, full_key) = key::generate_key();
    let secret = full_key
        .split_once('.')
        .map(|(_, s)| s)
        .ok_or_else(|| ApiError::Server {
            message: "generated key has no secret half".into(),
        })?;

    // Argon2 is tens of milliseconds of deliberate CPU. Off the runtime workers, for the
    // same reason the verify path is — see `auth::AuthService::authenticate`.
    let owned = secret.to_string();
    let hash = tokio::task::spawn_blocking(move || key::hash_secret(&owned))
        .await
        .map_err(|e| ApiError::Server {
            message: format!("key hashing task failed: {e}"),
        })?
        .map_err(|e| ApiError::Server {
            message: format!("failed to hash key: {e}"),
        })?;

    let id = Uuid::now_v7();
    let created_at: DateTime<Utc> = sqlx::query_scalar(
        r#"
        INSERT INTO api_keys (id, tenant_id, key_prefix, key_hash, name, scopes, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        RETURNING now()
        "#,
    )
    .bind(id)
    .bind(tenant_id)
    .bind(&prefix)
    .bind(&hash)
    .bind(name)
    .bind(scopes)
    .bind(expires_at)
    .fetch_one(pool)
    .await
    .map_err(server)?;

    Ok(CreatedKey {
        object: "api_key".into(),
        id,
        name: name.to_string(),
        key: full_key,
        key_prefix: prefix,
        scopes: scopes.to_vec(),
        expires_at,
        created_at,
    })
}

fn validate_name(name: &str) -> Result<String, ApiError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ApiError::InvalidRequest {
            message: "`name` must not be empty".into(),
            param: Some("name".into()),
        });
    }
    if trimmed.chars().count() > MAX_KEY_NAME_LEN {
        return Err(ApiError::InvalidRequest {
            message: format!("`name` must be at most {MAX_KEY_NAME_LEN} characters"),
            param: Some("name".into()),
        });
    }
    Ok(trimmed.to_string())
}

/// A caller may only grant what it already holds.
///
/// Without this check, one key with `keys` is enough to mint a key with any scope this
/// gateway ever adds — including ones that did not exist when the first key was issued.
fn resolve_scopes(
    requested: Option<Vec<String>>,
    caller_scopes: &[String],
) -> Result<Vec<String>, ApiError> {
    let Some(requested) = requested else {
        return Ok(DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect());
    };
    if requested.is_empty() {
        return Err(ApiError::InvalidRequest {
            message: "`scopes` must not be empty; omit it to accept the defaults".into(),
            param: Some("scopes".into()),
        });
    }

    if let Some(missing) = requested.iter().find(|s| !caller_scopes.contains(s)) {
        return Err(ApiError::Permission {
            message: format!(
                "cannot grant scope `{missing}`: this key does not hold it. \
                 A key may only issue scopes it already carries."
            ),
        });
    }
    Ok(requested)
}

fn resolve_expiry(expires_in_ms: Option<u64>) -> Result<Option<DateTime<Utc>>, ApiError> {
    let Some(ms) = expires_in_ms else {
        return Ok(None);
    };
    if ms == 0 || ms > MAX_EXPIRY_MS {
        return Err(ApiError::InvalidRequest {
            message: format!("`expires_in_ms` must be between 1 and {MAX_EXPIRY_MS}"),
            param: Some("expires_in_ms".into()),
        });
    }
    Ok(Some(Utc::now() + ChronoDuration::milliseconds(ms as i64)))
}

fn server(e: sqlx::Error) -> ApiError {
    ApiError::Server {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(scopes: &[&str]) -> Vec<String> {
        scopes.iter().map(|s| s.to_string()).collect()
    }

    /// The whole reason `resolve_scopes` exists. A key with `keys` is an administrative
    /// primitive, and must not become an escalation one.
    #[test]
    fn a_key_cannot_grant_a_scope_it_does_not_hold() {
        let caller = caller(&["keys", "chat"]);

        let err = resolve_scopes(Some(vec!["embeddings".into()]), &caller)
            .expect_err("embeddings is not held");
        assert_eq!(err.status(), http::StatusCode::FORBIDDEN);

        assert_eq!(
            resolve_scopes(Some(vec!["chat".into()]), &caller).unwrap(),
            vec!["chat".to_string()]
        );
    }

    /// Including `keys` itself: an admin key may issue another admin key, and nothing else
    /// may.
    #[test]
    fn only_a_keys_holder_can_mint_another_keys_holder() {
        assert!(resolve_scopes(Some(vec!["keys".into()]), &caller(&["chat"])).is_err());
        assert!(resolve_scopes(Some(vec!["keys".into()]), &caller(&["keys"])).is_ok());
    }

    #[test]
    fn omitting_scopes_takes_the_defaults_and_an_empty_list_is_an_error() {
        assert_eq!(
            resolve_scopes(None, &caller(&["keys"])).unwrap(),
            vec!["chat".to_string(), "embeddings".to_string()]
        );
        assert!(resolve_scopes(Some(vec![]), &caller(&["keys"])).is_err());
    }

    /// `403`, not `401`. The caller is authenticated; telling them to authenticate again
    /// sends them round a loop with no exit.
    #[test]
    fn a_key_without_the_scope_is_forbidden_not_unauthenticated() {
        use crate::domain::tenant::{AuthenticatedKey, Tenant};
        let ctx = TenantCtx {
            request_id: "r".into(),
            deadline: None,
            key: AuthenticatedKey {
                api_key_id: Some(Uuid::now_v7()),
                scopes: caller(&["chat"]),
                tenant: Tenant {
                    id: Uuid::nil(),
                    name: "t".into(),
                    rpm_limit: 60,
                    tpm_limit: 60_000,
                    monthly_budget_nano: None,
                },
            },
        };
        let err = require_keys_scope(&ctx).expect_err("chat is not keys");
        assert_eq!(err.status(), http::StatusCode::FORBIDDEN);
    }

    #[test]
    fn an_expiry_must_be_positive_and_bounded() {
        assert!(resolve_expiry(None).unwrap().is_none());
        assert!(resolve_expiry(Some(0)).is_err());
        assert!(resolve_expiry(Some(MAX_EXPIRY_MS + 1)).is_err());
        assert!(resolve_expiry(Some(60_000)).unwrap().is_some());
    }

    #[test]
    fn a_key_name_is_trimmed_bounded_and_never_empty() {
        assert_eq!(validate_name("  prod  ").unwrap(), "prod");
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"n".repeat(MAX_KEY_NAME_LEN + 1)).is_err());
    }
}
