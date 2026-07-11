//! The switchboard client contract, implemented.
//!
//! Every obligation in `docs/14 §8` and `docs/15 §8` shows up here as code, and each
//! one carries the failure mode it prevents. If you are integrating with switchboard
//! from another language, this file is the specification you are re-implementing.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::backoff;
use super::headers::{BudgetView, Provenance, RateLimitView};
use super::retry_budget::RetryBudget;

/// What a single attempt against the gateway produced.
///
/// The failure modes are kept apart because a client must react to them differently, and
/// `docs/15 §7` is the reason each is a distinct status code — or, for the two that share
/// `429`, a distinct `error.type`.
#[derive(Debug)]
pub enum Outcome {
    /// Boxed because it is by far the largest variant, and every *other* variant is what a
    /// saturated gateway returns. Inlining it would make the failure paths pay, in stack
    /// and in memcpy, for the size of the success path.
    Answered(Box<Answer>),
    /// `503` — the gateway is saturated. Not your fault; every other client got this
    /// too, at the same instant. Back off *and* shrink concurrency.
    Busy {
        retry_after: Option<Duration>,
    },
    /// `429 rate_limit_exceeded` — you exceeded your own quota. Back off. Do **not**
    /// shrink concurrency: this says nothing about gateway capacity.
    Throttled {
        retry_after: Option<Duration>,
    },
    /// `429 insufficient_quota` — **you are out of money.**
    ///
    /// Shares a status code with `Throttled` and shares nothing else. Backing off does not
    /// help; waiting does not help; retrying burns nothing but does not succeed until
    /// somebody tops the account up. A fleet that conflates the two will retry until the
    /// end of the billing month.
    QuotaExhausted {
        message: String,
    },
    /// `504` — the deadline expired inside the gateway. Retrying costs money for an
    /// answer nobody is waiting for.
    DeadlineExceeded,
    /// `4xx` that a retry cannot fix: malformed body, bad key, idempotency conflict.
    Fatal(String),
    /// Transport died before a response. Retryable, but only before the first byte —
    /// which for `stream: false` is always.
    Transport(String),
}

#[derive(Debug, Clone)]
pub struct Answer {
    pub text: String,
    /// The model that **actually** answered, from `response.model`. `docs/08 §3`: a caller
    /// who checks it sees the truth about a fallback.
    pub served_by: String,
    pub fallback: Option<String>,
    pub hedged: Option<String>,
    /// From `x-switchboard-cache`. A hit consumed no provider capacity and cost nothing —
    /// but the replayed body still reports real token counts, so a client that prices its
    /// own spend from tokens must check this or it will bill itself for free answers.
    pub cache_hit: bool,
    /// From `x-switchboard-replay`. **Not** the same thing as `cache_hit`, and conflating
    /// them corrupts the ledger in the opposite direction.
    ///
    /// A cache hit means no provider ran and the gateway billed nothing, so the client must
    /// count zero. A replay means a provider *did* run — on this message's first attempt —
    /// and the gateway billed for it exactly once, on the usage row that carries this same
    /// `x-request-id`. The retry bought nothing extra. So the client counts this answer once,
    /// at full price, and a ledger that prices it at zero will under-report its own spend
    /// and be surprised by the invoice.
    ///
    /// What the flag is *for*: it is proof the `Idempotency-Key` worked. A replay after a
    /// retry is the system behaving. A replay with `attempts == 1` is a key collision, and
    /// [`crate::economics::audit`] treats it as a finding.
    pub replay: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// The tenant's headroom, as of this response. Read it and you never meet a `429`.
    pub rate_limit: Option<RateLimitView>,
    /// The tenant's month-to-date balance, as of this response.
    pub budget: Option<BudgetView>,
    /// Wire time for the attempt that succeeded.
    pub latency: Duration,
    /// How many attempts this answer took, including the successful one.
    pub attempts: u32,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    /// Must exceed the gateway's p99, or you convert slow-but-succeeding requests into
    /// failures *and* retries — which add load, which raise p99. `docs/15 §8`.
    /// `docs/14 §7` puts a bot's total attempt budget at 25s, not the SDK's 300s,
    /// because a person in Telegram is not a patient SDK.
    pub request_timeout: Duration,
    pub connect_timeout: Duration,
    /// Size the pool at your in-flight cap, not above it. A cold TCP+TLS handshake is
    /// one to three round trips of pure, avoidable p99.
    pub pool_max_idle_per_host: usize,
    pub max_attempts: u32,
    pub backoff_base: Duration,
    pub backoff_cap: Duration,
    /// What one answer is assumed to cost, in nanodollars.
    ///
    /// Compared against `x-switchboard-budget-remaining-nano` before a request is sent. It
    /// does not have to be exact: it is the difference between stopping one request early
    /// and stopping one request late, and the second of those is a `429` in the middle of
    /// somebody's sentence.
    pub reserve_nano: i64,
    /// Whether to sit out an empty rate-limit bucket instead of firing into it.
    pub pace_on_ratelimit_headers: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:8080".into(),
            api_key: String::new(),
            model: "qwen3:4b".into(),
            request_timeout: Duration::from_secs(25),
            connect_timeout: Duration::from_secs(2),
            pool_max_idle_per_host: 16,
            max_attempts: 3,
            backoff_base: Duration::from_millis(200),
            backoff_cap: Duration::from_secs(8),
            // ~1000 tokens of gpt-4o-mini. Deliberately generous: over-reserving stops one
            // request early, under-reserving buys a refusal.
            reserve_nano: 500_000,
            pace_on_ratelimit_headers: true,
        }
    }
}

