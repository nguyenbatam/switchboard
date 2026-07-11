pub mod key;

use crate::config::AppConfig;
use crate::domain::tenant::{AuthenticatedKey, Tenant};
use crate::http::error::ApiError;
use moka::future::Cache;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

const LAST_USED_INTERVAL: Duration = Duration::from_secs(60);

/// How long a verified key stays trusted without re-hashing.
///
/// This is the revocation lag: a key revoked in Postgres keeps working for at most this
/// long. Sixty seconds buys back tens of milliseconds of argon2 on every request in
/// between, and "revoked within a minute" is what the `revoked_at` column was always
/// really promising.
const VERIFIED_TTL: Duration = Duration::from_secs(60);

/// Failures are cached for far less time than successes: long enough to blunt the
/// amplification described on [`AuthService::authenticate`], short enough that a client
/// which fixes its typo is not locked out.
const REJECTED_TTL: Duration = Duration::from_secs(10);

const CACHE_CAPACITY: u64 = 10_000;

pub struct AuthService {
    pool: Option<PgPool>,
    dev_tenant_id: Uuid,
    /// Applied to any tenant whose row leaves a limit unset (`<= 0`).
    ///
    /// `ratelimit.default_rpm` and `default_tpm` were declared in the config and read
    /// nowhere, so an unlimited tenant really was unlimited — and a per-tenant GCRA is the
    /// cheap first line of fairness: it bounds a tenant's *arrival rate* before the
    /// per-provider semaphore ever sees it. A tenant with two hundred open connections
    /// otherwise takes every upstream slot, and every other tenant's p99 becomes the
    /// noisiest one's queue depth.
    default_rpm: i32,
    default_tpm: i32,
    /// Keys whose `last_used_at` was refreshed recently, so a busy key does not
    /// generate one write per request for a column nobody reads more than daily.
    last_used_seen: Mutex<HashMap<Uuid, Instant>>,
    /// Verified tokens, keyed on `sha256(whole token)`.
    ///
    /// The *whole* token, never the prefix alone. The prefix is the non-secret half of
    /// `sk-sb-<prefix>.<secret>`, so keying on it would hand a cached success to anyone
    /// who supplied a known prefix and any secret at all.
    verified: Cache<[u8; 32], AuthenticatedKey>,
    /// Tokens that failed to verify, keyed the same way.
    rejected: Cache<[u8; 32], ()>,
    /// Key ids revoked through this process, checked on every `verified` hit.
    ///
    /// `verified` is keyed on a token fingerprint, so a revocation — which knows only a
    /// `key_id` — cannot reach into it. Without this, `DELETE /v1/keys/{id}` would answer
    /// `204` while the key it revoked kept working for up to [`VERIFIED_TTL`] on the very
    /// replica that processed the revocation. That is the one place a stale cache is
    /// indefensible: an operator revoking a leaked key is watching.
    ///
    /// Other replicas still take up to [`VERIFIED_TTL`] to notice, and that bound is the
    /// documented contract. Making revocation instant cluster-wide costs a Redis round trip
    /// on every authenticated request, which is the cost this cache exists to avoid.
    revoked: Cache<Uuid, ()>,
}

