use std::collections::VecDeque;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use crate::providers::ProviderError;
use rand::Rng;
use std::time::Instant;

const BASE_MS: u64 = 200;
const CAP_MS: u64 = 8000;
const MAX_ATTEMPTS: u32 = 2;

pub fn is_retryable(err: &ProviderError) -> bool {
    matches!(
        err,
        ProviderError::RateLimited { .. }
            | ProviderError::Overloaded
            | ProviderError::Upstream5xx { .. }
            | ProviderError::Timeout(crate::providers::TimeoutKind::Connect)
            | ProviderError::Timeout(crate::providers::TimeoutKind::Ttft)
    )
}

/// Retries a provider call, spending from `budget` for every attempt after the first.
///
/// The budget is the control that matters when an upstream degrades. Without it,
/// every caller retrying twice turns a 30%-error incident into three times the
/// load — which is how a degradation becomes an outage. Once it is exhausted the
/// original error is returned unretried.
pub async fn with_retry<F, Fut, T>(budget: &RetryBudget, mut f: F) -> Result<T, ProviderError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ProviderError>>,
{
    budget.record_request();
    let mut attempt = 0u32;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if is_retryable(&e) && attempt < MAX_ATTEMPTS => {
                if !budget.try_acquire() {
                    tracing::warn!(
                        error = %e,
                        "retry budget exhausted, failing fast rather than amplifying load"
                    );
                    return Err(e);
                }
                let delay = backoff(attempt, &e);
                tracing::debug!(attempt, ?delay, "retrying provider call");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Full jitter: the entire capped interval is randomised, not just its upper half.
/// Half-jitter leaves a fixed floor that keeps a synchronised fleet's retries
/// clumped, which is the exact behaviour backoff exists to break up.
fn backoff(attempt: u32, err: &ProviderError) -> Duration {
    if let ProviderError::RateLimited { retry_after } = err
        && let Some(d) = retry_after
    {
        return *d;
    }
    let exp = BASE_MS.saturating_mul(1u64 << attempt.min(4));
    let capped = exp.min(CAP_MS);
    Duration::from_millis(rand::rng().random_range(0..=capped))
}

const BUDGET_WINDOW: Duration = Duration::from_secs(10);

/// Token-bucket retry budget: at most `ratio` of live traffic may be retries.
///
/// One per provider, shared by every in-flight call to it, so it is interior-mutable.
pub struct RetryBudget {
    inner: Mutex<BudgetState>,
    ratio: f64,
}

struct BudgetState {
    tokens: f64,
    capacity: f64,
    last_refill: Instant,
    /// First-attempt calls seen in the current window, used to estimate throughput.
    requests: f64,
    window_started: Instant,
}

impl RetryBudget {
    /// `capacity` is the burst of retries allowed when traffic is idle; `ratio` the
    /// sustained fraction of live traffic that may be spent on retrying.
    pub fn new(capacity: f64, ratio: f64) -> Self {
        Self {
            inner: Mutex::new(BudgetState {
                tokens: capacity,
                capacity,
                last_refill: Instant::now(),
                requests: 0.0,
                window_started: Instant::now(),
            }),
            ratio,
        }
    }

    /// Counts a first-attempt call. The refill rate is derived from this.
    pub fn record_request(&self) {
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if s.window_started.elapsed() >= BUDGET_WINDOW {
            s.requests = 0.0;
            s.window_started = Instant::now();
        }
        s.requests += 1.0;
    }

    pub fn try_acquire(&self) -> bool {
        let mut s = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        let now = Instant::now();
        let elapsed = now.duration_since(s.last_refill).as_secs_f64();
        s.last_refill = now;

        let window_secs = s.window_started.elapsed().as_secs_f64().max(1.0);
        let live_rps = s.requests / window_secs;
        s.tokens = (s.tokens + elapsed * live_rps * self.ratio).min(s.capacity);

        if s.tokens >= 1.0 {
            s.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

impl Default for RetryBudget {
    fn default() -> Self {
        Self::new(10.0, 0.1)
    }
}

/// Per-provider circuit breaker (in-process).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakerState {
    Closed,
    Open,
    HalfOpen,
}

impl BreakerState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Closed => "closed",
            Self::Open => "open",
            Self::HalfOpen => "half_open",
        }
    }
}

const WINDOW: Duration = Duration::from_secs(30);
const MIN_REQUESTS: usize = 20;
const ERROR_THRESHOLD: f64 = 0.5;
const HALF_OPEN_AFTER: Duration = Duration::from_secs(10);

pub struct CircuitBreaker {
    state: BreakerState,
    /// `(observed_at, succeeded)`. Time-bounded, not count-bounded: at 2 rps a
    /// 30-entry window spans 15 seconds, and at 200 rps it spans 150 milliseconds.
    /// Only a duration gives the threshold a stable meaning across traffic levels.
    ///
    /// A `VecDeque` rather than a `Vec`, because this is pruned on every `record()` and
    /// `record()` is on the request path, under a global mutex. The window is already
    /// time-ordered — entries are pushed with a monotonic `Instant` — so the expired ones
    /// are always a prefix, and dropping them is `pop_front` until the front is fresh.
    /// `Vec::retain` rescanned all of it instead: a 30-second window at 200 rps holds
    /// ~6000 entries, so that was an O(n) scan per request to remove, usually, nothing.
    window: VecDeque<(Instant, bool)>,
    opened_at: Option<Instant>,
}

impl CircuitBreaker {
    pub fn new() -> Self {
        Self {
            state: BreakerState::Closed,
            window: VecDeque::new(),
            opened_at: None,
        }
    }

    pub fn state(&self) -> BreakerState {
        self.state
    }

    /// O(number of entries that actually expired), not O(window).
    fn prune(&mut self, now: Instant) {
        while let Some(&(at, _)) = self.window.front() {
            if now.duration_since(at) < WINDOW {
                break;
            }
            self.window.pop_front();
        }
    }

    pub fn record(&mut self, success: bool) {
        let now = Instant::now();

        if self.state == BreakerState::Open {
            if self
                .opened_at
                .is_some_and(|t| t.elapsed() >= HALF_OPEN_AFTER)
            {
                self.state = BreakerState::HalfOpen;
            } else {
                return;
            }
        }

        if self.state == BreakerState::HalfOpen {
            if success {
                self.state = BreakerState::Closed;
                self.window.clear();
            } else {
                self.state = BreakerState::Open;
                self.opened_at = Some(now);
            }
            return;
        }

        self.window.push_back((now, success));
        self.prune(now);

        if self.window.len() >= MIN_REQUESTS {
            let errors = self.window.iter().filter(|(_, ok)| !ok).count() as f64;
            if errors / self.window.len() as f64 > ERROR_THRESHOLD {
                self.state = BreakerState::Open;
                self.opened_at = Some(now);
            }
        }
    }

    pub fn allow(&mut self) -> bool {
        if self.state == BreakerState::Open {
            if self
                .opened_at
                .is_some_and(|t| t.elapsed() >= HALF_OPEN_AFTER)
            {
                self.state = BreakerState::HalfOpen;
                true
            } else {
                false
            }
        } else {
            true
        }
    }
}

impl Default for CircuitBreaker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_can_return_the_whole_interval_including_near_zero() {
        let err = ProviderError::Overloaded;
        let samples: Vec<u64> = (0..200)
            .map(|_| backoff(1, &err).as_millis() as u64)
            .collect();

        let cap = BASE_MS * 2;
        assert!(samples.iter().all(|&d| d <= cap));
        assert!(
            samples.iter().any(|&d| d < cap / 2),
            "full jitter must be able to return less than half the interval"
        );
    }

    #[test]
    fn retry_after_overrides_backoff() {
        let err = ProviderError::RateLimited {
            retry_after: Some(Duration::from_secs(7)),
        };
        assert_eq!(backoff(0, &err), Duration::from_secs(7));
    }

    #[test]
    fn an_idle_budget_allows_its_burst_then_refuses() {
        let budget = RetryBudget::new(3.0, 0.1);
        assert!(budget.try_acquire());
        assert!(budget.try_acquire());
        assert!(budget.try_acquire());
        assert!(
            !budget.try_acquire(),
            "a fourth retry with no traffic to fund it must be refused"
        );
    }

    #[test]
    fn traffic_refills_the_budget_at_the_configured_ratio() {
        let budget = RetryBudget::new(1.0, 0.1);
        assert!(budget.try_acquire());
        assert!(!budget.try_acquire());

        // 1000 requests over the window: 10% of that is plenty to fund one retry.
        for _ in 0..1000 {
            budget.record_request();
        }
        std::thread::sleep(Duration::from_millis(60));
        assert!(budget.try_acquire());
    }

    #[tokio::test]
    async fn with_retry_stops_once_the_budget_is_spent() {
        let budget = RetryBudget::new(1.0, 0.0);
        let mut calls = 0;

        let result: Result<(), _> = with_retry(&budget, || {
            calls += 1;
            async { Err(ProviderError::Overloaded) }
        })
        .await;

        assert!(result.is_err());
        assert_eq!(
            calls, 2,
            "one initial attempt plus the single retry the budget could fund"
        );
    }

    #[tokio::test]
    async fn a_non_retryable_error_is_returned_immediately() {
        let budget = RetryBudget::default();
        let mut calls = 0;

        let result: Result<(), _> = with_retry(&budget, || {
            calls += 1;
            async { Err(ProviderError::Auth) }
        })
        .await;

        assert!(matches!(result, Err(ProviderError::Auth)));
        assert_eq!(calls, 1);
    }

    #[test]
    fn breaker_opens_past_the_error_threshold() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..MIN_REQUESTS {
            cb.record(false);
        }
        assert_eq!(cb.state(), BreakerState::Open);
        assert!(!cb.allow());
    }

    #[test]
    fn breaker_stays_closed_below_the_minimum_sample_size() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..(MIN_REQUESTS - 1) {
            cb.record(false);
        }
        assert_eq!(
            cb.state(),
            BreakerState::Closed,
            "too few samples to conclude anything"
        );
    }

    #[test]
    fn breaker_tolerates_an_error_rate_at_the_threshold() {
        let mut cb = CircuitBreaker::new();
        for _ in 0..15 {
            cb.record(true);
            cb.record(false);
        }
        assert_eq!(cb.state(), BreakerState::Closed, "50% is not >50%");
    }

    /// `prune` drops a *prefix*, and stops at the first live entry.
    ///
    /// That is the whole licence for `pop_front` replacing `Vec::retain`: the window is
    /// pushed in monotonic `Instant` order, so everything expired is at the front and
    /// nothing expired is behind something live. If a future change ever pushed out of
    /// order, `retain` would still be correct and this would silently keep stale entries —
    /// so the ordering is pinned here rather than assumed.
    #[test]
    fn pruning_the_breaker_window_drops_exactly_the_expired_prefix() {
        let now = Instant::now();
        let mut cb = CircuitBreaker::new();
        cb.window = VecDeque::from(vec![
            (now - WINDOW * 2, false),                        // expired
            (now - WINDOW - Duration::from_millis(1), false), // expired, just
            (now - WINDOW + Duration::from_millis(1), true),  // live, just
            (now, true),                                      // live
        ]);

        cb.prune(now);

        assert_eq!(
            cb.window.len(),
            2,
            "both expired entries, and only those, go"
        );
        assert!(
            cb.window
                .iter()
                .all(|(at, ok)| *ok && now.duration_since(*at) < WINDOW),
            "the survivors are the live ones, still in order"
        );
    }

    #[test]
    fn the_window_is_measured_in_time_not_in_requests() {
        let mut cb = CircuitBreaker::new();
        // More failures than a 30-entry window would hold, all inside 30 seconds.
        for _ in 0..100 {
            cb.record(false);
        }
        assert_eq!(cb.state(), BreakerState::Open);

        // Every one of those observations is still within the window.
        let mut fresh = CircuitBreaker::new();
        for _ in 0..40 {
            fresh.record(true);
        }
        for _ in 0..15 {
            fresh.record(false);
        }
        assert_eq!(
            fresh.state(),
            BreakerState::Closed,
            "15 errors against 55 recent requests is under threshold; a 30-entry \
             window would have forgotten the successes and opened"
        );
    }
}
