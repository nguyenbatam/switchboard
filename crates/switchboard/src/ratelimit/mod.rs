pub mod gcra;

use crate::http::error::{ApiError, RateLimitScope};
use deadpool_redis::Pool;
use std::sync::Arc;
use uuid::Uuid;

pub struct RateLimiter {
    pool: Pool,
    fail_open: bool,
}

/// One limiter's view of a tenant, on its way to `x-ratelimit-*`.
///
/// A client that cannot read its own limit discovers it by hitting it, mid-sentence,
/// as a `429`. One that can read it throttles first. Both dimensions are reported
/// separately because they are exceeded separately and the remedies differ.
#[derive(Debug, Clone, Copy)]
pub struct RateLimitSnapshot {
    pub scope: RateLimitScope,
    pub limit: u32,
    pub remaining: u32,
    pub reset_ms: u64,
}

/// Rides the response so a middleware can render the headers. Two entries at most:
/// requests and tokens.
#[derive(Debug, Clone, Default)]
pub struct RateLimitHeaders(pub Vec<RateLimitSnapshot>);

/// A token reservation that has not been settled yet.
///
/// Admission has to charge the worst case — `estimated_prompt + max_tokens` —
/// because the response length is unknowable up front. Left unsettled, a tenant
/// with the 4096-token default is limited at a small fraction of its real quota.
/// [`RateLimiter::settle_tokens`] refunds the difference once the answer is known.
#[derive(Debug, Clone, Copy)]
pub struct TokenReservation {
    tenant_id: Uuid,
    tpm_limit: i32,
    reserved: u32,
}

impl RateLimiter {
    pub fn new(pool: Pool, fail_open: bool) -> Self {
        Self { pool, fail_open }
    }

    pub async fn check_request(
        &self,
        tenant_id: &Uuid,
        rpm_limit: i32,
    ) -> Result<Option<RateLimitSnapshot>, ApiError> {
        let key = format!("rl:req:{tenant_id}");
        self.consume(&key, 1, rpm_limit as u32, 60_000, RateLimitScope::Requests)
            .await
    }

    /// Pre-auth flood protection. An unauthenticated request is free to send, and
    /// the most expensive thing it can reach is the argon2 verify on the auth path.
    pub async fn check_ip(&self, ip: &str, ip_rpm: u32) -> Result<(), ApiError> {
        let key = format!("rl:ip:{ip}");
        self.consume(&key, 1, ip_rpm, 60_000, RateLimitScope::Ip)
            .await?;
        Ok(())
    }

    pub async fn reserve_tokens(
        &self,
        tenant_id: &Uuid,
        tpm_limit: i32,
        tokens: u32,
    ) -> Result<(TokenReservation, Option<RateLimitSnapshot>), ApiError> {
        let key = format!("rl:tok:{tenant_id}");
        let snapshot = self
            .consume(
                &key,
                tokens as i64,
                tpm_limit as u32,
                60_000,
                RateLimitScope::Tokens,
            )
            .await?;
        Ok((
            TokenReservation {
                tenant_id: *tenant_id,
                tpm_limit,
                reserved: tokens,
            },
            snapshot,
        ))
    }

    /// Refunds `reserved - actual` back into the bucket. Best-effort: an unrefunded
    /// reservation ages out of the GCRA window on its own within one period.
    pub async fn settle_tokens(&self, reservation: TokenReservation, actual: u32) {
        let refund = reservation.reserved.saturating_sub(actual);
        if refund == 0 {
            return;
        }
        let key = format!("rl:tok:{}", reservation.tenant_id);
        if let Err(e) = self
            .consume(
                &key,
                -(refund as i64),
                reservation.tpm_limit as u32,
                60_000,
                RateLimitScope::Tokens,
            )
            .await
        {
            tracing::warn!(
                error = %e,
                tenant = %reservation.tenant_id,
                refund,
                "token refund failed"
            );
        }
    }

    async fn consume(
        &self,
        key: &str,
        cost: i64,
        capacity: u32,
        period_ms: u64,
        scope: RateLimitScope,
    ) -> Result<Option<RateLimitSnapshot>, ApiError> {
        if capacity == 0 {
            return Ok(None);
        }

        let result = gcra::check(&self.pool, key, cost, capacity, period_ms).await;

        match result {
            Ok(gcra::GcraResult::Allowed(bucket)) => Ok(Some(RateLimitSnapshot {
                scope,
                limit: capacity,
                remaining: bucket.remaining,
                reset_ms: bucket.reset_ms,
            })),
            Ok(gcra::GcraResult::Denied {
                retry_after_ms,
                bucket,
            }) => {
                crate::telemetry::metrics::record_ratelimit_denied(scope.dimension());
                Err(ApiError::RateLimit {
                    retry_after_ms,
                    scope,
                    snapshot: Some(RateLimitSnapshot {
                        scope,
                        limit: capacity,
                        remaining: bucket.remaining,
                        reset_ms: bucket.reset_ms.max(retry_after_ms.unwrap_or(0)),
                    }),
                })
            }
            // Fail-open on an unreachable Redis: an infrastructure outage must not
            // look like a billing outage. No snapshot, because we know nothing.
            Err(e) if self.fail_open => {
                tracing::warn!(error = %e, key, "rate limit check failed, fail-open");
                Ok(None)
            }
            Err(e) => Err(ApiError::Server {
                message: e.to_string(),
            }),
        }
    }
}

pub type SharedRateLimiter = Arc<RateLimiter>;
