use crate::http::wire::{ErrorDetailOwned, ErrorEnvelopeOwned};
use crate::ratelimit::{RateLimitHeaders, RateLimitSnapshot};
use thiserror::Error;

/// Which limiter said no.
///
/// `429` on its own tells a client to slow down without telling it *what* to slow
/// down. Reducing request rate does nothing for a tenant that is over its token
/// budget on a single large prompt, and shrinking prompts does nothing for one
/// sending too many small ones. The dimension travels on `error.code`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitScope {
    /// Tenant exceeded requests-per-minute.
    Requests,
    /// Tenant exceeded tokens-per-minute.
    Tokens,
    /// Pre-auth per-IP flood protection.
    Ip,
    /// The upstream provider rate-limited us, and we are passing it on.
    Upstream,
}

impl RateLimitScope {
    pub fn code(self) -> &'static str {
        match self {
            Self::Requests => "requests_rate_limit_exceeded",
            Self::Tokens => "tokens_rate_limit_exceeded",
            Self::Ip => "ip_rate_limit_exceeded",
            Self::Upstream => "upstream_rate_limit_exceeded",
        }
    }

    /// The `sb_ratelimit_denied_total{dimension}` label. Kept distinct from
    /// [`Self::code`], which is a wire contract and must not move when a metric is
    /// renamed.
    pub fn dimension(self) -> &'static str {
        match self {
            Self::Requests => "rpm",
            Self::Tokens => "tpm",
            Self::Ip => "ip",
            Self::Upstream => "upstream",
        }
    }

    /// The `x-ratelimit-*` infix: `requests` or `tokens`.
    ///
    /// `None` for the limiters a tenant cannot see. The IP bucket is keyed on a source
    /// address, not on the caller's identity, so publishing its remaining count would
    /// leak one client's traffic level to every other client behind the same proxy.
    fn header_infix(self) -> Option<&'static str> {
        match self {
            Self::Requests => Some("requests"),
            Self::Tokens => Some("tokens"),
            Self::Ip | Self::Upstream => None,
        }
    }

    fn message(self) -> &'static str {
        match self {
            Self::Requests => {
                "Rate limit exceeded: too many requests per minute. \
                 Reduce your request rate; see x-ratelimit-remaining-requests."
            }
            Self::Tokens => {
                "Rate limit exceeded: too many tokens per minute. \
                 Send smaller prompts or fewer concurrent requests; \
                 see x-ratelimit-remaining-tokens."
            }
            Self::Ip => "Rate limit exceeded for this source address.",
            Self::Upstream => "Rate limit exceeded at the upstream provider.",
        }
    }
}

/// Renders one limiter's state as the three headers every OpenAI-compatible client
/// already knows how to read.
///
/// The reset is milliseconds with an explicit `ms` suffix, matching OpenAI's own
/// `x-ratelimit-reset-requests: 6ms`. Bare seconds would round every interesting
/// answer to `0` or `1`.
pub fn set_ratelimit_headers(headers: &mut http::HeaderMap, snapshots: &[RateLimitSnapshot]) {
    for snap in snapshots {
        let Some(infix) = snap.scope.header_infix() else {
            continue;
        };
        let mut put = |name: String, value: String| {
            if let (Ok(n), Ok(v)) = (
                http::HeaderName::try_from(name),
                http::HeaderValue::from_str(&value),
            ) {
                headers.insert(n, v);
            }
        };
        put(format!("x-ratelimit-limit-{infix}"), snap.limit.to_string());
        put(
            format!("x-ratelimit-remaining-{infix}"),
            snap.remaining.to_string(),
        );
        put(
            format!("x-ratelimit-reset-{infix}"),
            format!("{}ms", snap.reset_ms),
        );
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("{message}")]
    InvalidRequest {
        message: String,
        param: Option<String>,
    },
    #[error("Incorrect API key provided")]
    Authentication,
    #[error("{message}")]
    Permission { message: String },
    #[error("Rate limit exceeded")]
    RateLimit {
        retry_after_ms: Option<u64>,
        scope: RateLimitScope,
        /// The bucket as it stood when it refused. Rendered as `x-ratelimit-*` so a
        /// `429` carries the same telemetry a `200` does.
        snapshot: Option<RateLimitSnapshot>,
    },
    #[error("{message}")]
    InsufficientQuota { message: String },
    /// The same `Idempotency-Key` was reused with a different request body.
    ///
    /// `409`, not `400`: against a `400` a client cannot distinguish "your JSON is
    /// malformed" (fix the request) from "you reused a key" (mint a new one), and
    /// those have opposite remedies. Stripe and the IETF idempotency-key draft both
    /// use `409`.
    #[error("{message}")]
    IdempotencyConflict { message: String },
    /// The gateway is saturated and refused rather than queued.
    ///
    /// This is not a degradation of a client's p99 — it is the mechanism that
    /// produces one. See docs/15 §3. Always carries a `Retry-After`.
    #[error("{message}")]
    Overloaded {
        message: String,
        retry_after_ms: Option<u64>,
    },
    /// Every provider that could serve this model is broken (circuit breaker open).
    ///
    /// Distinct from [`Self::Overloaded`] on the wire only by `error.code`, because a
    /// client's correct reply to both is the same: back off, then retry. Distinct in
    /// the logs because the operator's reply is not.
    #[error("{message}")]
    ProviderUnavailable {
        message: String,
        retry_after_ms: Option<u64>,
    },
    /// The client's deadline expired before an answer existed.
    #[error("{message}")]
    DeadlineExceeded { message: String },
    #[error("{message}")]
    Server { message: String },
    #[error("The model `{model}` does not exist")]
    ModelNotFound { model: String },
    #[error("{message}")]
    NotFound { message: String },
}

