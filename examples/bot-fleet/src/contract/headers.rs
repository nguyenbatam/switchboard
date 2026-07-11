//! What the gateway tells you on every response, and what a client owes it in return.
//!
//! `docs/15 §8` says a client must bound its own in-flight requests and honour
//! `Retry-After`. Both of those are reactions: they happen *after* a `429` or a `503`. The
//! headers in this module are the other half — the gateway publishing, on every single
//! response, exactly how much headroom is left — and a client that reads them never has to
//! learn its limit by hitting it in the middle of somebody's sentence.
//!
//! Nothing here does I/O. It is arithmetic over strings, so it can be tested as arithmetic.

use std::time::Duration;

use reqwest::header::HeaderMap;

/// One limiter's headroom, from `x-ratelimit-{limit,remaining,reset}-{requests,tokens}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub limit: u64,
    pub remaining: u64,
    /// How long until the bucket is full again.
    pub reset: Duration,
}

impl Bucket {
    pub fn is_empty(&self) -> bool {
        self.remaining == 0
    }
}

/// Both of a tenant's limiters, as of the response that carried them.
///
/// They are exceeded separately and the remedies are opposite: a tenant over its RPM must
/// send *fewer* requests, and one over its TPM must send *smaller* ones. Merging them into
/// a single "rate limit" number would tell a client to do one when it needed the other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RateLimitView {
    pub requests: Option<Bucket>,
    pub tokens: Option<Bucket>,
}

impl RateLimitView {
    pub fn from_headers(h: &HeaderMap) -> Option<Self> {
        let view = Self {
            requests: bucket(h, "requests"),
            tokens: bucket(h, "tokens"),
        };
        (view.requests.is_some() || view.tokens.is_some()).then_some(view)
    }

    /// How long to wait before the next request will not be refused, or `None` if now.
    ///
    /// The longer of the two waits, because a request that fits the token bucket and not
    /// the request bucket is still a `429`. Taking the shorter one would fire early, eat
    /// the refusal, and learn nothing it did not already know.
    pub fn wait_before_next(&self) -> Option<Duration> {
        let waits = [self.requests, self.tokens]
            .into_iter()
            .flatten()
            .filter(Bucket::is_empty)
            .map(|b| b.reset);
        waits.max()
    }
}

/// The tenant's month-to-date position, from `x-switchboard-budget-*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetView {
    pub limit_nano: i64,
    pub spent_nano: i64,
    /// **Signed.** The gateway's budget check is a pre-check against already-recorded
    /// spend, so a tenant overshoots by roughly one in-flight request and this goes
    /// negative. A client that parses it as unsigned reads a small overspend as an
    /// astronomically large balance, and keeps spending.
    pub remaining_nano: i64,
}

impl BudgetView {
    pub fn from_headers(h: &HeaderMap) -> Option<Self> {
        Some(Self {
            limit_nano: int(h, "x-switchboard-budget-limit-nano")?,
            spent_nano: int(h, "x-switchboard-budget-spend-nano")?,
            remaining_nano: int(h, "x-switchboard-budget-remaining-nano")?,
        })
    }

    /// Whether the next request is worth attempting.
    ///
    /// `reserve_nano` is what the caller believes one answer costs. Firing a request that
    /// cannot be paid for spends a user's patience and a provider's slot to be told
    /// something the previous response already said.
    pub fn can_afford(&self, reserve_nano: i64) -> bool {
        self.remaining_nano >= reserve_nano
    }
}

/// Where a response body came from, when it did not come from a provider just now.
///
/// Both replay a stored body — including its `usage` object, with real token counts — but
/// they mean **opposite things about money**, and a client that merges them corrupts its
/// own ledger in one direction or the other.
///
/// * `cache_hit`: no provider ran, and the gateway billed **zero**. Count zero.
/// * `replay`: a provider ran on this message's *first attempt*, and the gateway billed for
///   it exactly once, on the usage row carrying this same `x-request-id`. The retry bought
///   nothing extra. Count the answer once, at **full price**.
///
/// Price a replay at zero and your books come in under the invoice. Price a cache hit at
/// full rate and they come in over it. Neither error announces itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provenance {
    /// `x-switchboard-cache: hit`. No provider was called; nothing was billed.
    pub cache_hit: bool,
    /// `x-switchboard-replay: idempotent`. An earlier attempt of *this* request paid.
    pub replay: bool,
}

impl Provenance {
    pub fn from_headers(h: &HeaderMap) -> Self {
        Self {
            cache_hit: str_header(h, "x-switchboard-cache").as_deref() == Some("hit"),
            replay: str_header(h, "x-switchboard-replay").is_some(),
        }
    }

    /// Whether the gateway charged nothing for **this** response.
    ///
    /// True for a cache hit. **False for a replay**: the charge exists, it simply happened
    /// on the attempt whose response never reached us.
    pub fn gateway_billed_zero(&self) -> bool {
        self.cache_hit
    }
}

pub fn str_header(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
}

fn int(h: &HeaderMap, name: &str) -> Option<i64> {
    str_header(h, name)?.trim().parse().ok()
}