fn token_fingerprint(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

impl AuthService {
    pub fn new(config: &AppConfig, pool: Option<PgPool>) -> Self {
        Self {
            pool,
            dev_tenant_id: Uuid::parse_str(&config.auth.dev_tenant_id)
                .expect("valid dev_tenant_id in config"),
            default_rpm: config.ratelimit.default_rpm as i32,
            default_tpm: config.ratelimit.default_tpm as i32,
            last_used_seen: Mutex::new(HashMap::new()),
            verified: Cache::builder()
                .time_to_live(VERIFIED_TTL)
                .max_capacity(CACHE_CAPACITY)
                .build(),
            rejected: Cache::builder()
                .time_to_live(REJECTED_TTL)
                .max_capacity(CACHE_CAPACITY)
                .build(),
            // Outlives `verified`, or a revoked key would reappear when its tombstone
            // expired before the entry it was suppressing.
            revoked: Cache::builder()
                .time_to_live(VERIFIED_TTL * 2)
                .max_capacity(CACHE_CAPACITY)
                .build(),
        }
    }

    /// Records that a key was used, at most once a minute per key.
    ///
    /// The column exists to answer "is this key still in use, can we revoke it?".
    /// That question tolerates a minute of staleness; a write on every request does
    /// not tolerate production traffic.
    fn touch_last_used(&self, key_id: Uuid, pool: &PgPool) {
        {
            let mut seen = self
                .last_used_seen
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(at) = seen.get(&key_id)
                && at.elapsed() < LAST_USED_INTERVAL
            {
                return;
            }
            seen.insert(key_id, Instant::now());
        }

        let pool = pool.clone();
        tokio::spawn(async move {
            let result = sqlx::query("UPDATE api_keys SET last_used_at = now() WHERE id = $1")
                .bind(key_id)
                .execute(&pool)
                .await;
            if let Err(e) = result {
                tracing::debug!(error = %e, "failed to record api key last_used_at");
            }
        });
    }

    /// Verifies a bearer token, hashing at most once per key per [`VERIFIED_TTL`].
    ///
    /// Three things changed together here, and any two without the third are worthless.
    ///
    /// **The cache.** `Argon2::default()` is Argon2id at `m_cost = 19 MiB`, `t_cost = 2`.
    /// It is *designed* to cost tens of milliseconds. Uncached on every request, it was
    /// the p99 of the entire gateway.
    ///
    /// **`spawn_blocking` on the miss.** Argon2 is CPU-bound and never yields. Tokio runs
    /// one worker per core, so eight concurrent verifies on an eight-core box occupied
    /// every worker thread and nothing else ran — not `/v1`, not the timer that would
    /// fire a timeout, and not `/health`. The configured `concurrency_limit` of 256 was a
    /// fiction; the real limit was the core count. This is why moving the ops routes
    /// outside the limiter is necessary but not sufficient: permission to run is not a
    /// thread to run on.
    ///
    /// **The negative cache.** Argon2 runs only after the `key_prefix` row is found, but
    /// the prefix is the non-secret half of the token. Anyone who has seen one in a log
    /// can force an unbounded number of verifies with wrong secrets, and a positive cache
    /// does nothing about that.
    ///
    /// The argon2 parameters are not lowered. They defend the hashes at rest; the
    /// placement was what was wrong.
    pub async fn authenticate(&self, header: &str) -> Result<AuthenticatedKey, ApiError> {
        let token = key::parse_bearer(header).ok_or(ApiError::Authentication)?;
        let (prefix, secret) = key::split_key(token).ok_or(ApiError::Authentication)?;

        let fingerprint = token_fingerprint(token);

        if self.rejected.get(&fingerprint).await.is_some() {
            return Err(ApiError::Authentication);
        }
        if let Some(hit) = self.verified.get(&fingerprint).await {
            match hit.api_key_id {
                Some(id) if self.revoked.get(&id).await.is_some() => {
                    // Revoked since we cached it. Evict rather than merely refuse, so the
                    // next request pays a Postgres lookup and learns the same thing from
                    // the source of truth.
                    self.verified.invalidate(&fingerprint).await;
                    return Err(ApiError::Authentication);
                }
                Some(id) => {
                    if let Some(pool) = &self.pool {
                        self.touch_last_used(id, pool);
                    }
                }
                None => {}
            }
            return Ok(hit);
        }

        let Some(pool) = &self.pool else {
            return Err(ApiError::Authentication);
        };

        let row = sqlx::query_as::<_, ApiKeyRow>(
            r#"
            SELECT k.id, k.tenant_id, k.key_hash, k.scopes,
                   t.name, t.rpm_limit, t.tpm_limit, t.monthly_budget_nano
            FROM api_keys k
            JOIN tenants t ON t.id = k.tenant_id
            WHERE k.key_prefix = $1
              AND k.revoked_at IS NULL
              AND (k.expires_at IS NULL OR k.expires_at > now())
              AND t.status = 'active'
            "#,
        )
        .bind(prefix)
        .fetch_optional(pool)
        .await
        .map_err(|e| ApiError::Server {
            message: e.to_string(),
        })?;

        let Some(row) = row else {
            self.rejected.insert(fingerprint, ()).await;
            return Err(ApiError::Authentication);
        };

        // The only place argon2 runs, and it runs off the runtime workers.
        let hash = row.key_hash.clone();
        let secret = secret.to_string();
        let verified = tokio::task::spawn_blocking(move || key::verify_secret(&hash, &secret))
            .await
            .map_err(|e| ApiError::Server {
                message: format!("password verification task failed: {e}"),
            })?;

        if !verified {
            self.rejected.insert(fingerprint, ()).await;
            return Err(ApiError::Authentication);
        }

        self.touch_last_used(row.id, pool);

        let key = AuthenticatedKey {
            api_key_id: Some(row.id),
            tenant: Tenant {
                id: row.tenant_id,
                name: row.name,
                rpm_limit: self.or_default(row.rpm_limit, self.default_rpm),
                tpm_limit: self.or_default(row.tpm_limit, self.default_tpm),
                monthly_budget_nano: row.monthly_budget_nano,
            },
            scopes: row.scopes,
        };
        self.verified.insert(fingerprint, key.clone()).await;
        Ok(key)
    }

    /// Marks a key id revoked, so the cached `AuthenticatedKey` behind it stops working
    /// immediately on this replica.
    ///
    /// Other replicas notice within [`VERIFIED_TTL`], when their own cached entry expires
    /// and the `revoked_at IS NULL` clause in the lookup refuses to return the row.
    pub async fn mark_revoked(&self, key_id: Uuid) {
        self.revoked.insert(key_id, ()).await;
    }

    /// A tenant row that names no limit gets the configured default, not infinity.
    fn or_default(&self, from_row: i32, default: i32) -> i32 {
        if from_row > 0 { from_row } else { default }
    }

    pub fn dev_tenant(&self) -> AuthenticatedKey {
        AuthenticatedKey {
            api_key_id: None,
            tenant: Tenant {
                id: self.dev_tenant_id,
                name: "dev".into(),
                rpm_limit: self.default_rpm,
                tpm_limit: self.default_tpm,
                monthly_budget_nano: None,
            },
            scopes: vec!["chat".into(), "embeddings".into()],
        }
    }
}

#[derive(sqlx::FromRow)]
struct ApiKeyRow {
    id: Uuid,
    tenant_id: Uuid,
    key_hash: String,
    scopes: Vec<String>,
    name: String,
    rpm_limit: i32,
    tpm_limit: i32,
    monthly_budget_nano: Option<i64>,
}

pub type SharedAuth = Arc<AuthService>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Keying the cache on the prefix would accept any secret once one request with that
    /// prefix had succeeded. It is keyed on the whole token for exactly this reason.
    #[test]
    fn the_fingerprint_covers_the_secret_not_just_the_prefix() {
        let a = token_fingerprint("sk-sb-abcdefgh.secretone");
        let b = token_fingerprint("sk-sb-abcdefgh.secrettwo");
        assert_ne!(a, b);
        assert_eq!(a, token_fingerprint("sk-sb-abcdefgh.secretone"));
    }
}
