//! Usage accounting for streamed completions.
//!
//! A streamed response has no single point where you know it finished — the client
//! can vanish between any two chunks, and when it does, the upstream `usage` chunk
//! never arrives. So the guard counts deltas locally as they cross the wire and
//! writes the row from `Drop`, which runs on every exit: clean finish, client
//! disconnect, provider error, task cancellation.
//!
//! The tokens were generated either way. Somebody paid for them.

use crate::cache::ExactCache;
use crate::config::models::ModelSpec;
use crate::domain::chat::{
    ChatRequest, ChatResponse, Choice, FinishReason, Message, MessageContent, Role,
};
use crate::domain::usage::{RecordStatus, Usage};
use crate::http::extract::TenantCtx;
use crate::http::idempotency::{Guard as IdemGuard, StoredResponse};
use crate::ratelimit::{RateLimiter, TokenReservation};
use crate::usage::{TokenCounter, UsageEvent, UsageRecorder};
use std::sync::Arc;
use std::time::Instant;

/// Relative disagreement between our local count and the upstream count above
/// which we assume our tokenizer is wrong about this model, and say so.
const DRIFT_TOLERANCE: f64 = 0.02;

/// What a completed stream needs in order to seed the shared cache.
///
/// A gateway cache that is only *populated* by non-streaming requests never fills,
/// because production streams. The response has to be reassembled from the deltas
/// as they go past.
pub struct CacheSeed {
    pub cache: Arc<ExactCache>,
    pub tenant_id: uuid::Uuid,
    pub request: ChatRequest,
}

pub struct StreamGuard {
    end_user_id: Option<String>,
    /// Dropped with the guard, releasing the upstream slot. Never read.
    _permit: Option<crate::providers::registry::ProviderPermit>,
    /// Cancelled in `drop`, so a dead socket stops the work it was paying for.
    cancel: tokio_util::sync::CancellationToken,

    recorder: Arc<UsageRecorder>,
    counter: Arc<TokenCounter>,
    rate_limiter: Option<Arc<RateLimiter>>,
    reservation: Option<TokenReservation>,

    tenant: TenantCtx,
    model_alias: String,
    provider: String,
    spec: ModelSpec,

    started: Instant,
    first_token_at: Option<Instant>,

    prompt_tokens: u32,
    prompt_estimated: bool,
    /// Everything the client actually received, for the local completion count.
    completion_buf: String,
    upstream_usage: Option<Usage>,
    finish_reason: Option<FinishReason>,
    /// Set once the terminal event has been emitted to the client.
    completed: bool,
    error_kind: Option<String>,
    /// True if any tool-call delta crossed the wire; such a response is not
    /// reusable and `completion_buf` holds arguments rather than prose.
    saw_tool_calls: bool,
    cache_seed: Option<CacheSeed>,
    /// Held open until the stream completes, so a retried request replays the same
    /// answer instead of paying to generate a second one.
    idem: Option<IdemGuard>,
}

/// Everything the guard needs to bill, cache, and record a stream it has not seen yet.
pub struct StreamGuardInit {
    pub recorder: Arc<UsageRecorder>,
    pub counter: Arc<TokenCounter>,
    pub rate_limiter: Option<Arc<RateLimiter>>,
    pub reservation: Option<TokenReservation>,
    pub tenant: TenantCtx,
    pub model_alias: String,
    pub spec: ModelSpec,
    pub prompt_tokens: u32,
    pub prompt_estimated: bool,
    pub cache_seed: Option<CacheSeed>,
    pub idem: Option<IdemGuard>,
    /// The caller's own end-user identifier, for the usage row this guard will write.
    pub end_user_id: Option<String>,
    /// The upstream slot this stream occupies.
    ///
    /// It rides the guard, which rides the response body, so the slot is released when the
    /// last token is sent, the socket dies, or the task is cancelled — whichever comes
    /// first. Released any earlier and the per-provider cap would bound nothing: a
    /// streaming request holds its upstream for its whole answer, not for its headers.
    pub permit: Option<crate::providers::registry::ProviderPermit>,
    /// Cancels the reader and every spawned tool task when the guard drops.
    ///
    /// `docs/01 §5` prints this `drop` body and its first line is `self.cancel.cancel()`.
    /// It was never called anywhere in `src/`: cancellation worked only as a side effect of
    /// axum dropping the handler future, which propagates into the response body but **not**
    /// into `set.spawn`ed tool tasks. A client that disconnected while three tools were
    /// running left three tools running, and kept paying for them.
    pub cancel: tokio_util::sync::CancellationToken,
}

