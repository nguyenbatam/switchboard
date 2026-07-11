//! A retry budget: retries may be at most a fixed fraction of live traffic.
//!
//! `docs/08 §1` calls this "the control that matters at 3am; the backoff curve is the
//! one that gets discussed." The gateway keeps one for its calls to providers. The
//! identical argument applies one layer out, and `docs/15 §8` makes it an obligation:
//! without a budget, exponential backoff still lets a large fleet multiply a brownout.
//!
//! The failure it prevents: a provider degrades, every request fails, every request
//! retries twice, and you have tripled the load on a system that was already failing.
//! Backoff spreads those retries over time. It does not reduce their number.
//!
//! When the budget is empty, the error returns immediately. That is the point — a fast
//! failure is a signal; a slow one is an outage.
//!
//! ## Why this counts in integers
//!
//! The obvious implementation stores `tokens: f64` and adds `0.1` per request. Ten
//! requests then leave you with `0.9999999999999999`, and the retry you earned never
//! arrives. Worse, the error is load-dependent: the bucket drifts further from the truth
//! the busier you are, so the control you added for 3am is least accurate at 3am.
//!
//! `docs/00` states the rule for the gateway's money — "bigint nanodollars, never f64,
//! because in floating point that accumulates visible error over a month of traffic". A
//! retry budget is a ledger too. It gets the same treatment: fixed-point micro-tokens,
//! integer arithmetic, exact.

use std::sync::Mutex;

/// One whole retry token, in micro-tokens.
const ONE: u64 = 1_000_000;

/// Token bucket. Every request deposits `ratio` tokens; every retry withdraws one.
#[derive(Debug)]
pub struct RetryBudget {
    inner: Mutex<Bucket>,
}

#[derive(Debug)]
struct Bucket {
    /// Micro-tokens. Integer, so ten deposits of 0.1 are exactly one token.
    tokens: u64,
    deposit: u64,
    max: u64,
}

impl RetryBudget {
    /// `ratio` is the permitted retries per request — 0.1 means "retries may be at most
    /// 10% of live traffic". `max_retries` bounds how much budget an idle period can
    /// bank, so a quiet night cannot fund a retry storm at dawn.
    ///
    /// `ratio` is converted to fixed point exactly once, here, and never touched again.
    pub fn new(ratio: f64, max_retries: f64) -> Self {
        assert!(ratio > 0.0 && ratio <= 1.0, "ratio must be in (0, 1]");
        assert!(max_retries >= 1.0);
        Self {
            inner: Mutex::new(Bucket {
                tokens: 0,
                deposit: (ratio * ONE as f64).round() as u64,
                max: (max_retries * ONE as f64).round() as u64,
            }),
        }
    }

    /// Call once per *original* request, before its first attempt.
    pub fn deposit(&self) {
        let mut b = self.lock();
        b.tokens = b.tokens.saturating_add(b.deposit).min(b.max);
    }

    /// `true` if a retry is affordable. Withdraws a token when it is.
    pub fn withdraw(&self) -> bool {
        let mut b = self.lock();
        if b.tokens >= ONE {
            b.tokens -= ONE;
            true
        } else {
            false
        }
    }

    /// Whole retries currently affordable.
    pub fn available(&self) -> u64 {
        self.lock().tokens / ONE
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Bucket> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cold_budget_funds_no_retries() {
        let b = RetryBudget::new(0.1, 100.0);
        assert!(!b.withdraw(), "the first request must not be able to retry");
    }

    #[test]
    fn ten_requests_fund_one_retry_at_ten_percent() {
        let b = RetryBudget::new(0.1, 100.0);
        for _ in 0..10 {
            b.deposit();
        }
        assert!(b.withdraw());
        assert!(!b.withdraw(), "only one retry was earned");
    }

    #[test]
    fn a_total_brownout_cannot_triple_the_load() {
        // 1000 requests, all failing, each wanting two retries. Without a budget that is
        // 3000 calls. With one, it is 1000 + 100.
        let b = RetryBudget::new(0.1, 1000.0);
        let mut retries = 0;
        for _ in 0..1000 {
            b.deposit();
            for _ in 0..2 {
                if b.withdraw() {
                    retries += 1;
                }
            }
        }
        assert_eq!(
            retries, 100,
            "retries must be capped at 10% of live traffic"
        );
    }

    #[test]
    fn an_idle_night_cannot_bank_a_storm() {
        let b = RetryBudget::new(0.1, 5.0);
        for _ in 0..10_000 {
            b.deposit();
        }
        assert_eq!(b.available(), 5, "banked budget is capped");
    }

    #[test]
    fn the_ledger_does_not_drift() {
        // The f64 version of this bucket fails here: 0.1 summed ten times is
        // 0.9999999999999999, so the retry that was earned is never affordable, and the
        // shortfall compounds with traffic. Integers do not drift.
        let b = RetryBudget::new(0.1, 1_000_000.0);
        for _ in 0..1_000_000 {
            b.deposit();
        }
        assert_eq!(
            b.available(),
            100_000,
            "one million requests at 10% must fund exactly one hundred thousand retries"
        );
    }
}
