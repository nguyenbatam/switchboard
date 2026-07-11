use serde::Serialize;
use uuid::Uuid;

use crate::usage::UsageRecordRow;

#[derive(Debug, Clone, Serialize)]
pub struct UsageResponse {
    pub object: String,
    pub tenant_id: Uuid,
    pub total_requests: u64,
    pub total_prompt_tokens: u64,
    pub total_completion_tokens: u64,
    /// All-time, nanodollars.
    pub total_cost_nano: i64,

    /// The ceiling this tenant is enforced against. `null` means unlimited.
    ///
    /// Exposed because a client that cannot read its own limit can only discover it by
    /// hitting it — a `429 insufficient_quota` in the middle of a user's sentence. A bot
    /// that can see the balance can warn, throttle, or top up first.
    pub monthly_budget_nano: Option<i64>,
    /// Month-to-date spend, the number `budget::check` compares against the ceiling.
    pub month_spend_nano: i64,
    /// `monthly_budget_nano - month_spend_nano`. `null` when unlimited. May go negative:
    /// the budget check is a pre-check against already-recorded spend, so a tenant can
    /// overshoot by roughly one in-flight request.
    pub remaining_nano: Option<i64>,
}

impl UsageResponse {
    pub fn new(
        tenant_id: Uuid,
        total_requests: u64,
        total_prompt_tokens: u64,
        total_completion_tokens: u64,
        total_cost_nano: i64,
        monthly_budget_nano: Option<i64>,
        month_spend_nano: i64,
    ) -> Self {
        Self {
            object: "usage".into(),
            tenant_id,
            total_requests,
            total_prompt_tokens,
            total_completion_tokens,
            total_cost_nano,
            monthly_budget_nano,
            month_spend_nano,
            remaining_nano: monthly_budget_nano.map(|limit| limit - month_spend_nano),
        }
    }
}

/// Why a request cost what it cost.
///
/// Money in this system is only ever charged for tokens a provider actually generated.
/// There is no refund, because there is nothing to reverse: a request that produced no
/// upstream tokens was never charged in the first place. Making that visible is the whole
/// purpose of this field — "charged: false" with a reason beats a zero a client has to
/// explain to itself.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// Tokens were generated upstream and billed.
    Charged,
    /// Served from cache. No provider was called, so nothing was spent.
    FreeCacheHit,
    /// The request failed before or during generation. Errors are never billed.
    FreeError,
    /// Completed, but generated nothing chargeable — a priced-at-zero local model, or an
    /// empty completion.
    FreeZeroCost,
}

impl Billing {
    fn classify(row: &UsageRecordRow) -> Self {
        if row.cost_nano > 0 {
            Billing::Charged
        } else if row.cache_hit.is_some() {
            Billing::FreeCacheHit
        } else if row.status == "error" {
            Billing::FreeError
        } else {
            Billing::FreeZeroCost
        }
    }
}

/// One call, as the tenant that made it is allowed to see it.
#[derive(Debug, Clone, Serialize)]
pub struct UsageRecord {
    pub id: Uuid,
    /// Echoes the caller's `x-request-id`, so their logs join to this row.
    pub request_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub endpoint: String,

    /// What was asked for.
    pub model_requested: String,
    /// What actually answered. Differs when a fallback fired ([docs/08 §3]).
    pub model_served: String,
    pub provider: String,

    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub cached_tokens: u32,

    /// Nanodollars. Zero unless `billing == "charged"`.
    pub cost_nano: i64,
    pub billing: Billing,

    /// `completed`, `client_disconnect`, or `error`.
    ///
    /// A `client_disconnect` **is** billed for the tokens generated before the socket
    /// died. The provider produced them; somebody pays.
    pub status: String,
    pub error_kind: Option<String>,
    pub finish_reason: Option<String>,

    pub latency_ms: u32,
    pub ttft_ms: Option<u32>,

