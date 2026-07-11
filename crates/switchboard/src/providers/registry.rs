use super::{CallCx, Caps, ModelProvider, ProviderError};
use crate::config::AppConfig;
use crate::config::models::{ModelRegistry, ModelSpec};
use crate::domain::chat::{ChatRequest, ChatResponse};
use crate::domain::embed::{EmbedRequest, EmbedResponse};
use crate::domain::stream::StreamEvent;
use crate::reliability::{BreakerState, CircuitBreaker, RetryBudget, with_retry};
use futures::stream::BoxStream;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

pub fn error_kind(e: &ProviderError) -> &'static str {
    match e {
        ProviderError::RateLimited { .. } => "rate_limited",
        ProviderError::Overloaded => "overloaded",
        ProviderError::Busy => "busy",
        ProviderError::Timeout(_) => "timeout",
        ProviderError::Upstream5xx { .. } => "upstream_5xx",
        ProviderError::Auth => "auth",
        ProviderError::BadRequest { .. } => "bad_request",
        ProviderError::ContextLength { .. } => "context_length",
        ProviderError::ContentFilter => "content_filter",
        ProviderError::Cancelled => "cancelled",
        ProviderError::DeadlineExceeded => "deadline_exceeded",
        ProviderError::Decode(_) => "decode",
        ProviderError::Http(_) => "http",
        ProviderError::Other(_) => "other",
    }
}

pub struct ProviderRegistry {
    pub model_registry: ModelRegistry,
    providers: HashMap<String, Arc<dyn ModelProvider>>,
    breakers: Mutex<HashMap<String, CircuitBreaker>>,
    /// One budget per provider: a struggling OpenAI must not spend the retries
    /// that Ollama is entitled to.
    retry_budgets: HashMap<String, RetryBudget>,
    /// A hard in-flight cap per provider. The most important limit in this file.
    ///
    /// A gateway spends almost no CPU; it blocks on upstreams. So the scarce resource is
    /// **provider capacity, not HTTP request count**, and a global `ConcurrencyLimit(256)`
    /// measures the wrong quantity entirely. Pushing 256 requests at a provider that can
    /// serve eight produces 256 people waiting. Serving eight quickly and handing the rest
    /// a `503` they can act on is strictly better for all 256.
    ///
    /// Always `try_acquire`, never `acquire().await`. Awaiting the permit rebuilds the
    /// unbounded queue one layer down, which is the exact bug this exists to prevent.
    semaphores: HashMap<String, Arc<tokio::sync::Semaphore>>,
    redis: Option<deadpool_redis::Pool>,
    /// `(checked_at, healthy)`. See [`ProviderRegistry::any_provider_healthy`].
    health_cache: Mutex<Option<(std::time::Instant, bool)>>,
}

impl ProviderRegistry {
    pub fn new(config: &AppConfig, model_registry: ModelRegistry) -> Self {
        Self::with_redis(config, model_registry, None)
    }

    pub fn with_redis(
        config: &AppConfig,
        model_registry: ModelRegistry,
        redis: Option<deadpool_redis::Pool>,
    ) -> Self {
        let mut providers: HashMap<String, Arc<dyn ModelProvider>> = HashMap::new();
        providers.insert(
            "ollama".into(),
            Arc::new(super::ollama::OllamaProvider::with_timeouts(
                config.providers.ollama.base_url.clone(),
                (&config.providers.ollama.timeouts).into(),
            )),
        );

        if let Some(key) = AppConfig::openai_api_key() {
            providers.insert(
                "openai".into(),
                Arc::new(super::openai::OpenAiProvider::with_timeouts(
                    config.providers.openai.base_url.clone(),
                    key,
                    (&config.providers.openai.timeouts).into(),
                )),
            );
        }

        let retry_budgets = providers
            .keys()
            .map(|name| (name.clone(), RetryBudget::default()))
            .collect();

        let semaphores = providers
            .keys()
            .map(|name| {
                let permits = match name.as_str() {
                    "openai" => config.providers.openai.max_inflight,
                    _ => config.providers.ollama.max_inflight,
                };
                let permits = permits.max(1);
                crate::telemetry::metrics::set_provider_inflight_limit(name, permits);
                (name.clone(), Arc::new(tokio::sync::Semaphore::new(permits)))
            })
            .collect();

        Self {
            model_registry,
            providers,
            breakers: Mutex::new(HashMap::new()),
            retry_budgets,
            semaphores,
            redis,
            health_cache: Mutex::new(None),
        }
    }

