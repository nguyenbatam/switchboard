pub mod budget;
pub mod notify;
pub mod stream_guard;
pub mod tokenizer;

pub use stream_guard::StreamGuard;
pub use tokenizer::{TokenCounter, TokenEstimate};

use crate::config::models::ModelSpec;
use crate::domain::chat::{ChatResponse, FinishReason};
use crate::domain::usage::{RecordStatus, Usage};
use crate::http::extract::TenantCtx;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// Everything one billable call produced. Built by the caller that knows the
/// outcome — including the callers that only know the call *ended*.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub request_id: String,
    pub tenant_id: Uuid,
    pub api_key_id: Option<Uuid>,
    pub endpoint: &'static str,
    pub model_alias: String,
    pub provider: String,
    /// The alias that actually answered; differs from `model_alias` after a fallback.
    pub served_by_alias: String,
    pub usage: Usage,
    /// Revenue: what the tenant is billed.
    pub cost_nano: i64,
    /// Cost of goods: what we paid the provider. Never leaves this process except through
    /// `sbctl`. A tenant that can see the margin can negotiate against it.
    pub upstream_cost_nano: i64,
    pub latency_ms: i32,
    pub ttft_ms: Option<i32>,
    pub agent_iterations: i16,
    pub finish_reason: Option<String>,
    pub status: RecordStatus,
    pub error_kind: Option<String>,
    pub cache_hit: Option<String>,
    /// True when the prompt count came from `chars / 3.6` rather than an encoder.
    pub estimated_tokens: bool,
    /// The caller's own identifier for the end user, from the request's `user` field.
    ///
    /// Opaque to us, and validated for length at the edge — see
    /// [`crate::http::routes::chat::validate_end_user`]. It reaches exactly one place: the
    /// `usage_records.end_user_id` column. Never a metric label; never a cache key.
    pub end_user_id: Option<String>,
    /// The tenant's ceiling, carried on the event so the writer can spot a threshold
    /// crossing without a second lookup. `None` for an unlimited tenant.
    pub monthly_budget_nano: Option<i64>,
}

impl UsageEvent {
    /// Attaches the caller's own end-user identifier, if they sent one.
    ///
    /// A builder rather than a constructor parameter because every event has the same
    /// answer to this question and almost none of them care: threading `Option<String>`
    /// through five constructors would make the field look load-bearing when it is a leaf.
    pub fn with_end_user(mut self, end_user_id: Option<String>) -> Self {
        self.end_user_id = end_user_id;
        self
    }
}

/// How one chat call turned out. Groups the six facts a usage row needs beyond the
/// tenant and the model, so `chat_event` reads as a function and not a form.
pub struct ChatOutcome<'a> {
    pub resp: &'a ChatResponse,
    pub latency_ms: i32,
    /// Time to first token; `None` where the shape of the call has no such moment.
    pub ttft_ms: Option<i32>,
    pub agent_iterations: i16,
    /// `Some("exact")` when the answer came from cache and therefore cost nothing.
    pub cache_hit: Option<&'a str>,
    pub estimated_tokens: bool,
}

impl<'a> ChatOutcome<'a> {
    /// A plain, uncached, non-agent completion.
    pub fn completed(resp: &'a ChatResponse, latency_ms: i32, estimated_tokens: bool) -> Self {
        Self {
            resp,
            latency_ms,
            ttft_ms: None,
            agent_iterations: 0,
            cache_hit: None,
            estimated_tokens,
        }
    }

    pub fn cache_hit(resp: &'a ChatResponse, latency_ms: i32) -> Self {
        Self {
            resp,
            latency_ms,
            ttft_ms: Some(latency_ms),
            agent_iterations: 0,
            cache_hit: Some("exact"),
            estimated_tokens: false,
        }
    }

    pub fn with_ttft(mut self, ttft_ms: Option<i32>) -> Self {
        self.ttft_ms = ttft_ms;
        self
    }