/// The last thing the gateway told us about our own headroom.
///
/// `docs/15 §8` tells a client to honour `Retry-After` and to bound its in-flight requests.
/// Both are reactions to a refusal. This is the thing that makes the refusal unnecessary:
/// the gateway publishes its verdict on **every** response, and a client that reads it
/// throttles before the limit rather than after it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Headroom {
    pub rate_limit: Option<RateLimitView>,
    pub budget: Option<BudgetView>,
}

pub struct GatewayClient {
    http: reqwest::Client,
    cfg: Config,
    budget: Arc<RetryBudget>,
    /// Updated from the headers of every response, success or refusal alike. Shared across
    /// every bot in the fleet, because the limits it describes are the *tenant's*, not any
    /// one bot's — a per-bot view would let N bots each believe they held the last slot.
    headroom: Arc<Mutex<Headroom>>,
}

impl GatewayClient {
    pub fn new(cfg: Config) -> anyhow::Result<Self> {
        // Built ONCE. Rebuilding this per request throws away the connection pool and
        // pays a handshake every call — the most common self-inflicted p99 wound, and
        // the default behaviour of naive HTTP clients in most languages.
        let http = reqwest::Client::builder()
            .timeout(cfg.request_timeout)
            .connect_timeout(cfg.connect_timeout)
            .pool_max_idle_per_host(cfg.pool_max_idle_per_host)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(30))
            .build()?;