    /// A permit, or [`ProviderError::Busy`]. Never waits.
    fn acquire(&self, provider_name: &str) -> Result<ProviderPermit, ProviderError> {
        let Some(sem) = self.semaphores.get(provider_name) else {
            return Ok(ProviderPermit::unlimited());
        };
        let permit = sem
            .clone()
            .try_acquire_owned()
            .map_err(|_| ProviderError::Busy)?;
        Ok(ProviderPermit::held(provider_name, permit))
    }

    fn provider_for(
        &self,
        alias: &str,
    ) -> Result<(&ModelSpec, Arc<dyn ModelProvider>), ProviderError> {
        let spec = self
            .model_registry
            .get(alias)
            .ok_or_else(|| ProviderError::Other(format!("model not found: {alias}")))?;
        let provider = self.providers.get(&spec.provider).cloned().ok_or_else(|| {
            ProviderError::Other(format!("provider '{}' not configured", spec.provider))
        })?;
        Ok((spec, provider))
    }

    fn budget_for(&self, provider_name: &str) -> &RetryBudget {
        static FALLBACK: OnceLock<RetryBudget> = OnceLock::new();
        self.retry_budgets
            .get(provider_name)
            .unwrap_or_else(|| FALLBACK.get_or_init(RetryBudget::default))
    }

    fn check_breaker(&self, provider_name: &str) -> Result<(), ProviderError> {
        let (allowed, state, changed) = {
            let mut breakers = self.breakers.lock().unwrap_or_else(|e| e.into_inner());
            let breaker = breakers.entry(provider_name.to_string()).or_default();
            let before = breaker.state();
            let allowed = breaker.allow();
            let after = breaker.state();
            (allowed, after, before != after)
        };

        self.publish_breaker_state(provider_name, state, changed);
        if allowed {
            Ok(())
        } else {
            Err(ProviderError::Overloaded)
        }
    }

    /// Records the outcome of one provider call.
    ///
    /// Errors are counted here rather than at the HTTP layer, because a fallback
    /// that rescues the request would otherwise hide the failure that triggered it.
    /// "The upstream is failing and users have not noticed yet" is exactly what the
    /// provider-errors panel exists to show.
    fn record_outcome<T>(&self, provider_name: &str, result: &Result<T, ProviderError>) {
        if let Err(e) = result {
            // A client that hung up or ran out of deadline says nothing about the
            // provider's health. Counting it would let a fleet of impatient callers trip
            // a breaker on an upstream that never failed.
            if !matches!(
                e,
                ProviderError::Cancelled | ProviderError::DeadlineExceeded
            ) {
                crate::telemetry::metrics::record_provider_error(provider_name, error_kind(e));
            } else {
                return;
            }
        }

        let (state, changed) = {
            let mut breakers = self.breakers.lock().unwrap_or_else(|e| e.into_inner());
            let breaker = breakers.entry(provider_name.to_string()).or_default();
            let before = breaker.state();
            breaker.record(result.is_ok());
            let after = breaker.state();
            (after, before != after)
        };
        self.publish_breaker_state(provider_name, state, changed);
    }