    pub fn with_agent_iterations(mut self, iterations: i16) -> Self {
        self.agent_iterations = iterations;
        self
    }
}

#[derive(Debug, Default)]
pub struct UsageSummary {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_nano: i64,
}

/// How many rows the writer will bundle into one transaction.
const BATCH_ROWS: usize = 500;

/// …and how long it will wait to fill that bundle. Whichever comes first.
const BATCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(200);

/// Depth of the queue between the request path and the writer.
///
/// Bounded on purpose. An unbounded channel does not remove backpressure, it relocates
/// it into the allocator and calls the resulting OOM a surprise. When this fills, rows are
/// dropped and `sb_usage_records_dropped_total` counts them — which is the metric the
/// README already names as the one to page on, and which previously could only be
/// incremented by a failing `INSERT`, because there was no queue to overflow.
const QUEUE_DEPTH: usize = 10_000;

pub struct UsageRecorder {
    /// Only for the read paths: `/v1/usage` and `/v1/usage/records`. Writes go through
    /// `tx` to the background writer, which owns its own handle.
    pool: Option<PgPool>,
    /// `None` when there is no Postgres to write to, or no runtime to spawn on.
    tx: Option<tokio::sync::mpsc::Sender<UsageEvent>>,
}

/// What the background writer needs beyond the queue.
pub struct WriterDeps {
    pub pool: PgPool,
    pub redis: Option<deadpool_redis::Pool>,
    /// `None` when no `billing.budget_webhook_url` is configured.
    pub notifier: Option<Arc<notify::BudgetNotifier>>,
}

impl UsageRecorder {
    /// Starts the background writer, if there is anything for it to write to.
    ///
    /// It used to be that `record()` spawned one task per request to issue one `INSERT`,
    /// plus a second task for `budget::add_spend`. Two tasks and a database round trip on
    /// every call, all contending for the same ten-connection pool as the api-key lookup
    /// and RAG search. Under load the request path fought itself for connections.
    ///
    /// docs/01 §5 specified a bounded `mpsc` feeding a batching writer. This is that.
    pub fn new(
        pool: Option<PgPool>,
        redis: Option<deadpool_redis::Pool>,
        notifier: Option<Arc<notify::BudgetNotifier>>,
    ) -> Self {
        let tx = match (&pool, tokio::runtime::Handle::try_current()) {
            (Some(pool), Ok(handle)) => {
                let (tx, rx) = tokio::sync::mpsc::channel(QUEUE_DEPTH);
                let deps = WriterDeps {
                    pool: pool.clone(),
                    redis,
                    notifier,
                };
                handle.spawn(async move { writer_loop(rx, deps).await });
                Some(tx)
            }
            _ => None,
        };
        Self { pool, tx }
    }