        Ok(Self {
            http,
            cfg,
            budget: Arc::new(RetryBudget::new(0.1, 100.0)),
            headroom: Arc::new(Mutex::new(Headroom::default())),
        })
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// What the gateway last said about this tenant's limits and balance.
    pub fn headroom(&self) -> Headroom {
        *self.headroom.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Seeds the headroom before the first request.
    ///
    /// A fleet that has just restarted knows nothing about its own balance, so its first
    /// request is the one that discovers the budget is gone. `GET /v1/usage` answers that
    /// question before any user is waiting on it.
    pub fn prime_headroom(&self, h: Headroom) {
        *self.headroom.lock().unwrap_or_else(|e| e.into_inner()) = h;
    }

    fn observe(&self, h: &reqwest::header::HeaderMap) -> (Option<RateLimitView>, Option<BudgetView>) {
        let rate_limit = RateLimitView::from_headers(h);
        let budget = BudgetView::from_headers(h);

        let mut slot = self.headroom.lock().unwrap_or_else(|e| e.into_inner());
        if rate_limit.is_some() {
            slot.rate_limit = rate_limit;
        }
        if budget.is_some() {
            slot.budget = budget;
        }
        (rate_limit, budget)
    }

    /// Shared with [`super::usage`], which reads the gateway's ledger over the same pool.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// One attempt. `remaining` is what is left of the caller's deadline; it becomes
    /// `x-switchboard-deadline-ms` so the gateway can refuse to start work it cannot
    /// finish. `docs/15 §7`: relative milliseconds, because your clock and the
    /// gateway's disagree and an absolute timestamp would encode the skew.
    pub async fn complete_once(
        &self,
        prompt: &str,
        idempotency_key: &str,
        request_id: &str,
        remaining: Duration,
        end_user: Option<&str>,
    ) -> Outcome {
        let body = ChatRequest {
            model: &self.cfg.model,
            // `stream: false`. Streaming forfeits retry, and today forfeits fallback and
            // cache seeding too. Nobody is watching a cursor on Telegram.
            stream: false,
            messages: vec![Message {
                role: "user",
                content: prompt,
            }],
            user: end_user,
        };

        let started = Instant::now();
        let resp = self
            .http
            .post(format!("{}/v1/chat/completions", self.cfg.base_url))
            .bearer_auth(&self.cfg.api_key)
            // Same key for every retry of the same message. At-least-once delivery plus
            // an LLM means paying twice for one answer; the gateway replays instead.
            .header("Idempotency-Key", idempotency_key)
            .header("x-request-id", request_id)
            .header(
                "x-switchboard-deadline-ms",
                remaining.as_millis().to_string(),
            )
            .json(&body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => return Outcome::Transport(e.to_string()),
        };

        let latency = started.elapsed();
        let status = resp.status();
        let retry_after = parse_retry_after(&resp);
        let fallback = header_str(&resp, "x-switchboard-fallback");
        let hedged = header_str(&resp, "x-switchboard-hedged");
        let free = Provenance::from_headers(resp.headers());
        // Recorded from *every* response. A `429` and a `503` carry these too, and the one
        // response that most needs to say "here is your headroom" is the one that refused.
        let (rate_limit, budget) = self.observe(resp.headers());

        match status.as_u16() {
            200 => match resp.json::<ChatResponse>().await {
                Ok(body) => {
                    let text = body
                        .choices
                        .into_iter()
                        .next()
                        .map(|c| c.message.content)
                        .unwrap_or_default();
                    Outcome::Answered(Box::new(Answer {
                        text,
                        served_by: body.model,
                        fallback,
                        hedged,
                        cache_hit: free.cache_hit,
                        replay: free.replay,
                        prompt_tokens: body.usage.prompt_tokens,
                        completion_tokens: body.usage.completion_tokens,
                        rate_limit,
                        budget,
                        latency,
                        attempts: 1,
                    }))
                }
                Err(e) => Outcome::Transport(format!("malformed 200 body: {e}")),
            },
            // Two very different things wear this status code. Read the body, not the code.
            429 => match error_type(resp).await.as_deref() {
                Some("insufficient_quota") => Outcome::QuotaExhausted {
                    message: "monthly budget exhausted".into(),
                },
                _ => Outcome::Throttled { retry_after },
            },
            // The gateway shed us, or no provider will take the call. Either way it is
            // not our request that is wrong, and both carry a `Retry-After`.
            503 => Outcome::Busy { retry_after },
            504 => Outcome::DeadlineExceeded,
            // An idempotency conflict: the same key with a different body. A retry cannot
            // fix it — mint a new key, or stop reusing one.
            409 => Outcome::Fatal(
                "idempotency conflict: this key was used with a different body".into(),
            ),
            _ => Outcome::Fatal(format!("{status}")),
        }
    }

    /// Attempts until the answer, the deadline, the attempt limit, or the retry budget
    /// runs out — whichever comes first.
    ///
    /// `on_shed` is how the caller learns to shrink its concurrency. Nothing else in
    /// this function knows about [`super::aimd`], and it should not.
    pub async fn complete(
        &self,
        prompt: &str,
        idempotency_key: &str,
        request_id: &str,
        deadline: &crate::fleet::Deadline,
        end_user: Option<&str>,
        mut on_shed: impl FnMut(),
    ) -> Outcome {
        self.budget.deposit();

        // The cheapest request is the one you do not send. The previous response already
        // told us the balance; spending a user's patience and a provider's slot to be told
        // it again is the failure this check exists to prevent.
        if let Some(b) = self.headroom().budget
            && !b.can_afford(self.cfg.reserve_nano)
        {
            return Outcome::QuotaExhausted {
                message: format!(
                    "monthly budget has {} nanodollars left, below the {} reserved per answer",
                    b.remaining_nano, self.cfg.reserve_nano
                ),
            };
        }

        let mut last = Outcome::Transport("no attempt made".into());

        for attempt in 0..self.cfg.max_attempts {
            // Never spend a token on work that has expired. `docs/14 §6`, stated once
            // and applied everywhere: check the deadline immediately before the call.
            let Some(remaining) = deadline.remaining() else {
                return Outcome::DeadlineExceeded;
            };

            if attempt > 0 && !self.budget.withdraw() {
                // The budget is empty. Fail now, fast, rather than adding a retry to a
                // system that is already failing.
                return last;
            }

            // An empty bucket refuses whatever we send it. Waiting it out is strictly
            // better than firing, being refused, and waiting anyway — the refusal costs a
            // round trip and a `sb_ratelimit_denied_total` on somebody's dashboard.
            if self.cfg.pace_on_ratelimit_headers
                && let Some(wait) = self.headroom().rate_limit.and_then(|rl| rl.wait_before_next())
            {
                match deadline.remaining() {
                    Some(left) if left > wait => tokio::time::sleep(wait).await,
                    _ => return Outcome::DeadlineExceeded,
                }
            }

            last = self
                .complete_once(prompt, idempotency_key, request_id, remaining, end_user)
                .await;

            let retry_after = match &mut last {
                Outcome::Answered(a) => {
                    a.attempts = attempt + 1;
                    return last;
                }
                // No retry can conjure money, and no backoff shortens a billing month.
                Outcome::QuotaExhausted { .. } | Outcome::Fatal(_) | Outcome::DeadlineExceeded => {
                    return last;
                }
                Outcome::Busy { retry_after } => {
                    on_shed();
                    *retry_after
                }
                Outcome::Throttled { retry_after } => *retry_after,
                Outcome::Transport(_) => None,
            };

            let delay = backoff::next_delay(
                attempt,
                retry_after,
                self.cfg.backoff_base,
                self.cfg.backoff_cap,
            );

            // Sleeping past the deadline is how you pay for an answer nobody wants.
            match deadline.remaining() {
                Some(left) if left > delay => tokio::time::sleep(delay).await,
                _ => return Outcome::DeadlineExceeded,
            }
        }

        last
    }
}

/// Stable across retries of the same message, distinct across messages. Reusing a key
/// with a *different* body is a `409`, so it must not be derived from the prompt alone.
pub fn idempotency_key(conversation_id: &str, message_id: &str) -> String {
    let mut h = Sha256::new();
    h.update(conversation_id.as_bytes());
    h.update([0u8]); // domain separator: ("ab","c") must not collide with ("a","bc")
    h.update(message_id.as_bytes());
    hex::encode(h.finalize())
}

fn header_str(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// How long to wait, in whichever precision the gateway offered.
///
/// `retry-after-ms` first. `Retry-After` is `delta-seconds` per RFC 9110 §10.2.3, so a
/// gateway that wants to say "250 ms" has to round it up to `1` — and a fleet that sleeps
/// a whole second for a quarter-second bucket refill throws away most of its throughput.
/// The millisecond header is the exact figure; the seconds header is the one every proxy
/// and standard client understands. Read the precise one when it is there.
///
/// Never read `retry-after` *as* milliseconds. `250` there means four minutes.
fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    retry_after_from(
        header_str(resp, "retry-after-ms").as_deref(),
        header_str(resp, "retry-after").as_deref(),
    )
}

/// The header arithmetic, with no I/O in it, so it can be tested as arithmetic.
fn retry_after_from(ms: Option<&str>, secs: Option<&str>) -> Option<Duration> {
    if let Some(ms) = ms
        && let Ok(ms) = ms.trim().parse::<u64>()
    {
        return Some(Duration::from_millis(ms));
    }
    secs?.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Reads `error.type` from switchboard's OpenAI-shaped error envelope.
async fn error_type(resp: reqwest::Response) -> Option<String> {
    resp.json::<ErrorEnvelope>()
        .await
        .ok()
        .map(|e| e.error.r#type)
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    r#type: String,
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    stream: bool,
    messages: Vec<Message<'a>>,
    /// Who this answer is for, in the fleet's own naming.
    ///
    /// A tenant is who pays the gateway. It is not who caused the spend. Without this, ten
    /// thousand end users arrive as one undifferentiated invoice, and the bot operator
    /// cannot re-bill their own customers or find the one whose loop ate the month.
    /// `GET /v1/usage/records?user=` reads it back.
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
}

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Deserialize)]
struct ChatResponse {
    model: String,
    choices: Vec<Choice>,
    #[serde(default)]
    usage: WireUsage,
}

#[derive(Deserialize, Default)]
struct WireUsage {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    #[serde(default)]
    content: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_idempotency_key_is_stable_across_retries() {
        let a = idempotency_key("conv-1", "msg-1");
        let b = idempotency_key("conv-1", "msg-1");
        assert_eq!(a, b, "a retry must present the same key or you pay twice");
    }

