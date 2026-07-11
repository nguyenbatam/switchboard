//! `GET /v1/usage` — the gateway's ledger, as the tenant is allowed to see it.
//!
//! This is the authority on money. The client keeps its own book
//! ([`crate::economics::ledger`]) because aggregates cannot tell you what the p99 user
//! waited, but when the two disagree about *cost*, the gateway is right and the client has
//! a bug — or the gateway does, and that is worth knowing before the invoice arrives.
//!
//! Two things this endpoint does not do, both of which a real integration will want:
//!
//! `GET /v1/usage/records` is the per-call history: which request was charged, which was
//! free, and why. `/v1/usage` says *what the bill is*; only the history says *why*.
//!
//! One thing this endpoint still does not do: `/v1/usage` reports all-time totals with no
//! `?since=` and no per-model split. For a run that starts from a clean tenant a
//! before/after delta is exact, which is why the simulator takes one.

use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::client::GatewayClient;

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub struct UsageSnapshot {
    #[serde(default)]
    pub total_requests: u64,
    #[serde(default)]
    pub total_prompt_tokens: u64,
    #[serde(default)]
    pub total_completion_tokens: u64,
    /// Nanodollars. Never a float.
    #[serde(default)]
    pub total_cost_nano: i64,

    /// The ceiling. `None` means unlimited — which is a different thing from a ceiling of
    /// zero, and a client that renders it as `$0.00` will tell its operator it is broke.
    #[serde(default)]
    pub monthly_budget_nano: Option<i64>,
    #[serde(default)]
    pub month_spend_nano: i64,
    /// Can be negative: the gateway's budget check is a pre-check against already-recorded
    /// spend, so a tenant overshoots by roughly one in-flight request.
    #[serde(default)]
    pub remaining_nano: Option<i64>,
}

/// One call, from the gateway's own books.
#[derive(Debug, Clone, Deserialize)]
pub struct UsageRecord {
    pub id: String,
    pub request_id: String,
    pub model_requested: String,
    pub model_served: String,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    pub cost_nano: i64,
    /// `charged` | `free_cache_hit` | `free_error` | `free_zero_cost`.
    pub billing: String,
    /// `completed` | `client_disconnect` | `error`.
    pub status: String,
    #[serde(default)]
    pub error_kind: Option<String>,
    #[serde(default)]
    pub latency_ms: u64,
    /// The `user` this bot sent on the request, echoed back by the gateway.
    ///
    /// The join key for re-billing: a fleet's own customer, on the gateway's own row. It is
    /// how an operator answers "which of my users spent this" from one ledger instead of
    /// two that disagree.
    #[serde(default)]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct UsageRecordsPage {
    pub data: Vec<UsageRecord>,
    #[serde(default)]
    pub has_more: bool,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

impl GatewayClient {
    pub async fn usage(&self) -> Result<UsageSnapshot> {
        let resp = self
            .http()
            .get(format!("{}/v1/usage", self.config().base_url))
            .bearer_auth(&self.config().api_key)
            .send()
            .await
            .context("GET /v1/usage")?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET /v1/usage returned {status}");
        }
        resp.json::<UsageSnapshot>()
            .await
            .context("decoding /v1/usage")
    }

    /// One page of call history, newest first.
    pub async fn usage_records(
        &self,
        limit: u32,
        before: Option<&str>,
    ) -> Result<UsageRecordsPage> {
        self.usage_records_filtered(limit, before, None).await
    }

    /// The same page, narrowed to one of *our* end users.
    ///
    /// `?user=` narrows within the tenant and cannot widen past it. Asking for a user that
    /// belongs to somebody else returns an empty page, not somebody else's invoice.
    pub async fn usage_records_for_user(&self, user: &str, limit: u32) -> Result<UsageRecordsPage> {
        self.usage_records_filtered(limit, None, Some(user)).await
    }

    async fn usage_records_filtered(
        &self,
        limit: u32,
        before: Option<&str>,
        user: Option<&str>,
    ) -> Result<UsageRecordsPage> {
        let mut url = format!("{}/v1/usage/records?limit={limit}", self.config().base_url);
        if let Some(cursor) = before {
            url.push_str(&format!("&before={cursor}"));
        }
        if let Some(user) = user {
            url.push_str(&format!("&user={}", urlencode(user)));
        }

        let resp = self
            .http()
            .get(url)
            .bearer_auth(&self.config().api_key)
            .send()
            .await
            .context("GET /v1/usage/records")?;

        let status = resp.status();
        if !status.is_success() {
            anyhow::bail!("GET /v1/usage/records returned {status}");
        }
        resp.json::<UsageRecordsPage>()
            .await
            .context("decoding /v1/usage/records")
    }

    /// Walks the cursor until `max` rows or the history runs out.
    ///
    /// Stops on a repeated cursor rather than trusting the server to advance it. A
    /// pagination loop that trusts its peer is a pagination loop that hangs.
    pub async fn usage_history(&self, max: usize) -> Result<Vec<UsageRecord>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;

        while out.len() < max {
            let page = self.usage_records(200, cursor.as_deref()).await?;
            let empty = page.data.is_empty();
            out.extend(page.data);

            match page.next_cursor {
                Some(next) if page.has_more && Some(&next) != cursor.as_ref() && !empty => {
                    cursor = Some(next)
                }
                _ => break,
            }
        }
        out.truncate(max);
        Ok(out)
    }

    /// Waits for the ledger to stop moving.
    ///
    /// The usage row is written by a task the request path spawns and does not await —
    /// deliberately, so Postgres is never on the client's clock (`docs/15 §2`). The
    /// consequence is that immediately after the last answer, the ledger is still catching
    /// up. Reading it too early under-reports spend and makes the gateway look cheaper than
    /// it is, which is exactly the direction of error a report must not have.
    pub async fn usage_settled(
        &self,
        quiet_for: Duration,
        timeout: Duration,
    ) -> Result<UsageSnapshot> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut last = self.usage().await?;
        let mut stable_since = tokio::time::Instant::now();

        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;
            let now = self.usage().await?;
            if now == last {
                if stable_since.elapsed() >= quiet_for {
                    return Ok(now);
                }
            } else {
                last = now;
                stable_since = tokio::time::Instant::now();
            }
            if tokio::time::Instant::now() >= deadline {
                tracing::warn!(
                    "usage ledger never settled; reporting the last read. Spend may be under-counted."
                );
                return Ok(last);
            }
        }
    }
}

/// Percent-encodes what a query string cannot carry literally.
///
/// An end-user identifier is the *partner's* string, not ours. It can contain `&`, `=`, a
/// space, or anything else a person typed into a form — and a client that splices it raw
/// into a URL is a client that reads back somebody else's rows when a customer's id
/// happens to contain an ampersand.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A customer id containing `&user=` must not be able to re-write the query it lands in.
    #[test]
    fn an_end_user_id_cannot_smuggle_query_parameters() {
        assert_eq!(urlencode("u_42"), "u_42");
        assert_eq!(urlencode("a&user=b"), "a%26user%3Db");
        assert_eq!(urlencode("a b"), "a%20b");
        assert_eq!(urlencode("safe-._~"), "safe-._~");
    }

    /// Non-ASCII is bytes, not chars: a naive `char as u8` would corrupt every id a
    /// Vietnamese or Japanese customer chose for themselves.
    #[test]
    fn multibyte_ids_are_percent_encoded_per_byte() {
        assert_eq!(urlencode("é"), "%C3%A9");
    }
}