    /// The gauge is for alerting; the Redis key lets a freshly started replica see
    /// that a provider is already known-bad rather than rediscovering it the hard way.
    ///
    /// `changed` is why this is not two Redis writes per request. The gauge is a local
    /// atomic and costs nothing, but the `SET` used to fire from both `check_breaker` and
    /// `record_outcome` on every single call, to store a value that changes a few times a
    /// day. State transitions are the only thing worth publishing.
    fn publish_breaker_state(&self, provider_name: &str, state: BreakerState, changed: bool) {
        crate::telemetry::metrics::set_circuit_breaker_state(provider_name, state);

        if !changed {
            return;
        }
        tracing::warn!(provider = %provider_name, state = %state.as_str(), "circuit breaker changed state");

        let Some(redis) = &self.redis else { return };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };

        let redis = redis.clone();
        let key = format!("cb:{provider_name}");
        let value = state.as_str();
        handle.spawn(async move {
            use redis::AsyncCommands;
            let result: Result<(), Box<dyn std::error::Error + Send + Sync>> = async {
                let mut conn = redis.get().await?;
                let _: () = conn.set_ex(&key, value, 60).await?;
                Ok(())
            }
            .await;
            if let Err(e) = result {
                tracing::debug!(error = %e, "failed to publish breaker state");
            }
        });
    }

    pub fn capabilities(&self, alias: &str) -> Caps {
        self.provider_for(alias)
            .map(|(spec, p)| p.capabilities(&spec.upstream_name))
            .unwrap_or_default()
    }

    pub async fn chat(
        &self,
        alias: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<ChatResponse, ProviderError> {
        match self.chat_inner(alias, req, cx).await {
            Ok(resp) => Ok(resp),
            Err(e) => self.try_fallback_chat(alias, req, cx, e).await,
        }
    }

    async fn chat_inner(
        &self,
        alias: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<ChatResponse, ProviderError> {
        let (spec, provider) = self.provider_for(alias)?;
        self.check_breaker(&spec.provider)?;
        // Dead work is the most expensive thing a gateway can do: real tokens, real
        // money, for an answer nobody is still waiting for. Checked here rather than at
        // the handler because a fallback attempt deserves the same question asked again.
        cx.check_deadline()?;

        // Held for the whole call, including its retries. Released on drop.
        let _permit = self.acquire(&spec.provider)?;

        let upstream = spec.upstream_name.clone();
        let provider_name = spec.provider.clone();
        let p = provider.clone();

        let result = with_retry(self.budget_for(&provider_name), || {
            let p = p.clone();
            let upstream = upstream.clone();
            let req = req.clone();
            let cx = cx.clone();
            async move {
                cx.check_deadline()?;
                p.chat(&upstream, &req, &cx).await
            }
        })
        .await;

        self.record_outcome(&provider_name, &result);
        result
    }

    async fn try_fallback_chat(
        &self,
        alias: &str,
        req: &ChatRequest,
        cx: &CallCx,
        original_err: ProviderError,
    ) -> Result<ChatResponse, ProviderError> {
        let Some(spec) = self.model_registry.get(alias) else {
            return Err(original_err);
        };

        for fb_alias in &spec.fallback {
            tracing::warn!(from = %alias, to = %fb_alias, "provider fallback");
            match self.chat_inner(fb_alias, req, cx).await {
                Ok(mut resp) => {
                    resp.model = fb_alias.clone();
                    // Recorded on the usage row, so "what fraction of traffic was
                    // served by the fallback" stays an answerable question.
                    resp.served_by = Some(fb_alias.clone());
                    return Ok(resp);
                }
                Err(e) => {
                    tracing::warn!(fallback = %fb_alias, error = %e, "fallback failed");
                }
            }
        }
        Err(original_err)
    }

    /// Opens a stream, falling back to another model if this one will not open.
    ///
    /// The key fact, and the reason this is safe where *retrying* a stream is not:
    /// `stream()` returns **before the first byte leaves the server**. Response headers
    /// are not yet committed, so choosing a different provider here cannot duplicate an
    /// answer. The objection that correctly forbids retrying a stream mid-flight does not
    /// reach a stream that never started.
    ///
    /// Production streams. Without this, the entire fallback mechanism — every
    /// `fallback = [...]` in the model registry — essentially never ran.
    pub async fn stream(
        &self,
        alias: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<StreamHandle, ProviderError> {
        match self.stream_inner(alias, req, cx).await {
            Ok((stream, permit)) => Ok(StreamHandle {
                stream,
                served_by: None,
                permit,
            }),
            Err(original) => {
                let Some(spec) = self.model_registry.get(alias) else {
                    return Err(original);
                };
                for fb_alias in &spec.fallback {
                    tracing::warn!(from = %alias, to = %fb_alias, "provider fallback (stream)");
                    match self.stream_inner(fb_alias, req, cx).await {
                        Ok((stream, permit)) => {
                            return Ok(StreamHandle {
                                stream,
                                served_by: Some(fb_alias.clone()),
                                permit,
                            });
                        }
                        Err(e) => {
                            tracing::warn!(fallback = %fb_alias, error = %e, "stream fallback failed")
                        }
                    }
                }
                Err(original)
            }
        }
    }

    #[allow(clippy::type_complexity)]
    async fn stream_inner(
        &self,
        alias: &str,
        req: &ChatRequest,
        cx: &CallCx,
    ) -> Result<
        (
            BoxStream<'static, Result<StreamEvent, ProviderError>>,
            ProviderPermit,
        ),
        ProviderError,
    > {
        let (spec, provider) = self.provider_for(alias)?;
        self.check_breaker(&spec.provider)?;
        cx.check_deadline()?;

        // Handed to the caller, because the slot is occupied until the last token, not
        // until the response headers.
        let permit = self.acquire(&spec.provider)?;

        // Deliberately not *retried*: a retry loop here would re-enter after a partial
        // response had begun. Falling back before the first byte is a different thing.
        let result = provider.stream(&spec.upstream_name, req, cx).await;
        self.record_outcome(&spec.provider, &result);
        result.map(|stream| (stream, permit))
    }

    pub async fn embed(
        &self,
        alias: &str,
        req: &EmbedRequest,
        cx: &CallCx,
    ) -> Result<EmbedResponse, ProviderError> {
        let (spec, provider) = self.provider_for(alias)?;
        self.check_breaker(&spec.provider)?;
        cx.check_deadline()?;
        let _permit = self.acquire(&spec.provider)?;
        let upstream = spec.upstream_name.clone();
        let provider_name = spec.provider.clone();
        let p = provider.clone();

        let result = with_retry(self.budget_for(&provider_name), || {
            let p = p.clone();
            let upstream = upstream.clone();
            let req = req.clone();
            let cx = cx.clone();
            async move { p.embed(&upstream, &req, &cx).await }
        })
        .await;

        self.record_outcome(&provider_name, &result);
        result
    }

    /// Whether any upstream answers, cached briefly.
    ///
    /// `/ready` is polled every couple of seconds by every replica. Without the cache,
    /// each poll fans out a live health request to every provider, so a fleet of N pods
    /// aims N × providers × poll-rate at upstreams that are already the scarce resource.
    /// Two seconds of staleness is well inside any orchestrator's readiness threshold.
    pub async fn any_provider_healthy(&self) -> bool {
        const TTL: std::time::Duration = std::time::Duration::from_secs(2);

        if let Some((at, healthy)) = *self.health_cache.lock().unwrap_or_else(|e| e.into_inner())
            && at.elapsed() < TTL
        {
            return healthy;
        }

        let mut healthy = false;
        for p in self.providers.values() {
            if p.healthy().await {
                healthy = true;
                break;
            }
        }

        *self.health_cache.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::time::Instant::now(), healthy));
        healthy
    }
}