impl StreamGuard {
    pub fn new(init: StreamGuardInit) -> Self {
        Self {
            _permit: init.permit,
            cancel: init.cancel,
            end_user_id: init.end_user_id,
            recorder: init.recorder,
            counter: init.counter,
            rate_limiter: init.rate_limiter,
            reservation: init.reservation,
            provider: init.spec.provider.clone(),
            tenant: init.tenant,
            model_alias: init.model_alias,
            spec: init.spec,
            started: Instant::now(),
            first_token_at: None,
            prompt_tokens: init.prompt_tokens,
            prompt_estimated: init.prompt_estimated,
            completion_buf: String::new(),
            upstream_usage: None,
            finish_reason: None,
            completed: false,
            error_kind: None,
            saw_tool_calls: false,
            cache_seed: init.cache_seed,
            idem: init.idem,
        }
    }

    pub fn on_delta(&mut self, content: &str) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
        self.completion_buf.push_str(content);
    }

    /// Tool-call arguments are generated tokens too.
    pub fn on_tool_call_delta(&mut self, arguments: &str) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
        self.saw_tool_calls = true;
        self.completion_buf.push_str(arguments);
    }

    pub fn on_upstream_usage(&mut self, usage: Usage) {
        self.upstream_usage = Some(usage);
    }

    pub fn on_finish(&mut self, reason: FinishReason) {
        self.finish_reason = Some(reason);
    }

    pub fn on_error(&mut self, kind: impl Into<String>) {
        self.error_kind = Some(kind.into());
    }

    /// Called after the terminal `[DONE]` has been yielded. Anything that drops
    /// without this is, by definition, a client that stopped listening.
    pub fn mark_completed(&mut self) {
        self.completed = true;
    }

    /// Prefers the upstream count when we have it, and reports disagreement.
    fn resolve_usage(&self) -> (Usage, bool) {
        let (local_completion, exact) = self
            .counter
            .count_text(&self.model_alias, &self.completion_buf);

        let Some(upstream) = &self.upstream_usage else {
            return (
                Usage {
                    prompt_tokens: self.prompt_tokens,
                    completion_tokens: local_completion,
                    ..Default::default()
                }
                .with_total(),
                self.prompt_estimated || !exact,
            );
        };

        let drifted = drift_exceeds(local_completion, upstream.completion_tokens);
        if drifted {
            tracing::debug!(
                model = %self.model_alias,
                local = local_completion,
                upstream = upstream.completion_tokens,
                "local completion token count disagrees with upstream"
            );
        }

        (upstream.clone().with_total(), drifted)
    }
}

