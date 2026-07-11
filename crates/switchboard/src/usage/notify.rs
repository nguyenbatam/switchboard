//! Telling somebody, before the tap closes.
//!
//! A tenant with a monthly budget discovers it the same way everyone discovers a wall: by
//! walking into it. `GET /v1/usage` has always exposed `remaining_nano`, but that requires
//! a client to poll a number that is uninteresting 99% of the time, and the 1% arrives
//! mid-sentence as a `429 insufficient_quota`.
//!
//! So the gateway pushes. When month-to-date spend crosses a configured fraction of the
//! budget, it POSTs once — and only once per tenant, per month, per threshold.
//!
//! **The URL is the operator's, never the tenant's.** See `config::BillingConfig`: a
//! tenant-supplied callback is a server-side request forgery primitive pointed at the
//! inside of your own cluster.

use deadpool_redis::Pool as RedisPool;
use serde::Serialize;
use std::time::Duration;
use uuid::Uuid;

/// Counters and dedupe markers outlive the month they describe, so a late-arriving usage
/// row still lands on the right key.
const NOTIFIED_TTL_SECS: u64 = 40 * 24 * 3600;

pub struct BudgetNotifier {
    client: reqwest::Client,
    url: String,
    /// Ascending fractions of the budget. `0.8`, `0.9`, `1.0`.
    thresholds: Vec<f64>,
}

/// What lands on the operator's endpoint.
///
/// `tenant_id` rather than any tenant-supplied address, because fanning out to a customer
/// is the operator's job and they already have a way to do it.
#[derive(Debug, Serialize)]
pub struct BudgetAlert {
    pub event: &'static str,
    pub tenant_id: Uuid,
    /// The fraction crossed: `0.8`, `0.9`, `1.0`.
    pub threshold: f64,
    pub monthly_budget_nano: i64,
    pub month_spend_nano: i64,
    /// Negative once the budget is blown. The check is a pre-check; see
    /// [`super::budget::BudgetStatus::remaining_nano`].
    pub remaining_nano: i64,
}

impl BudgetNotifier {
    /// `None` when no URL is configured, which is the default.
    pub fn new(cfg: &crate::config::BillingConfig) -> Option<Self> {
        let url = cfg.budget_webhook_url.clone()?;

        let mut thresholds: Vec<f64> = cfg
            .budget_thresholds
            .iter()
            .copied()
            .filter(|t| *t > 0.0)
            .collect();
        thresholds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if thresholds.is_empty() {
            return None;
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(cfg.webhook_timeout_ms))
            .build()
            .ok()?;

        Some(Self {
            client,
            url,
            thresholds,
        })
    }

    /// The highest threshold that `spent/limit` has reached, or `None`.
    ///
    /// Highest, not lowest: a single large request can cross 80% and 90% at once, and
    /// firing twice for one event teaches an operator to filter the alert.
    fn crossed(&self, spent: i64, limit: i64) -> Option<f64> {
        if limit <= 0 {
            return None;
        }
        let ratio = spent as f64 / limit as f64;
        self.thresholds.iter().rev().copied().find(|t| ratio >= *t)
    }

    /// Fires at most once per `(tenant, month, threshold)`.
    ///
    /// The dedupe is a Redis `SET NX`, which makes it correct across replicas: exactly one
    /// of N pods that observe the same crossing wins the key and posts. Without it, every
    /// replica notices independently and the operator's endpoint receives N copies of the
    /// same fact.
    ///
    /// Best-effort throughout. A budget alert we failed to deliver is a worse morning; a
    /// budget alert that could fail a request would be a worse product.
    pub async fn maybe_notify(&self, redis: &RedisPool, tenant_id: Uuid, spent: i64, limit: i64) {
        let Some(threshold) = self.crossed(spent, limit) else {
            return;
        };

        let now = chrono::Utc::now();
        let key = notified_key(tenant_id, now, threshold);

        let claimed: Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> = async {
            let mut conn = redis.get().await?;
            let won: Option<String> = redis::cmd("SET")
                .arg(&key)
                .arg(1)
                .arg("NX")
                .arg("EX")
                .arg(NOTIFIED_TTL_SECS)
                .query_async(&mut *conn)
                .await?;
            Ok(won)
        }
        .await;

        match claimed {
            // Somebody else already sent it, or Redis is unreachable. In the second case we
            // stay quiet rather than have every replica shout: an unreachable Redis already
            // has an alert of its own.
            Ok(None) | Err(_) => return,
            Ok(Some(_)) => {}
        }

        let alert = BudgetAlert {
            event: "budget.threshold_crossed",
            tenant_id,
            threshold,
            monthly_budget_nano: limit,
            month_spend_nano: spent,
            remaining_nano: limit - spent,
        };

        tracing::warn!(
            tenant = %tenant_id,
            threshold,
            spent,
            limit,
            "tenant crossed a budget threshold"
        );

        match self.client.post(&self.url).json(&alert).send().await {
            Ok(resp) if resp.status().is_success() => {}
            Ok(resp) => {
                tracing::warn!(status = %resp.status(), tenant = %tenant_id, "budget webhook rejected")
            }
            Err(e) => tracing::warn!(error = %e, tenant = %tenant_id, "budget webhook failed"),
        }
    }
}