    /// Builds an event for a completed (or cached) chat response.
    ///
    /// Two things decide the price, and neither is the model the client asked for:
    ///
    /// A cache hit costs nothing. It consumed no upstream tokens, so it bills zero
    /// rather than re-billing the tokens of the response that populated it.
    ///
    /// A fallback bills the model that *answered*. When `gpt-4o-mini` is down and a
    /// cheaper local model serves the request, charging the client gpt-4o-mini's
    /// rate would invoice them for tokens nobody generated. `served_by_alias` on the
    /// row, and `x-switchboard-fallback` on the response, say which model that was.
    pub fn chat_event(
        tenant: &TenantCtx,
        model_alias: &str,
        registry: &crate::config::models::ModelRegistry,
        requested_spec: &ModelSpec,
        outcome: ChatOutcome<'_>,
    ) -> UsageEvent {
        let ChatOutcome {
            resp,
            latency_ms,
            ttft_ms,
            agent_iterations,
            cache_hit,
            estimated_tokens,
        } = outcome;

        let served_by_alias = resp
            .served_by
            .clone()
            .unwrap_or_else(|| model_alias.to_string());

        let billing_spec = registry.get(&served_by_alias).unwrap_or(requested_spec);

        let cached = cache_hit.is_some();
        let usage = if cached {
            Usage {
                cached_tokens: resp.usage.total_tokens.max(resp.usage.total()),
                ..Default::default()
            }
        } else {
            resp.usage.clone()
        };

        // A cache hit reached no provider: it earns nothing and costs nothing. Both sides
        // of the ledger are zero, and the margin on it is neither infinite nor undefined.
        let (cost_nano, upstream_cost_nano) = if cached {
            (0, 0)
        } else {
            (
                crate::config::models::ModelRegistry::cost_nano_for_spec(
                    billing_spec,
                    usage.prompt_tokens,
                    usage.completion_tokens,
                ),
                crate::config::models::ModelRegistry::upstream_cost_nano_for_spec(
                    billing_spec,
                    usage.prompt_tokens,
                    usage.completion_tokens,
                ),
            )
        };

        UsageEvent {
            request_id: tenant.request_id.clone(),
            tenant_id: tenant.key.tenant.id,
            api_key_id: tenant.key.api_key_id,
            endpoint: "chat",
            model_alias: model_alias.into(),
            provider: billing_spec.provider.clone(),
            served_by_alias,
            usage,
            cost_nano,
            upstream_cost_nano,
            latency_ms,
            ttft_ms,
            agent_iterations,
            finish_reason: resp
                .choices
                .first()
                .and_then(|c| c.finish_reason)
                .map(|r: FinishReason| r.as_openai_str().to_string()),
            status: RecordStatus::Completed,
            error_kind: None,
            cache_hit: cache_hit.map(str::to_string),
            estimated_tokens,
            end_user_id: None,
            monthly_budget_nano: tenant.key.tenant.monthly_budget_nano,
        }
    }

    /// A request the upstream refused. No tokens were generated, so it costs
    /// nothing — but a ledger that only records successes cannot answer "what
    /// fraction of this tenant's traffic is failing", which is the first question
    /// asked when a customer complains.
    pub fn error_event(
        tenant: &TenantCtx,
        model_alias: &str,
        spec: &ModelSpec,
        latency_ms: i32,
        error_kind: &str,
    ) -> UsageEvent {
        UsageEvent {
            request_id: tenant.request_id.clone(),
            tenant_id: tenant.key.tenant.id,
            api_key_id: tenant.key.api_key_id,
            endpoint: "chat",
            model_alias: model_alias.into(),
            provider: spec.provider.clone(),
            served_by_alias: model_alias.into(),
            usage: Usage::default(),
            // An error earns nothing. It may still have cost us upstream tokens, but the
            // provider does not bill for a request it failed, and neither do we.
            cost_nano: 0,
            upstream_cost_nano: 0,
            latency_ms,
            ttft_ms: None,
            agent_iterations: 0,
            finish_reason: None,
            status: RecordStatus::Error,
            error_kind: Some(error_kind.to_string()),
            cache_hit: None,
            estimated_tokens: false,
            end_user_id: None,
            monthly_budget_nano: tenant.key.tenant.monthly_budget_nano,
        }
    }