fn drift_exceeds(local: u32, upstream: u32) -> bool {
    if upstream == 0 {
        return local > 0;
    }
    let diff = (local as f64 - upstream as f64).abs();
    diff / upstream as f64 > DRIFT_TOLERANCE
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        // First, before anything that might take time. Fans out to the reader task and to
        // every `set.spawn`ed tool task, none of which axum's own cancellation reaches.
        self.cancel.cancel();

        let (usage, estimated) = self.resolve_usage();

        let status = if self.error_kind.is_some() {
            RecordStatus::Error
        } else if self.completed {
            RecordStatus::Completed
        } else {
            RecordStatus::ClientDisconnect
        };

        let latency_ms = self.started.elapsed().as_millis() as i32;
        let ttft_ms = self
            .first_token_at
            .map(|t| t.duration_since(self.started).as_millis() as i32);

        if let (Some(first), true) = (self.first_token_at, usage.completion_tokens > 1) {
            let gen_secs = first.elapsed().as_secs_f64();
            if gen_secs > 0.0 {
                crate::telemetry::metrics::record_output_tps(
                    &self.model_alias,
                    (usage.completion_tokens - 1) as f64 / gen_secs,
                );
            }
        }
        crate::telemetry::metrics::record_tokens(
            &self.model_alias,
            "prompt",
            usage.prompt_tokens as u64,
        );
        crate::telemetry::metrics::record_tokens(
            &self.model_alias,
            "completion",
            usage.completion_tokens as u64,
        );

        // A stream that died mid-flight is still billed for the tokens the provider
        // generated. It charged us for them; the margin on a disconnect is real.
        let upstream_cost_nano = crate::config::models::ModelRegistry::upstream_cost_nano_for_spec(
            &self.spec,
            usage.prompt_tokens,
            usage.completion_tokens,
        );
        let cost_nano = crate::config::models::ModelRegistry::cost_nano_for_spec(
            &self.spec,
            usage.prompt_tokens,
            usage.completion_tokens,
        );

        let event = UsageEvent {
            request_id: self.tenant.request_id.clone(),
            tenant_id: self.tenant.key.tenant.id,
            api_key_id: self.tenant.key.api_key_id,
            endpoint: "chat",
            model_alias: self.model_alias.clone(),
            provider: self.provider.clone(),
            served_by_alias: self.model_alias.clone(),
            cost_nano,
            upstream_cost_nano,
            latency_ms,
            ttft_ms,
            agent_iterations: 0,
            finish_reason: self
                .finish_reason
                .map(|r| r.as_openai_str().to_string())
                .or_else(|| (!self.completed).then(|| "client_disconnect".to_string())),
            status,
            error_kind: self.error_kind.clone(),
            cache_hit: None,
            estimated_tokens: estimated,
            end_user_id: self.end_user_id.clone(),
            monthly_budget_nano: self.tenant.key.tenant.monthly_budget_nano,
            usage: usage.clone(),
        };

        self.recorder.record(event);

        // Settle the admission reservation against what was actually generated.
        if let (Some(rl), Some(reservation)) = (self.rate_limiter.clone(), self.reservation) {
            let actual = usage.total_tokens.max(usage.total());
            // `Drop` is sync; the refund is fire-and-forget. Outside a runtime
            // (unit tests, shutdown) there is nothing to spawn onto, and skipping
            // it is safe: the reservation ages out of the GCRA window anyway.
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    rl.settle_tokens(reservation, actual).await;
                });
            }
        }

        // A partial stream is not an answer, so neither the cache nor the
        // idempotency record may keep it. `Guard::drop` then releases the key and
        // the client's retry runs for real.
        if let Some(response) = self.assemble_response(&usage) {
            if let Some(guard) = self.idem.take()
                && let Ok(json) = serde_json::to_string(&StoredResponse {
                    response: response.clone(),
                    served_by: None,
                })
            {
                guard.commit_detached(json);
            }

            if let Some(seed) = self.cache_seed.take()
                && let Ok(handle) = tokio::runtime::Handle::try_current()
            {
                handle.spawn(async move {
                    seed.cache
                        .put(seed.tenant_id, &seed.request, response)
                        .await;
                });
            }
        }
    }
}