/// A stream, plus the model that actually opened it, plus the upstream slot it occupies.
///
/// `served_by` is `Some` only when a fallback answered. It becomes both the
/// `x-switchboard-fallback` header and the `served_by_alias` column, so "what fraction
/// of streamed traffic was served by the fallback" stays an answerable question.
pub struct StreamHandle {
    pub stream: BoxStream<'static, Result<StreamEvent, ProviderError>>,
    pub served_by: Option<String>,
    /// **Must outlive the stream body, not the response headers.**
    ///
    /// A streaming request holds an upstream slot until its last token. Dropping the
    /// permit when `stream()` returns would free the slot at the instant the answer starts
    /// arriving, and the cap would bound nothing. The caller hands this to `StreamGuard`,
    /// which rides the response body and whose `Drop` fires on the last chunk, a dead
    /// socket, or a cancelled task alike.
    pub permit: ProviderPermit,
}

/// One in-flight slot at one provider, released on drop.
///
/// Also the `sb_provider_inflight{provider}` gauge: it rises when a permit is taken and
/// falls when it is dropped, which is the only place both facts are known.
#[derive(Debug)]
pub struct ProviderPermit {
    _permit: Option<tokio::sync::OwnedSemaphorePermit>,
    provider: Option<String>,
}

impl ProviderPermit {
    /// For a provider with no configured cap. Costs nothing and releases nothing.
    fn unlimited() -> Self {
        Self {
            _permit: None,
            provider: None,
        }
    }