/// Basis points in the key, not a float: `0.8` has no exact decimal spelling and two
/// replicas that formatted it differently would each think they had won the `SET NX`.
fn notified_key(tenant_id: Uuid, now: chrono::DateTime<chrono::Utc>, threshold: f64) -> String {
    use chrono::Datelike;
    let bps = (threshold * 10_000.0).round() as i64;
    format!(
        "budget:notified:{}:{:04}{:02}:{}",
        tenant_id,
        now.year(),
        now.month(),
        bps
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn notifier(thresholds: Vec<f64>) -> BudgetNotifier {
        BudgetNotifier {
            client: reqwest::Client::new(),
            url: "http://operator.invalid/hook".into(),
            thresholds,
        }
    }

    /// One request can cross two thresholds. Firing for both teaches the operator to filter
    /// the alert, which is how the 100% one gets missed.
    #[test]
    fn a_single_leap_past_two_thresholds_reports_only_the_highest() {
        let n = notifier(vec![0.8, 0.9, 1.0]);
        assert_eq!(n.crossed(950, 1000), Some(0.9));
        assert_eq!(n.crossed(1_000_000, 1000), Some(1.0), "way over");
        assert_eq!(n.crossed(800, 1000), Some(0.8), "exactly at 80%");
    }

    #[test]
    fn below_every_threshold_is_silence() {
        let n = notifier(vec![0.8, 0.9, 1.0]);
        assert_eq!(n.crossed(799, 1000), None);
        assert_eq!(n.crossed(0, 1000), None);
    }

    /// A tenant with no budget has no thresholds to cross, and dividing by its limit would
    /// be dividing by zero.
    #[test]
    fn an_unlimited_tenant_never_crosses_anything() {
        let n = notifier(vec![0.8]);
        assert_eq!(n.crossed(1_000_000, 0), None);
        assert_eq!(n.crossed(1_000_000, -1), None);
    }

    /// The dedupe key must be identical on every replica and must roll over with the month.
    #[test]
    fn the_dedupe_key_is_stable_within_a_month_and_exact_about_the_threshold() {
        let id = Uuid::nil();
        let jan = chrono::Utc.with_ymd_and_hms(2026, 1, 5, 0, 0, 0).unwrap();
        let jan_late = chrono::Utc.with_ymd_and_hms(2026, 1, 31, 23, 0, 0).unwrap();
        let feb = chrono::Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();

        assert_eq!(notified_key(id, jan, 0.8), notified_key(id, jan_late, 0.8));
        assert_ne!(notified_key(id, jan, 0.8), notified_key(id, feb, 0.8));
        assert_ne!(notified_key(id, jan, 0.8), notified_key(id, jan, 0.9));
        assert!(notified_key(id, jan, 0.8).ends_with(":8000"));
        assert!(notified_key(id, jan, 1.0).ends_with(":10000"));
    }

    /// Thresholds may be configured in any order; `crossed` walks them descending.
    #[test]
    fn configured_thresholds_are_sorted_regardless_of_how_they_were_written() {
        let cfg = crate::config::BillingConfig {
            budget_webhook_url: Some("http://x.invalid".into()),
            budget_thresholds: vec![1.0, 0.5, 0.9],
            webhook_timeout_ms: 1000,
        };
        let n = BudgetNotifier::new(&cfg).expect("a url and thresholds make a notifier");
        assert_eq!(n.thresholds, vec![0.5, 0.9, 1.0]);
        assert_eq!(n.crossed(950, 1000), Some(0.9));
    }

    #[test]
    fn no_url_means_no_notifier() {
        let cfg = crate::config::BillingConfig::default();
        assert!(BudgetNotifier::new(&cfg).is_none());
    }
}
