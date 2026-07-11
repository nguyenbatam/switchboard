//! Monthly spend ceiling.
//!
//! Redis holds a per-tenant, per-month counter so the hot path is one `GET`.
//! Postgres is the ledger: on a counter miss (eviction, failover, a fresh replica)
//! the month is rebuilt from `usage_hourly` rather than restarting from zero,
//! which would hand every tenant a free month.

use crate::domain::tenant::Tenant;
use crate::http::error::ApiError;
use chrono::Datelike;
use deadpool_redis::Pool as RedisPool;
use redis::AsyncCommands;
use sqlx::PgPool;
use uuid::Uuid;

/// Counters outlive the month they describe by a wide margin, so a late-arriving
/// usage row still lands on the right key.
const KEY_TTL_SECS: u64 = 40 * 24 * 3600;

fn month_key(tenant_id: Uuid, now: chrono::DateTime<chrono::Utc>) -> String {
    format!("budget:{}:{:04}{:02}", tenant_id, now.year(), now.month())
}

/// The tenant's month-to-date position, when it has a budget at all.
///
/// Returned from [`check`] so the handler can put it on the response, because the whole
/// point of `remaining_nano` is that a client should not have to make a second call — or
/// hit a `429` mid-sentence — to learn it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetStatus {
    pub limit_nano: i64,
    pub spent_nano: i64,
}

impl BudgetStatus {
    /// May go negative: the check is a *pre*-check against already-recorded spend, so a
    /// tenant overshoots by roughly one in-flight request.
    pub fn remaining_nano(&self) -> i64 {
        self.limit_nano - self.spent_nano
    }
}

/// Rejects the request when the tenant has already spent its monthly allowance.
///
/// `fail_open` decides what an unreachable Redis means. Defaulting to open keeps
/// an infrastructure outage from looking like a billing outage; the alternative
/// is refusing paid traffic because a cache is down.
///
/// `Ok(None)` means the tenant has no budget: unlimited, and nothing to report.
pub async fn check(
    redis: Option<&RedisPool>,
    db: Option<&PgPool>,
    tenant: &Tenant,
    fail_open: bool,
) -> Result<Option<BudgetStatus>, ApiError> {
    let Some(limit) = tenant.monthly_budget_nano else {
        return Ok(None);
    };
    if limit <= 0 {
        return Ok(None);
    }

    let spent = match spend_so_far(redis, db, tenant.id).await {
        Ok(v) => v,
        Err(e) if fail_open => {
            tracing::warn!(error = %e, tenant = %tenant.id, "budget check failed, fail-open");
            return Ok(None);
        }
        Err(e) => {
            return Err(ApiError::Server {
                message: format!("budget check failed: {e}"),
            });
        }
    };

    if spent >= limit {
        crate::telemetry::metrics::record_ratelimit_denied("budget");
        return Err(ApiError::InsufficientQuota {
            message: format!(
                "monthly budget exhausted: spent {spent} of {limit} nanodollars. \
                 Raise the limit or wait for the next billing period."
            ),
        });
    }

    Ok(Some(BudgetStatus {
        limit_nano: limit,
        spent_nano: spent,
    }))
}

async fn spend_so_far(
    redis: Option<&RedisPool>,
    db: Option<&PgPool>,
    tenant_id: Uuid,
) -> Result<i64, Box<dyn std::error::Error + Send + Sync>> {
    let now = chrono::Utc::now();

    let Some(redis) = redis else {
        // No counter available; the ledger is the only source of truth.
        return match db {
            Some(pool) => Ok(month_spend_from_ledger(pool, tenant_id).await?),
            None => Ok(0),
        };
    };

    let key = month_key(tenant_id, now);
    let mut conn = redis.get().await?;

    if let Some(v) = conn.get::<_, Option<i64>>(&key).await? {
        return Ok(v);
    }

    // Counter miss. Rebuild from the ledger rather than assuming zero.
    let Some(pool) = db else { return Ok(0) };
    let rebuilt = month_spend_from_ledger(pool, tenant_id).await?;

    // NX: a concurrent request that lost the race already seeded the key, and its
    // value may include spend that landed after our SUM. Never clobber it.
    let _: Option<bool> = redis::cmd("SET")
        .arg(&key)
        .arg(rebuilt)
        .arg("NX")
        .arg("EX")
        .arg(KEY_TTL_SECS)
        .query_async(&mut *conn)
        .await?;

    tracing::info!(tenant = %tenant_id, rebuilt, "rebuilt budget counter from ledger");
    Ok(rebuilt)
}

async fn month_spend_from_ledger(pool: &PgPool, tenant_id: Uuid) -> Result<i64, sqlx::Error> {
    let (sum,): (i64,) = sqlx::query_as(
        r#"
        SELECT COALESCE(SUM(cost_nano), 0)::bigint
        FROM usage_hourly
        WHERE tenant_id = $1 AND hour >= date_trunc('month', now())
        "#,
    )
    .bind(tenant_id)
    .fetch_one(pool)
    .await?;
    Ok(sum)
}

/// Adds spend to the month counter and returns the new month-to-date total.
///
/// Best-effort: Postgres already has the row, so a lost increment self-heals on the next
/// counter rebuild. `None` on failure — and a caller that wanted to alert on a threshold
/// simply does not, rather than alerting on a number it does not have.
pub async fn add_spend(redis: &RedisPool, tenant_id: Uuid, cost_nano: i64) -> Option<i64> {
    let key = month_key(tenant_id, chrono::Utc::now());
    let result: Result<i64, Box<dyn std::error::Error + Send + Sync>> = async {
        let mut conn = redis.get().await?;
        let total: i64 = conn.incr(&key, cost_nano).await?;
        // A fresh key has no TTL; set one the first time we create it.
        if total == cost_nano {
            let _: bool = conn.expire(&key, KEY_TTL_SECS as i64).await?;
        }
        Ok(total)
    }
    .await;

    match result {
        Ok(total) => Some(total),
        Err(e) => {
            tracing::warn!(error = %e, tenant = %tenant_id, "failed to increment budget counter");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn month_key_is_stable_within_a_month_and_rolls_over() {
        let id = Uuid::nil();
        let jan_1 = chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let jan_31 = chrono::Utc
            .with_ymd_and_hms(2026, 1, 31, 23, 59, 59)
            .unwrap();
        let feb_1 = chrono::Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();

        assert_eq!(month_key(id, jan_1), month_key(id, jan_31));
        assert_ne!(month_key(id, jan_31), month_key(id, feb_1));
        assert!(month_key(id, feb_1).ends_with(":202602"));
    }

    #[tokio::test]
    async fn no_budget_configured_is_always_allowed() {
        let tenant = Tenant {
            id: Uuid::nil(),
            name: "t".into(),
            rpm_limit: 60,
            tpm_limit: 60_000,
            monthly_budget_nano: None,
        };
        assert!(check(None, None, &tenant, false).await.is_ok());
    }
}