    /// Fire-and-forget: the response is already on its way to the client, and a
    /// usage write must never be able to fail it. Drops are counted, not swallowed.
    ///
    /// Never blocks, never awaits, never allocates a task. It hands the event to the
    /// writer and returns.
    pub fn record(&self, ev: UsageEvent) {
        crate::telemetry::metrics::record_cost(&ev.model_alias, ev.cost_nano);
        if ev.estimated_tokens {
            crate::telemetry::metrics::record_token_drift(&ev.model_alias);
        }
        if ev.agent_iterations > 0 {
            crate::telemetry::metrics::record_agent_iterations(
                &ev.model_alias,
                ev.agent_iterations as f64,
            );
        }
        if let Some(ttft) = ev.ttft_ms {
            crate::telemetry::metrics::record_ttft(&ev.model_alias, ttft as f64 / 1000.0);
        }

        let Some(tx) = &self.tx else {
            return;
        };

        if let Err(e) = tx.try_send(ev) {
            // The queue is full or the writer is gone. The money was spent and we could
            // not write it down; that is exactly what this counter is for.
            crate::telemetry::metrics::record_usage_dropped();
            let request_id = match &e {
                tokio::sync::mpsc::error::TrySendError::Full(ev)
                | tokio::sync::mpsc::error::TrySendError::Closed(ev) => ev.request_id.clone(),
            };
            tracing::warn!(request_id = %request_id, "usage queue full, dropping a billing row");
        }
    }

    /// Reads the rollup, never the fact table.
    pub async fn tenant_summary(&self, tenant_id: Uuid) -> Option<UsageSummary> {
        let pool = self.pool.as_ref()?;
        let row = sqlx::query_as::<_, SummaryRow>(
            r#"
            SELECT
                COALESCE(SUM(requests), 0)::bigint          AS requests,
                COALESCE(SUM(prompt_tokens), 0)::bigint     AS prompt_tokens,
                COALESCE(SUM(completion_tokens), 0)::bigint AS completion_tokens,
                COALESCE(SUM(cost_nano), 0)::bigint         AS cost_nano
            FROM usage_hourly
            WHERE tenant_id = $1
            "#,
        )
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .ok()?;

        Some(UsageSummary {
            requests: row.requests as u64,
            prompt_tokens: row.prompt_tokens as u64,
            completion_tokens: row.completion_tokens as u64,
            cost_nano: row.cost_nano,
        })
    }
}

#[derive(sqlx::FromRow)]
struct SummaryRow {
    requests: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    cost_nano: i64,
}

/// One row of the tenant's own call history, as the tenant is allowed to see it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct UsageRecordRow {
    pub id: Uuid,
    pub request_id: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub endpoint: String,
    pub model_alias: String,
    pub served_by_alias: String,
    pub provider: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub cached_tokens: i32,
    pub cost_nano: i64,
    pub latency_ms: i32,
    pub ttft_ms: Option<i32>,
    pub status: String,
    pub error_kind: Option<String>,
    pub cache_hit: Option<String>,
    pub finish_reason: Option<String>,
    /// The caller's own end-user identifier, echoed back to the caller who sent it.
    pub end_user_id: Option<String>,
}

impl UsageRecorder {
    /// Month-to-date spend, from the rollup. The same number `budget::check` enforces
    /// against, so a client that reads it sees the balance the gateway will act on.
    pub async fn month_spend(&self, tenant_id: Uuid) -> Option<i64> {
        let pool = self.pool.as_ref()?;
        sqlx::query_scalar::<_, i64>(
            r#"
            SELECT COALESCE(SUM(cost_nano), 0)::bigint
            FROM usage_hourly
            WHERE tenant_id = $1 AND hour >= date_trunc('month', now())
            "#,
        )
        .bind(tenant_id)
        .fetch_one(pool)
        .await
        .ok()
    }