impl ApiError {
    /// The gateway is out of capacity. Reject; never queue.
    pub fn overloaded(retry_after_ms: Option<u64>) -> Self {
        Self::Overloaded {
            message: "The gateway is over capacity and shed this request. \
                      Retry after the interval in Retry-After, with full jitter."
                .into(),
            retry_after_ms,
        }
    }

    pub fn error_type(&self) -> &'static str {
        match self {
            Self::InvalidRequest { .. } => "invalid_request_error",
            Self::Authentication => "authentication_error",
            Self::Permission { .. } => "permission_error",
            Self::RateLimit { .. } => "rate_limit_error",
            Self::InsufficientQuota { .. } => "insufficient_quota",
            Self::IdempotencyConflict { .. } => "idempotency_conflict",
            Self::Overloaded { .. } => "overloaded",
            Self::ProviderUnavailable { .. } => "provider_unavailable",
            Self::DeadlineExceeded { .. } => "deadline_exceeded",
            Self::Server { .. } => "server_error",
            Self::ModelNotFound { .. } => "invalid_request_error",
            Self::NotFound { .. } => "invalid_request_error",
        }
    }

    pub fn code(&self) -> Option<&'static str> {
        match self {
            Self::ModelNotFound { .. } => Some("model_not_found"),
            Self::NotFound { .. } => Some("not_found"),
            Self::RateLimit { scope, .. } => Some(scope.code()),
            Self::Overloaded { .. } => Some("gateway_overloaded"),
            Self::ProviderUnavailable { .. } => Some("provider_unavailable"),
            Self::DeadlineExceeded { .. } => Some("deadline_exceeded"),
            _ => None,
        }
    }

    pub fn status(&self) -> http::StatusCode {
        match self {
            Self::InvalidRequest { .. } | Self::ModelNotFound { .. } => {
                http::StatusCode::BAD_REQUEST
            }
            Self::NotFound { .. } => http::StatusCode::NOT_FOUND,
            Self::Authentication => http::StatusCode::UNAUTHORIZED,
            Self::Permission { .. } => http::StatusCode::FORBIDDEN,
            Self::IdempotencyConflict { .. } => http::StatusCode::CONFLICT,
            Self::RateLimit { .. } | Self::InsufficientQuota { .. } => {
                http::StatusCode::TOO_MANY_REQUESTS
            }
            Self::Overloaded { .. } | Self::ProviderUnavailable { .. } => {
                http::StatusCode::SERVICE_UNAVAILABLE
            }
            Self::DeadlineExceeded { .. } => http::StatusCode::GATEWAY_TIMEOUT,
            Self::Server { .. } => http::StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// How long the client should wait, in milliseconds, or `None` if waiting will
    /// not help.
    ///
    /// `insufficient_quota` deliberately has none: no amount of waiting refills a
    /// monthly budget, and a `Retry-After` there would send a fleet into a retry loop
    /// against a wall.
    fn retry_after_ms(&self) -> Option<u64> {
        match self {
            Self::RateLimit { retry_after_ms, .. }
            | Self::Overloaded { retry_after_ms, .. }
            | Self::ProviderUnavailable { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        }
    }

    /// The `outcome` label for `sb_request_duration_seconds`, and the thing that decides
    /// whether `sb_requests_shed_total` ticks.
    ///
    /// Status alone cannot answer this. Two very different `503`s exist: one where **we**
    /// ran out of capacity and refused, and one where **the provider** is down. The first
    /// belongs in the shed rate, whose SLO is 0.1 % at rated capacity. The second does not,
    /// or a provider outage would page whoever is on call for gateway saturation — which
    /// docs/15 §1 exists to prevent: *alert on overhead, dashboard the provider*.
    fn outcome(&self) -> &'static str {
        match self {
            // We refused because we were full. This is the mechanism, not the failure.
            Self::Overloaded { .. } => "shed",
            Self::DeadlineExceeded { .. } => "timeout",
            _ => "error",
        }
    }

    fn into_envelope(self) -> ErrorEnvelopeOwned {
        let r#type = self.error_type().to_string();
        let code = self.code().map(str::to_string);
        let (message, param) = match self {
            ApiError::InvalidRequest { message, param } => (message, param),
            ApiError::Authentication => ("Incorrect API key provided".into(), None),
            ApiError::Permission { message } => (message, None),
            ApiError::RateLimit { scope, .. } => (scope.message().into(), None),
            ApiError::InsufficientQuota { message }
            | ApiError::Overloaded { message, .. }
            | ApiError::ProviderUnavailable { message, .. }
            | ApiError::DeadlineExceeded { message }
            | ApiError::Server { message }
            | ApiError::NotFound { message } => (message, None),
            ApiError::IdempotencyConflict { message } => (message, Some("Idempotency-Key".into())),
            ApiError::ModelNotFound { model } => {
                (format!("The model `{model}` does not exist"), None)
            }
        };

        ErrorEnvelopeOwned {
            error: ErrorDetailOwned {
                message,
                r#type,
                param,
                code,
            },
        }
    }
}

/// Writes both spellings of "wait this long".
///
/// `Retry-After` is `delta-seconds` per RFC 9110 §10.2.3 — a proxy, a CDN, or any
/// standard client reads a bare integer there as **seconds**. Putting milliseconds in
/// it turns a 250 ms backoff into a 250 second one. So it is rounded *up* to whole
/// seconds (never down to zero, and never truncated below the real wait), and the
/// exact figure travels beside it in `retry-after-ms`, which is the header the OpenAI
/// and Anthropic SDKs prefer when both are present.
pub fn set_retry_after(headers: &mut http::HeaderMap, retry_after_ms: u64) {
    let secs = retry_after_ms.div_ceil(1000).max(1);
    if let Ok(v) = http::HeaderValue::from_str(&secs.to_string()) {
        headers.insert(http::header::RETRY_AFTER, v);
    }
    if let Ok(v) = http::HeaderValue::from_str(&retry_after_ms.to_string()) {
        headers.insert("retry-after-ms", v);
    }
}

/// Rides the response so `telemetry::inflight_layer` can label the histogram with what
/// actually happened, rather than guessing from a status code that two causes share.
#[derive(Clone, Copy, Debug)]
pub struct Outcome(pub &'static str);

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let retry_after_ms = self.retry_after_ms();
        let outcome = Outcome(self.outcome());
        let snapshot = match &self {
            Self::RateLimit { snapshot, .. } => *snapshot,
            _ => None,
        };
        let status = self.status();
        let body = self.into_envelope();
        let mut resp = (status, axum::Json(body)).into_response();
        if let Some(ms) = retry_after_ms {
            set_retry_after(resp.headers_mut(), ms);
        }
        resp.extensions_mut().insert(outcome);
        // The layer that renders these for a `200` never sees a handler's `Err`, so a
        // refusal has to carry its own. Without it the one response that most needs to
        // say "here is your limit" is the one that does not.
        if let Some(snap) = snapshot {
            resp.extensions_mut().insert(RateLimitHeaders(vec![snap]));
        }
        resp
    }
}