impl StreamGuard {
    /// The answer this stream produced, if it produced a whole one.
    ///
    /// A partial stream is not an answer: replaying or caching what a disconnected
    /// client saw would hand that truncated text to the next caller as if it were
    /// complete. Tool calls are excluded too — the reassembled `content` would be
    /// JSON arguments rather than prose.
    fn assemble_response(&mut self, usage: &Usage) -> Option<ChatResponse> {
        if !self.completed || self.error_kind.is_some() || self.saw_tool_calls {
            return None;
        }
        if self.completion_buf.is_empty() {
            return None;
        }

        let response = ChatResponse {
            id: format!("chatcmpl-{}", uuid::Uuid::now_v7()),
            model: self.model_alias.clone(),
            served_by: None,
            created: chrono::Utc::now().timestamp(),
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: Some(MessageContent::Text(std::mem::take(
                        &mut self.completion_buf,
                    ))),
                    name: None,
                    tool_calls: None,
                    tool_call_id: None,
                },
                finish_reason: self.finish_reason,
            }],
            usage: usage.clone(),
        };

        Some(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_is_relative_and_tolerant_of_rounding() {
        assert!(!drift_exceeds(100, 100));
        assert!(!drift_exceeds(101, 100), "1% is within tolerance");
        assert!(drift_exceeds(105, 100), "5% is not");
        assert!(drift_exceeds(0, 100));
    }

    fn guard(seeded: bool) -> StreamGuard {
        use crate::domain::tenant::{AuthenticatedKey, Tenant};
        let cache = Arc::new(ExactCache::new(60, None));
        let registry = crate::config::models::ModelRegistry::for_test(
            "m",
            crate::config::models::TokenizerSpec::Estimate,
        );

        let mut req = crate::domain::chat::ChatRequest {
            model: "m".into(),
            messages: vec![],
            tools: None,
            tool_choice: None,
            temperature: Some(0.0),
            top_p: None,
            max_tokens: None,
            seed: None,
            stop: None,
            top_k: None,
            stream: true,
            stream_options: None,
            response_format: None,
            parallel_tool_calls: None,
            rag: None,
            user: None,
            legacy_completion: false,
            untrusted_context: false,
        };
        req.messages.clear();

        StreamGuard::new(StreamGuardInit {
            recorder: Arc::new(UsageRecorder::new(None, None, None)),
            counter: Arc::new(TokenCounter::new(&registry)),
            rate_limiter: None,
            reservation: None,
            tenant: TenantCtx {
                request_id: "r".into(),
                deadline: None,
                key: AuthenticatedKey {
                    api_key_id: None,
                    scopes: vec![],
                    tenant: Tenant {
                        id: uuid::Uuid::nil(),
                        name: "t".into(),
                        rpm_limit: 60,
                        tpm_limit: 60_000,
                        monthly_budget_nano: None,
                    },
                },
            },
            model_alias: "m".into(),
            spec: registry.get("m").unwrap().clone(),
            prompt_tokens: 10,
            prompt_estimated: true,
            cache_seed: seeded.then(|| CacheSeed {
                cache,
                tenant_id: uuid::Uuid::nil(),
                request: req,
            }),
            idem: None,
            end_user_id: None,
            permit: None,
            cancel: tokio_util::sync::CancellationToken::new(),
        })
    }

    #[test]
    fn a_completed_stream_yields_a_reusable_response() {
        let mut g = guard(true);
        g.on_delta("the answer");
        g.on_finish(FinishReason::Stop);
        g.mark_completed();

        let usage = Usage::default();
        assert!(
            g.assemble_response(&usage).is_some(),
            "a completed stream must be reusable: it seeds the cache and the idempotency record"
        );
    }

    #[test]
    fn a_disconnected_stream_yields_nothing_reusable() {
        let mut g = guard(true);
        g.on_delta("half an ans");
        // No mark_completed: the client hung up.

        assert!(
            g.assemble_response(&Usage::default()).is_none(),
            "replaying or caching a truncated answer would hand it to the next caller as complete"
        );
    }

    #[test]
    fn a_stream_that_errored_yields_nothing_reusable() {
        let mut g = guard(true);
        g.on_delta("partial");
        g.on_error("upstream_5xx");
        g.mark_completed();

        assert!(g.assemble_response(&Usage::default()).is_none());
    }

    #[test]
    fn a_stream_carrying_tool_calls_yields_nothing_reusable() {
        let mut g = guard(true);
        g.on_tool_call_delta("{\"expression\":\"2+2\"}");
        g.on_finish(FinishReason::ToolCalls);
        g.mark_completed();

        assert!(
            g.assemble_response(&Usage::default()).is_none(),
            "the reassembled content would be JSON arguments, not an answer"
        );
    }

    #[test]
    fn an_empty_stream_yields_nothing_reusable() {
        let mut g = guard(true);
        g.on_finish(FinishReason::Stop);
        g.mark_completed();

        assert!(g.assemble_response(&Usage::default()).is_none());
    }

    #[test]
    fn drift_against_a_zero_upstream_count_depends_on_what_we_saw() {
        assert!(
            !drift_exceeds(0, 0),
            "nothing generated, nothing to disagree about"
        );
        assert!(
            drift_exceeds(50, 0),
            "we counted tokens upstream says do not exist"
        );
    }
}