    /// The fact table, newest first, keyset-paginated.
    ///
    /// The cursor is an `id`, not a timestamp, because ids are UUIDv7 — *"time-ordered, so
    /// they index like a bigint and sort like a timestamp"* ([docs/00]). Two rows written
    /// in the same millisecond still have a total order, which an `OFFSET` or a
    /// `created_at` cursor would lose.
    ///
    /// Fetches `limit + 1` so `has_more` is a fact rather than a guess.
    pub async fn recent_records(
        &self,
        tenant_id: Uuid,
        limit: i64,
        before: Option<Uuid>,
        end_user_id: Option<&str>,
    ) -> Option<(Vec<UsageRecordRow>, bool)> {
        let pool = self.pool.as_ref()?;
        // `tenant_id = $1` is not a filter, it is the isolation boundary. There is no
        // parameter that widens it, and there must not be. `$4` narrows *within* it.
        let mut rows = sqlx::query_as::<_, UsageRecordRow>(
            r#"
            SELECT id, request_id, created_at, endpoint, model_alias, served_by_alias,
                   provider, prompt_tokens, completion_tokens, cached_tokens, cost_nano,
                   latency_ms, ttft_ms, status, error_kind, cache_hit, finish_reason,
                   end_user_id
            FROM usage_records
            WHERE tenant_id = $1
              AND ($2::uuid IS NULL OR id < $2)
              AND ($4::text IS NULL OR end_user_id = $4)
            ORDER BY id DESC
            LIMIT $3
            "#,
        )
        .bind(tenant_id)
        .bind(before)
        .bind(limit + 1)
        .bind(end_user_id)
        .fetch_all(pool)
        .await
        .ok()?;

        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        Some((rows, has_more))
    }
}

/// Drains the queue into Postgres in batches of [`BATCH_ROWS`], or every
/// [`BATCH_INTERVAL`], whichever comes first.
///
/// One transaction per batch instead of one per request. At 200 rps that is five
/// commits a second rather than two hundred, and the ten-connection pool goes back to
/// serving the request path.
async fn writer_loop(mut rx: tokio::sync::mpsc::Receiver<UsageEvent>, deps: WriterDeps) {
    let mut batch: Vec<UsageEvent> = Vec::with_capacity(BATCH_ROWS);

    loop {
        // Wait for the first event with no deadline: an idle gateway must not spin.
        let Some(first) = rx.recv().await else {
            break; // Sender dropped: the process is going away.
        };
        batch.push(first);

        // Then fill the batch, but never wait longer than the interval for it. A single
        // request at 3am is written within 200 ms, not when the 500th arrives.
        let deadline = tokio::time::Instant::now() + BATCH_INTERVAL;
        while batch.len() < BATCH_ROWS {
            match tokio::time::timeout_at(deadline, rx.recv()).await {
                Ok(Some(ev)) => batch.push(ev),
                Ok(None) => break,
                Err(_) => break, // interval elapsed
            }
        }

        flush(&deps, &mut batch).await;
    }

    // Drain whatever arrived while we were shutting down.
    while let Ok(ev) = rx.try_recv() {
        batch.push(ev);
    }
    if !batch.is_empty() {
        flush(&deps, &mut batch).await;
    }
}

/// Writes one batch, then clears it. Never returns an error: a batch we could not persist
/// is money spent and not written down, which is a metric and a log line, not a panic.
async fn flush(deps: &WriterDeps, batch: &mut Vec<UsageEvent>) {
    if batch.is_empty() {
        return;
    }

    if let Err(e) = write_batch(&deps.pool, batch).await {
        crate::telemetry::metrics::record_usage_dropped_n(batch.len() as u64);
        tracing::error!(
            error = %e,
            rows = batch.len(),
            "failed to persist a usage batch; money was spent and not written down"
        );
    } else if let Some(redis) = &deps.redis {
        // Only after Postgres has the rows. The Redis counter is a cache over the ledger,
        // and incrementing it for rows that were never committed would overcharge a tenant
        // until the next counter rebuild disagreed with it.
        let mut per_tenant: std::collections::HashMap<Uuid, (i64, Option<i64>)> =
            std::collections::HashMap::new();
        for ev in batch.iter().filter(|e| e.cost_nano > 0) {
            let entry = per_tenant
                .entry(ev.tenant_id)
                .or_insert((0, ev.monthly_budget_nano));
            entry.0 += ev.cost_nano;
        }

        for (tenant_id, (cost, limit)) in per_tenant {
            let Some(total) = budget::add_spend(redis, tenant_id, cost).await else {
                continue;
            };
            // The one place that knows both the new total and the tenant's ceiling. A
            // tenant with no ceiling has nothing to cross.
            if let (Some(notifier), Some(limit)) = (&deps.notifier, limit) {
                notifier.maybe_notify(redis, tenant_id, total, limit).await;
            }
        }
    }

    batch.clear();
}