fn bucket(h: &HeaderMap, infix: &str) -> Option<Bucket> {
    Some(Bucket {
        limit: int(h, &format!("x-ratelimit-limit-{infix}"))?.max(0) as u64,
        remaining: int(h, &format!("x-ratelimit-remaining-{infix}"))?.max(0) as u64,
        reset: parse_reset(&str_header(h, &format!("x-ratelimit-reset-{infix}"))?)?,
    })
}

/// `250ms`, `4171ms`, or a bare integer read as milliseconds.
///
/// OpenAI writes the unit suffix and switchboard follows it. A client that strips digits
/// and ignores the `ms` would read `4171ms` as 4171 *seconds* and stall for over an hour.
fn parse_reset(raw: &str) -> Option<Duration> {
    let raw = raw.trim();
    let ms: u64 = raw.strip_suffix("ms").unwrap_or(raw).trim().parse().ok()?;
    Some(Duration::from_millis(ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn a_reset_carries_its_unit_and_is_milliseconds() {
        assert_eq!(parse_reset("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_reset(" 4171ms "), Some(Duration::from_millis(4171)));
        // A bare integer is still milliseconds, not seconds. Reading `4171` as seconds
        // would stall this fleet for seventy minutes.
        assert_eq!(parse_reset("4171"), Some(Duration::from_millis(4171)));
        assert_eq!(parse_reset("soon"), None);
    }

    #[test]
    fn both_limiters_are_read_separately() {
        let h = headers(&[
            ("x-ratelimit-limit-requests", "600"),
            ("x-ratelimit-remaining-requests", "599"),
            ("x-ratelimit-reset-requests", "100ms"),
            ("x-ratelimit-limit-tokens", "60000"),
            ("x-ratelimit-remaining-tokens", "55896"),
            ("x-ratelimit-reset-tokens", "4104ms"),
        ]);
        let v = RateLimitView::from_headers(&h).expect("both buckets");
        assert_eq!(v.requests.unwrap().remaining, 599);
        assert_eq!(v.tokens.unwrap().reset, Duration::from_millis(4104));
        assert_eq!(v.wait_before_next(), None, "neither bucket is empty");
    }

    /// A request that fits the token bucket and not the request bucket is still a `429`.
    /// Waiting the shorter of the two fires early and learns nothing.
    #[test]
    fn an_empty_bucket_dictates_the_wait_and_the_longest_one_wins() {
        let h = headers(&[
            ("x-ratelimit-limit-requests", "2"),
            ("x-ratelimit-remaining-requests", "0"),
            ("x-ratelimit-reset-requests", "30000ms"),
            ("x-ratelimit-limit-tokens", "60000"),
            ("x-ratelimit-remaining-tokens", "0"),
            ("x-ratelimit-reset-tokens", "4104ms"),
        ]);
        let v = RateLimitView::from_headers(&h).unwrap();
        assert_eq!(v.wait_before_next(), Some(Duration::from_millis(30_000)));
    }

    #[test]
    fn no_ratelimit_headers_is_no_view_rather_than_an_empty_one() {
        assert_eq!(RateLimitView::from_headers(&HeaderMap::new()), None);
    }

    /// The gateway sends this negative once the pre-check has been overshot.
    #[test]
    fn a_negative_balance_stays_negative() {
        let h = headers(&[
            ("x-switchboard-budget-limit-nano", "5000"),
            ("x-switchboard-budget-spend-nano", "12000"),
            ("x-switchboard-budget-remaining-nano", "-7000"),
        ]);
        let b = BudgetView::from_headers(&h).unwrap();
        assert_eq!(b.remaining_nano, -7000);
        assert!(!b.can_afford(1), "an overspent tenant affords nothing");
        assert!(!b.can_afford(0), "not even a free one, once the ledger is red");
    }

    #[test]
    fn affordability_is_measured_against_what_one_answer_costs() {
        let h = headers(&[
            ("x-switchboard-budget-limit-nano", "10000"),
            ("x-switchboard-budget-spend-nano", "4000"),
            ("x-switchboard-budget-remaining-nano", "6000"),
        ]);
        let b = BudgetView::from_headers(&h).unwrap();
        assert!(b.can_afford(6000));
        assert!(!b.can_afford(6001));
    }

    #[test]
    fn a_partial_budget_header_set_is_no_view() {
        let h = headers(&[("x-switchboard-budget-limit-nano", "5000")]);
        assert_eq!(BudgetView::from_headers(&h), None);
    }

    /// The distinction the whole type exists for. A cache hit was never charged. A replay
    /// *was* charged — on the attempt whose answer never arrived — and pricing it at zero
    /// makes a client under-report its own spend.
    #[test]
    fn a_replay_was_paid_for_and_a_cache_hit_never_was() {
        let cached = Provenance::from_headers(&headers(&[("x-switchboard-cache", "hit")]));
        assert!(cached.cache_hit && !cached.replay);
        assert!(cached.gateway_billed_zero());

        let replayed =
            Provenance::from_headers(&headers(&[("x-switchboard-replay", "idempotent")]));
        assert!(replayed.replay && !replayed.cache_hit);
        assert!(
            !replayed.gateway_billed_zero(),
            "a replay is already paid for, not free"
        );

        let generated = Provenance::from_headers(&HeaderMap::new());
        assert!(!generated.cache_hit && !generated.replay && !generated.gateway_billed_zero());
    }
}