    #[test]
    fn distinct_messages_get_distinct_keys() {
        assert_ne!(
            idempotency_key("conv-1", "msg-1"),
            idempotency_key("conv-1", "msg-2")
        );
    }

    #[test]
    fn the_domain_separator_prevents_a_boundary_collision() {
        // Without the separator, ("ab","c") and ("a","bc") hash identically — two
        // different messages replaying each other's answer.
        assert_ne!(idempotency_key("ab", "c"), idempotency_key("a", "bc"));
    }

    #[test]
    fn a_chat_response_without_usage_still_decodes() {
        // The gateway omits `cached_tokens` when zero and could omit more. A client that
        // requires the whole object breaks on a field it never reads.
        let body = r#"{"model":"qwen3:4b","choices":[{"message":{"content":"hi"}}]}"#;
        let r: ChatResponse = serde_json::from_str(body).expect("must decode");
        assert_eq!(r.usage.prompt_tokens, 0);
        assert_eq!(r.model, "qwen3:4b");
    }

    #[test]
    fn the_error_envelope_exposes_the_type_that_disambiguates_429() {
        let body = r#"{"error":{"message":"monthly budget exhausted","type":"insufficient_quota","param":null,"code":null}}"#;
        let e: ErrorEnvelope = serde_json::from_str(body).expect("must decode");
        assert_eq!(e.error.r#type, "insufficient_quota");
    }

    /// `Retry-After` is seconds. Reading it as milliseconds turns a 1-second backoff into
    /// a 1-millisecond hot loop; writing milliseconds into it turns 250 ms into 4 minutes.
    /// The gateway sends both headers so neither mistake is necessary.
    #[test]
    fn the_millisecond_header_wins_and_the_seconds_header_is_still_seconds() {
        assert_eq!(
            retry_after_from(Some("250"), Some("1")),
            Some(Duration::from_millis(250)),
            "the precise header is preferred when both are present"
        );
        assert_eq!(
            retry_after_from(None, Some("2")),
            Some(Duration::from_secs(2)),
            "a bare Retry-After is delta-seconds, per RFC 9110"
        );
        assert_eq!(
            retry_after_from(Some("not-a-number"), Some("3")),
            Some(Duration::from_secs(3)),
            "a malformed ms header falls back rather than failing"
        );
        assert_eq!(retry_after_from(None, None), None);
    }

    /// A client pointed at a closed port. Any request it actually sends fails as
    /// `Transport`, so a test that gets a different answer proves no request was sent.
    fn offline_client(cfg: Config) -> GatewayClient {
        GatewayClient::new(Config {
            base_url: "http://127.0.0.1:1".into(),
            connect_timeout: Duration::from_millis(50),
            request_timeout: Duration::from_millis(200),
            ..cfg
        })
        .expect("client builds")
    }

    fn budget(remaining_nano: i64) -> Headroom {
        Headroom {
            rate_limit: None,
            budget: Some(BudgetView {
                limit_nano: 1_000_000,
                spent_nano: 1_000_000 - remaining_nano,
                remaining_nano,
            }),
        }
    }

    /// The cheapest request is the one you never send. The previous response already said
    /// the money was gone; spending a user's patience and a provider's slot to be told
    /// again is the whole point of publishing the balance on every response.
    #[tokio::test]
    async fn an_exhausted_budget_stops_the_request_before_it_is_sent() {
        let client = offline_client(Config {
            reserve_nano: 500_000,
            ..Config::default()
        });
        client.prime_headroom(budget(1_000));

        let deadline = crate::fleet::Deadline::in_(Duration::from_secs(5));
        let outcome = client
            .complete("hi", "k", "r", &deadline, Some("u1"), || {})
            .await;

        match outcome {
            Outcome::QuotaExhausted { message } => assert!(message.contains("1000")),
            other => panic!("a request was sent against an empty budget: {other:?}"),
        }
    }

    /// …and a budget that can still pay does not stop anything. The request goes out, and
    /// fails on the wire, which is how we know the guard let it through.
    #[tokio::test]
    async fn a_solvent_budget_lets_the_request_through() {
        let client = offline_client(Config {
            reserve_nano: 500_000,
            ..Config::default()
        });
        client.prime_headroom(budget(500_000));

        let deadline = crate::fleet::Deadline::in_(Duration::from_secs(5));
        let outcome = client
            .complete("hi", "k", "r", &deadline, None, || {})
            .await;

        assert!(
            matches!(outcome, Outcome::Transport(_)),
            "expected the request to be attempted, got {outcome:?}"
        );
    }

    /// An empty bucket refuses whatever we send it. A client that fires anyway pays a round
    /// trip and a `429` to learn what the last response already told it. Here the wait is
    /// longer than the deadline, so sitting it out means the deadline expires — and that is
    /// the observable proof that nothing was sent.
    #[tokio::test]
    async fn an_empty_rate_limit_bucket_is_waited_out_rather_than_fired_into() {
        let client = offline_client(Config::default());
        client.prime_headroom(Headroom {
            rate_limit: Some(RateLimitView {
                requests: Some(super::super::headers::Bucket {
                    limit: 2,
                    remaining: 0,
                    reset: Duration::from_secs(30),
                }),
                tokens: None,
            }),
            budget: None,
        });

        let deadline = crate::fleet::Deadline::in_(Duration::from_millis(100));
        let outcome = client
            .complete("hi", "k", "r", &deadline, None, || {})
            .await;

        assert!(
            matches!(outcome, Outcome::DeadlineExceeded),
            "expected the client to sit out the bucket, got {outcome:?}"
        );
    }

    /// Pacing is a choice. A fleet that wants the gateway to be the only authority on its
    /// limits can turn it off, and then it fires and takes the `429`.
    #[tokio::test]
    async fn pacing_can_be_disabled() {
        let client = offline_client(Config {
            pace_on_ratelimit_headers: false,
            ..Config::default()
        });
        client.prime_headroom(Headroom {
            rate_limit: Some(RateLimitView {
                requests: Some(super::super::headers::Bucket {
                    limit: 2,
                    remaining: 0,
                    reset: Duration::from_secs(30),
                }),
                tokens: None,
            }),
            budget: None,
        });

        let deadline = crate::fleet::Deadline::in_(Duration::from_millis(500));
        let outcome = client
            .complete("hi", "k", "r", &deadline, None, || {})
            .await;
        assert!(
            matches!(outcome, Outcome::Transport(_)),
            "expected the request to be attempted, got {outcome:?}"
        );
    }
}