/// The fact rows and the rollup move together, or neither moves.
async fn write_batch(pool: &PgPool, batch: &[UsageEvent]) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;

    let mut rows = sqlx::QueryBuilder::new(
        "INSERT INTO usage_records (
            id, request_id, tenant_id, api_key_id, endpoint, model_alias, provider,
            served_by_alias, prompt_tokens, completion_tokens, cached_tokens,
            reasoning_tokens, cost_nano, upstream_cost_nano, ttft_ms, latency_ms,
            agent_iterations, finish_reason, status, error_kind, cache_hit, end_user_id
        ) ",
    );
    rows.push_values(batch, |mut b, r| {
        b.push_bind(Uuid::now_v7())
            .push_bind(&r.request_id)
            .push_bind(r.tenant_id)
            .push_bind(r.api_key_id)
            .push_bind(r.endpoint)
            .push_bind(&r.model_alias)
            .push_bind(&r.provider)
            .push_bind(&r.served_by_alias)
            .push_bind(r.usage.prompt_tokens as i32)
            .push_bind(r.usage.completion_tokens as i32)
            .push_bind(r.usage.cached_tokens as i32)
            .push_bind(r.usage.reasoning_tokens as i32)
            .push_bind(r.cost_nano)
            .push_bind(r.upstream_cost_nano)
            .push_bind(r.ttft_ms)
            .push_bind(r.latency_ms)
            .push_bind(r.agent_iterations)
            .push_bind(r.finish_reason.as_deref())
            .push_bind(r.status.as_str())
            .push_bind(r.error_kind.as_deref())
            .push_bind(r.cache_hit.as_deref())
            .push_bind(r.end_user_id.as_deref());
    });
    rows.build().execute(&mut *tx).await?;

    // Collapsed before it reaches the database. Fifty rows from one tenant on one model
    // are one upsert, not fifty contending for the same rollup row's lock.
    let mut rollup: std::collections::HashMap<(Uuid, &str), RollupDelta> =
        std::collections::HashMap::new();
    for r in batch {
        let entry = rollup
            .entry((r.tenant_id, r.model_alias.as_str()))
            .or_default();
        entry.requests += 1;
        entry.prompt_tokens += r.usage.prompt_tokens as i64;
        entry.completion_tokens += r.usage.completion_tokens as i64;
        entry.cost_nano += r.cost_nano;
        entry.upstream_cost_nano += r.upstream_cost_nano;
    }

    for ((tenant_id, model_alias), d) in rollup {
        sqlx::query(
            r#"
            INSERT INTO usage_hourly (
                tenant_id, hour, model_alias, requests, prompt_tokens, completion_tokens,
                cost_nano, upstream_cost_nano
            ) VALUES (
                $1, date_trunc('hour', now()), $2, $3, $4, $5, $6, $7
            )
            ON CONFLICT (tenant_id, hour, model_alias) DO UPDATE SET
                requests           = usage_hourly.requests + EXCLUDED.requests,
                prompt_tokens      = usage_hourly.prompt_tokens + EXCLUDED.prompt_tokens,
                completion_tokens  = usage_hourly.completion_tokens + EXCLUDED.completion_tokens,
                cost_nano          = usage_hourly.cost_nano + EXCLUDED.cost_nano,
                upstream_cost_nano = usage_hourly.upstream_cost_nano + EXCLUDED.upstream_cost_nano
            "#,
        )
        .bind(tenant_id)
        .bind(model_alias)
        .bind(d.requests)
        .bind(d.prompt_tokens)
        .bind(d.completion_tokens)
        .bind(d.cost_nano)
        .bind(d.upstream_cost_nano)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await
}