    /// The `user` the caller sent on the request, echoed back.
    ///
    /// This is what makes the row re-billable one level down: a partner serving ten
    /// thousand people can now answer "which of my users spent this" without keeping a
    /// second ledger of their own and hoping it agrees with ours.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

impl From<UsageRecordRow> for UsageRecord {
    fn from(row: UsageRecordRow) -> Self {
        let billing = Billing::classify(&row);
        Self {
            id: row.id,
            request_id: row.request_id,
            created_at: row.created_at,
            endpoint: row.endpoint,
            model_requested: row.model_alias,
            model_served: row.served_by_alias,
            provider: row.provider,
            prompt_tokens: row.prompt_tokens.max(0) as u32,
            completion_tokens: row.completion_tokens.max(0) as u32,
            cached_tokens: row.cached_tokens.max(0) as u32,
            cost_nano: row.cost_nano,
            billing,
            status: row.status,
            error_kind: row.error_kind,
            finish_reason: row.finish_reason,
            latency_ms: row.latency_ms.max(0) as u32,
            ttft_ms: row.ttft_ms.map(|v| v.max(0) as u32),
            user: row.end_user_id,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageRecordsResponse {
    pub object: String,
    pub data: Vec<UsageRecord>,
    pub has_more: bool,
    /// Pass as `?before=` to fetch the next, older page.
    pub next_cursor: Option<Uuid>,
}

impl UsageRecordsResponse {
    pub fn new(rows: Vec<UsageRecordRow>, has_more: bool) -> Self {
        let data: Vec<UsageRecord> = rows.into_iter().map(UsageRecord::from).collect();
        let next_cursor = has_more.then(|| data.last().map(|r| r.id)).flatten();
        Self {
            object: "list".into(),
            data,
            has_more,
            next_cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(cost_nano: i64, status: &str, cache_hit: Option<&str>) -> UsageRecordRow {
        UsageRecordRow {
            id: Uuid::now_v7(),
            request_id: "req".into(),
            created_at: chrono::Utc::now(),
            endpoint: "chat".into(),
            model_alias: "gpt-4o-mini".into(),
            served_by_alias: "gpt-4o-mini".into(),
            provider: "openai".into(),
            prompt_tokens: 10,
            completion_tokens: 5,
            cached_tokens: 0,
            cost_nano,
            latency_ms: 100,
            ttft_ms: Some(40),
            status: status.into(),
            error_kind: None,
            cache_hit: cache_hit.map(str::to_string),
            finish_reason: Some("stop".into()),
            end_user_id: None,
        }
    }

    #[test]
    fn a_billed_request_is_charged() {
        assert_eq!(
            Billing::classify(&row(270_000, "completed", None)),
            Billing::Charged
        );
    }

    #[test]
    fn a_cache_hit_is_free_and_says_why() {
        assert_eq!(
            Billing::classify(&row(0, "completed", Some("exact"))),
            Billing::FreeCacheHit
        );
    }

    #[test]
    fn an_error_is_free_and_says_why() {
        assert_eq!(
            Billing::classify(&row(0, "error", None)),
            Billing::FreeError
        );
    }

    #[test]
    fn a_local_model_is_free_but_is_not_an_error() {
        // qwen3:4b is priced at zero. Reporting that as `free_error` would tell a client
        // its successful answers had failed.
        assert_eq!(
            Billing::classify(&row(0, "completed", None)),
            Billing::FreeZeroCost
        );
    }

    #[test]
    fn a_disconnect_that_generated_tokens_is_still_charged() {
        // The provider produced them. Somebody pays, and the client deserves to see it
        // rather than find it on the invoice.
        assert_eq!(
            Billing::classify(&row(120_000, "client_disconnect", None)),
            Billing::Charged
        );
    }

    #[test]
    fn remaining_is_none_when_the_budget_is_unlimited() {
        let r = UsageResponse::new(Uuid::now_v7(), 1, 2, 3, 4, None, 4);
        assert_eq!(r.remaining_nano, None);
    }

    #[test]
    fn remaining_may_be_negative_because_the_check_is_a_pre_check() {
        let r = UsageResponse::new(Uuid::now_v7(), 1, 2, 3, 500, Some(400), 500);
        assert_eq!(r.remaining_nano, Some(-100));
    }

    #[test]
    fn the_cursor_is_the_last_id_only_when_more_remain() {
        let rows = vec![row(1, "completed", None), row(2, "completed", None)];
        let last = rows[1].id;

        let page = UsageRecordsResponse::new(rows.clone(), true);
        assert_eq!(page.next_cursor, Some(last));

        let page = UsageRecordsResponse::new(rows, false);
        assert_eq!(
            page.next_cursor, None,
            "no cursor when the page is the last"
        );
    }

    #[test]
    fn an_empty_last_page_has_no_cursor() {
        let page = UsageRecordsResponse::new(vec![], false);
        assert!(page.data.is_empty());
        assert_eq!(page.next_cursor, None);
    }
}
