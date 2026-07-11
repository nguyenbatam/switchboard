use crate::app::AppState;
use crate::domain::tenant::AuthenticatedKey;
use crate::http::error::{ApiError, set_ratelimit_headers};
use crate::providers::Deadline;
use crate::ratelimit::RateLimitHeaders;
use axum::{
    extract::{ConnectInfo, FromRequestParts, Request, State},
    http::{HeaderMap, header::AUTHORIZATION, request::Parts},
    middleware::Next,
    response::Response,
};
use std::net::{IpAddr, SocketAddr};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct RequestId(pub String);

impl Default for RequestId {
    fn default() -> Self {
        Self::new()
    }
}

impl RequestId {
    pub fn new() -> Self {
        Self(format!("req_{}", Uuid::now_v7()))
    }
}

impl<S> FromRequestParts<S> for RequestId
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        if let Some(id) = parts
            .headers
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
        {
            return Ok(Self(id.to_string()));
        }
        Ok(Self::new())
    }
}

#[derive(Clone, Debug)]
pub struct TenantCtx {
    pub key: AuthenticatedKey,
    pub request_id: String,
    /// When the client stops wanting an answer. `None` means "as long as it takes".
    pub deadline: Option<Deadline>,
}

/// Renders `x-ratelimit-*` from whatever the limiters recorded on the response.
///
/// The snapshot travels in the response extensions rather than being written at the
/// call site because the two places that know the numbers — a handler that succeeded
/// and an `ApiError` that refused — sit on opposite sides of a `?`.
pub async fn ratelimit_headers_layer(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    if let Some(headers) = response.extensions().get::<RateLimitHeaders>().cloned() {
        set_ratelimit_headers(response.headers_mut(), &headers.0);
    }
    response
}

pub async fn auth_middleware(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("req_{}", Uuid::now_v7()));

    let deadline = parse_deadline(req.headers())?;

    // Checked before authenticating. An unauthenticated request costs the sender
    // nothing, and the most expensive thing it can reach is the argon2 verify below.
    if let Some(rl) = &state.rate_limiter {
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        let ip = client_ip(req.headers(), peer, &state.config.server.trusted_proxies);
        rl.check_ip(&ip, state.config.ratelimit.ip_rpm).await?;
    }

    let auth = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok());

    let key = if state.config.auth.require_auth {
        let header = auth.ok_or(ApiError::Authentication)?;
        state.auth.authenticate(header).await?
    } else if let Some(header) = auth {
        state
            .auth
            .authenticate(header)
            .await
            .unwrap_or_else(|_| state.auth.dev_tenant())
    } else {
        state.auth.dev_tenant()
    };

    req.extensions_mut().insert(TenantCtx {
        key,
        request_id,
        deadline,
    });

    Ok(next.run(req).await)
}

/// `x-switchboard-deadline-ms`: how much longer the client is willing to wait.
///
/// Relative, never absolute, and that is not a style choice. An absolute timestamp is
/// wrong the moment the client's clock and ours disagree, and they do — `grpc-timeout`
/// is relative for the same reason. The gateway starts its clock on receipt and eats
/// the network leg itself.
fn parse_deadline(headers: &HeaderMap) -> Result<Option<Deadline>, ApiError> {
    let Some(raw) = headers
        .get("x-switchboard-deadline-ms")
        .and_then(|v| v.to_str().ok())
    else {
        return Ok(None);
    };

    let ms: u64 = raw.trim().parse().map_err(|_| ApiError::InvalidRequest {
        message: format!(
            "x-switchboard-deadline-ms must be a positive integer of milliseconds, got '{raw}'"
        ),
        param: Some("x-switchboard-deadline-ms".into()),
    })?;

    if ms == 0 {
        return Err(ApiError::InvalidRequest {
            message: "x-switchboard-deadline-ms is already in the past".into(),
            param: Some("x-switchboard-deadline-ms".into()),
        });
    }

    Ok(Some(Deadline::in_ms(ms)))
}

/// The address to charge the pre-auth flood limiter.
///
/// `x-forwarded-for` is client-supplied. Trusting it unconditionally lets anyone evade
/// `ip_rpm` — the one limiter standing in front of the argon2 verify — by forging a
/// fresh address per request. So the socket address is the source of truth, and XFF is
/// read only when the peer is a proxy we configured.
///
/// The old fallback of `"unknown"` was its own denial of service: with no XFF present,
/// every caller on earth shared a single bucket and throttled each other.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>, trusted: &[IpAddr]) -> String {
    let Some(peer) = peer else {
        // No `ConnectInfo`, so there is no socket to trust. Fall back to a single
        // bucket rather than to a forgeable header: fail closed, not open.
        return "unknown".into();
    };

    if trusted.contains(&peer.ip())
        && let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.split(',').next())
            .map(str::trim)
            .filter(|s| !s.is_empty())
    {
        return forwarded.to_string();
    }

    peer.ip().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xff(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", value.parse().unwrap());
        h
    }

    fn sock(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    /// The limiter that protects argon2 must not be bypassable by a header.
    #[test]
    fn an_untrusted_peer_cannot_forge_its_own_rate_limit_bucket() {
        assert_eq!(
            client_ip(&xff("1.2.3.4"), Some(sock("9.9.9.9:1234")), &[]),
            "9.9.9.9"
        );
    }

    #[test]
    fn a_trusted_proxy_is_believed() {
        let trusted = vec!["10.0.0.1".parse().unwrap()];
        assert_eq!(
            client_ip(
                &xff("1.2.3.4, 10.0.0.1"),
                Some(sock("10.0.0.1:1234")),
                &trusted
            ),
            "1.2.3.4"
        );
    }

    /// Every caller sharing one bucket is a limiter that throttles the innocent.
    #[test]
    fn peers_without_a_forwarded_header_do_not_collapse_into_one_bucket() {
        let a = client_ip(&HeaderMap::new(), Some(sock("9.9.9.9:1")), &[]);
        let b = client_ip(&HeaderMap::new(), Some(sock("8.8.8.8:1")), &[]);
        assert_ne!(a, b);
    }

    #[test]
    fn a_deadline_must_be_a_positive_relative_integer() {
        let mut h = HeaderMap::new();
        h.insert("x-switchboard-deadline-ms", "2500".parse().unwrap());
        assert!(parse_deadline(&h).unwrap().is_some());

        h.insert("x-switchboard-deadline-ms", "0".parse().unwrap());
        assert!(parse_deadline(&h).is_err());

        h.insert("x-switchboard-deadline-ms", "soon".parse().unwrap());
        assert!(parse_deadline(&h).is_err());

        assert!(parse_deadline(&HeaderMap::new()).unwrap().is_none());
    }
}