#[derive(Default)]
struct RollupDelta {
    requests: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    cost_nano: i64,
    upstream_cost_nano: i64,
}

pub type SharedUsage = Arc<UsageRecorder>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::models::{ModelRegistry, TokenizerSpec};
    use crate::domain::chat::{Choice, Message, MessageContent, Role};
    use crate::domain::tenant::{AuthenticatedKey, Tenant};

    fn registry() -> ModelRegistry {
        let mut r = ModelRegistry::for_test("expensive", TokenizerSpec::Estimate);
        r.insert_for_test("expensive", "openai", 1.0, 2.0);
        r.insert_for_test("cheap", "ollama", 0.5, 1.0);
        r
    }

    fn tenant() -> TenantCtx {
        TenantCtx {
            request_id: "req-1".into(),
            deadline: None,
            key: AuthenticatedKey {
                api_key_id: None,
                scopes: vec![],
                tenant: Tenant {
                    id: Uuid::nil(),
                    name: "t".into(),
                    rpm_limit: 60,
                    tpm_limit: 60_000,
                    monthly_budget_nano: None,
                },
            },
        }
    }

    fn response(served_by: Option<&str>) -> ChatResponse {
        ChatResponse {
            id: "c1".into(),
            model: "expensive".into(),
            served_by: served_by.map(str::to_string),
            created: 0,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Some(MessageContent::Text("hi".into())),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: Some(FinishReason::Stop),
            }],
            usage: Usage {
                prompt_tokens: 1_000,
                completion_tokens: 1_000,
                ..Default::default()
            }
            .with_total(),
        }
    }

    fn event(served_by: Option<&str>, cache_hit: Option<&str>) -> UsageEvent {
        let reg = registry();
        let spec = reg.get("expensive").unwrap();
        let resp = response(served_by);
        let outcome = match cache_hit {
            Some(_) => ChatOutcome::cache_hit(&resp, 10),
            None => ChatOutcome::completed(&resp, 10, false),
        };
        UsageRecorder::chat_event(&tenant(), "expensive", &reg, spec, outcome)
    }

    #[test]
    fn a_normal_request_bills_the_model_that_was_asked_for() {
        let ev = event(None, None);
        assert_eq!(ev.served_by_alias, "expensive");
        // 1000 * $1.00/M + 1000 * $2.00/M = $0.003 = 3_000_000 nanodollars.
        assert_eq!(ev.cost_nano, 3_000_000);
        assert_eq!(ev.provider, "openai");
    }

    #[test]
    fn a_fallback_bills_the_model_that_actually_answered() {
        let ev = event(Some("cheap"), None);

        assert_eq!(ev.model_alias, "expensive", "what the client asked for");
        assert_eq!(ev.served_by_alias, "cheap", "what actually ran");
        assert_eq!(
            ev.cost_nano, 1_500_000,
            "charging the requested model's rate would invoice for tokens \
             the fallback never generated"
        );
        assert_eq!(ev.provider, "ollama", "the provider that did the work");
    }

    #[test]
    fn an_unknown_fallback_alias_falls_back_to_the_requested_price() {
        let ev = event(Some("deleted-from-config"), None);
        assert_eq!(
            ev.cost_nano, 3_000_000,
            "a missing spec must not silently price the request at zero"
        );
    }

    #[test]
    fn a_cache_hit_is_free_and_reports_the_tokens_it_saved() {
        let ev = event(None, Some("exact"));

        assert_eq!(ev.cost_nano, 0, "no upstream tokens were consumed");
        assert_eq!(ev.usage.prompt_tokens, 0);
        assert_eq!(ev.usage.completion_tokens, 0);
        assert_eq!(ev.usage.cached_tokens, 2_000, "what the hit saved");
        assert_eq!(ev.cache_hit.as_deref(), Some("exact"));
    }
}