// Axum Json rejection → OpenAI-shaped 400
impl From<axum::extract::rejection::JsonRejection> for ApiError {
    fn from(err: axum::extract::rejection::JsonRejection) -> Self {
        Self::InvalidRequest {
            message: err.to_string(),
            param: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shed_and_deadline_codes_are_the_ones_clients_key_on() {
        assert_eq!(
            ApiError::overloaded(Some(500)).status(),
            http::StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ApiError::DeadlineExceeded {
                message: "x".into()
            }
            .status(),
            http::StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            ApiError::IdempotencyConflict {
                message: "x".into()
            }
            .status(),
            http::StatusCode::CONFLICT
        );
    }

    /// A `429` that does not say which limiter fired leaves a client guessing between
    /// two opposite remedies.
    #[test]
    fn a_429_names_the_dimension_that_denied_it() {
        let tokens = ApiError::RateLimit {
            retry_after_ms: Some(1),
            scope: RateLimitScope::Tokens,
            snapshot: None,
        };
        assert_eq!(tokens.error_type(), "rate_limit_error");
        assert_eq!(tokens.code(), Some("tokens_rate_limit_exceeded"));

        let requests = ApiError::RateLimit {
            retry_after_ms: Some(1),
            scope: RateLimitScope::Requests,
            snapshot: None,
        };
        assert_eq!(requests.code(), Some("requests_rate_limit_exceeded"));
    }

    /// The IP bucket is keyed on a source address that many tenants can share. Its
    /// remaining count is not the caller's to read.
    #[test]
    fn the_ip_bucket_is_never_published_to_the_client() {
        let mut headers = http::HeaderMap::new();
        set_ratelimit_headers(
            &mut headers,
            &[
                RateLimitSnapshot {
                    scope: RateLimitScope::Ip,
                    limit: 100,
                    remaining: 7,
                    reset_ms: 500,
                },
                RateLimitSnapshot {
                    scope: RateLimitScope::Requests,
                    limit: 60,
                    remaining: 59,
                    reset_ms: 1000,
                },
            ],
        );
        assert_eq!(headers["x-ratelimit-limit-requests"], "60");
        assert_eq!(headers["x-ratelimit-remaining-requests"], "59");
        assert_eq!(headers["x-ratelimit-reset-requests"], "1000ms");
        assert!(!headers.contains_key("x-ratelimit-limit-ip"));
    }

    /// RFC 9110 says `Retry-After` is seconds. A 250 ms wait must never round down to
    /// `0`, and must never be written as `250` — a client would sleep for four minutes.
    #[test]
    fn retry_after_rounds_up_to_seconds_and_keeps_the_exact_value_in_ms() {
        let mut headers = http::HeaderMap::new();

        set_retry_after(&mut headers, 250);
        assert_eq!(headers[http::header::RETRY_AFTER], "1");
        assert_eq!(headers["retry-after-ms"], "250");

        set_retry_after(&mut headers, 1500);
        assert_eq!(headers[http::header::RETRY_AFTER], "2");
        assert_eq!(headers["retry-after-ms"], "1500");

        set_retry_after(&mut headers, 3000);
        assert_eq!(headers[http::header::RETRY_AFTER], "3");
    }

    /// The shed-rate SLO is 0.1 % at rated capacity. If a provider outage counted as a
    /// shed, that SLO would fire for something nobody on call can fix — which is precisely
    /// the alert docs/15 §1 forbids.
    #[test]
    fn a_provider_outage_is_not_a_shed_even_though_both_are_503() {
        let shed = ApiError::overloaded(Some(1_000));
        let outage = ApiError::ProviderUnavailable {
            message: "x".into(),
            retry_after_ms: Some(10_000),
        };

        assert_eq!(shed.status(), outage.status(), "both are 503 on the wire");
        assert_eq!(shed.outcome(), "shed");
        assert_eq!(outage.outcome(), "error");

        assert_eq!(
            ApiError::DeadlineExceeded {
                message: "x".into()
            }
            .outcome(),
            "timeout"
        );
    }

    /// Waiting does not refill a monthly budget.
    #[test]
    fn insufficient_quota_carries_no_retry_after() {
        let e = ApiError::InsufficientQuota {
            message: "broke".into(),
        };
        assert_eq!(e.status(), http::StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.retry_after_ms(), None);
    }
}