    fn held(provider: &str, permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        crate::telemetry::metrics::provider_inflight_enter(provider);
        Self {
            _permit: Some(permit),
            provider: Some(provider.to_string()),
        }
    }
}

impl Drop for ProviderPermit {
    fn drop(&mut self) {
        if let Some(provider) = &self.provider {
            crate::telemetry::metrics::provider_inflight_exit(provider);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(ollama_max: usize) -> ProviderRegistry {
        let mut config: AppConfig = toml::from_str(
            &std::fs::read_to_string("../../config/switchboard.toml")
                .expect("the shipped config is the one under test"),
        )
        .expect("config parses");
        config.providers.ollama.max_inflight = ollama_max;
        let models = ModelRegistry::load("../../config/models.toml").expect("models config loads");
        ProviderRegistry::new(&config, models)
    }

    /// The cap must **refuse**, not wait.
    ///
    /// `acquire().await` would rebuild the unbounded queue one layer below the load
    /// shedder — request 257 parks, memory grows, and the gateway slows until it dies
    /// without ever emitting a `503`. `try_acquire` is what makes p99 a constant.
    #[test]
    fn a_full_provider_refuses_rather_than_queues() {
        let registry = registry_with(2);

        let a = registry.acquire("ollama").expect("first permit");
        let b = registry.acquire("ollama").expect("second permit");

        assert!(
            matches!(registry.acquire("ollama"), Err(ProviderError::Busy)),
            "a third caller must be told the provider is full, not made to wait"
        );

        // Busy is not Overloaded. One licenses a fallback; the other forbids the call.
        drop(a);
        assert!(
            registry.acquire("ollama").is_ok(),
            "a released slot is reusable"
        );
        drop(b);
    }

    /// A permit released by `Drop` is what makes the cap survive a panicking handler or a
    /// client that hangs up mid-stream.
    #[test]
    fn permits_come_back_when_the_holder_is_dropped() {
        let registry = registry_with(1);
        {
            let _held = registry.acquire("ollama").expect("permit");
            assert!(matches!(
                registry.acquire("ollama"),
                Err(ProviderError::Busy)
            ));
        }
        assert!(registry.acquire("ollama").is_ok());
    }

    /// An unconfigured provider is uncapped rather than unusable.
    #[test]
    fn a_provider_with_no_semaphore_is_never_busy() {
        let registry = registry_with(1);
        assert!(registry.acquire("no-such-provider").is_ok());
        assert!(registry.acquire("no-such-provider").is_ok());
    }
}
