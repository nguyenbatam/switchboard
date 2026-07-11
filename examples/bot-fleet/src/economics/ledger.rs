//! What the client observed, one row per answer.
//!
//! This is the client's own book. It is kept because the gateway's `/v1/usage` gives
//! *aggregates* — total requests, total tokens, total cost — and aggregates cannot answer
//! "how long did the p99 user wait" or "which model answered". Only the client is standing
//! where the user is standing.
//!
//! Three latencies are recorded, and they are not the same number:
//!
//! * `gateway_ms` — the HTTP call. Switchboard's overhead plus the provider's generation.
//! * `queue_wait_ms` — how long the job sat on the broker before a consumer took it. This
//!   is *your* backlog, not the gateway's, and it is invisible from the gateway's side.
//! * `end_to_end_ms` — enqueue to reply. What the human actually experienced.
//!
//! A report that quotes only the first is quoting the number that makes it look best.

use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Answer {
    /// The `x-request-id` the bot sent. The join key into the gateway's own history, and
    /// the reason a per-request audit is possible at all.
    pub request_id: String,
    pub queue_wait_ms: u64,
    pub gateway_ms: u64,
    pub end_to_end_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// The model asked for.
    pub requested_model: String,
    /// The model that actually answered, from `response.model`.
    pub served_by: String,
    pub cache_hit: bool,
    /// The gateway replayed a stored answer under our `Idempotency-Key`.
    ///
    /// It is **not** a discount — the original attempt was billed, and this row is that
    /// bill. It is evidence the key did its job: a retry that did not buy a second
    /// completion. Priced at full rate, and counted separately so the report can say how
    /// often at-least-once delivery would otherwise have cost twice.
    pub replay: bool,
    pub attempts: u32,
}

/// A request that never became an answer, and therefore must never have been charged.
#[derive(Debug, Clone)]
pub struct Refusal {
    pub request_id: String,
    /// `shed_503`, `throttled_429`, `quota_429`, `deadline`, `failed`.
    pub kind: &'static str,
}

/// Everything that happened, including what did not become an answer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Tallies {
    pub offered: u64,
    pub broker_full: u64,
    pub coalesced: u64,
    pub expired_before_attempted: u64,
    pub shed_by_gateway: u64,
    pub throttled: u64,
    pub quota_exhausted: u64,
    pub failed: u64,
}

#[derive(Default)]
pub struct Ledger {
    answers: Mutex<Vec<Answer>>,
    refusals: Mutex<Vec<Refusal>>,
    tallies: Mutex<Tallies>,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn record(&self, a: Answer) {
        self.lock_answers().push(a);
    }

    /// Records a request that reached the gateway and was refused. The audit asserts the
    /// gateway charged nothing for it.
    pub fn refuse(&self, request_id: &str, kind: &'static str) {
        self.refusals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Refusal {
                request_id: request_id.to_string(),
                kind,
            });
    }

    pub fn refusals(&self) -> Vec<Refusal> {
        self.refusals
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn tally(&self, f: impl FnOnce(&mut Tallies)) {
        f(&mut self.tallies.lock().unwrap_or_else(|e| e.into_inner()));
    }

    pub fn tallies(&self) -> Tallies {
        *self.tallies.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn answers(&self) -> Vec<Answer> {
        self.lock_answers().clone()
    }

    fn lock_answers(&self) -> std::sync::MutexGuard<'_, Vec<Answer>> {
        self.answers.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Nearest-rank percentile over an already-sorted slice.
///
/// `p` is in basis points: 9_900 is p99. Returns 0 for an empty sample, which the report
/// must present as "no data" rather than "zero milliseconds" — those look identical in a
/// table and mean opposite things.
pub fn percentile_sorted(sorted: &[u64], p_bp: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    // ceil(p * n / 10000), clamped to [1, n], then 1-indexed.
    let n = sorted.len() as u64;
    let rank = (p_bp * n).div_ceil(10_000).clamp(1, n);
    sorted[(rank - 1) as usize]
}

pub fn ms(d: Duration) -> u64 {
    d.as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank_and_never_index_out_of_bounds() {
        let v: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile_sorted(&v, 5_000), 50);
        assert_eq!(percentile_sorted(&v, 9_500), 95);
        assert_eq!(percentile_sorted(&v, 9_900), 99);
        assert_eq!(percentile_sorted(&v, 10_000), 100);
    }

    #[test]
    fn a_single_sample_is_every_percentile() {
        assert_eq!(percentile_sorted(&[42], 5_000), 42);
        assert_eq!(percentile_sorted(&[42], 9_900), 42);
    }

    #[test]
    fn an_empty_sample_returns_zero_and_the_caller_must_notice() {
        assert_eq!(percentile_sorted(&[], 9_900), 0);
    }

    #[test]
    fn p99_of_a_hundred_samples_is_the_second_largest_not_the_largest() {
        // The off-by-one that flatters a report: taking `sorted[n * 99 / 100]` on a
        // hundred samples gives index 99 — the max — and calls it p99.
        let mut v: Vec<u64> = vec![1; 99];
        v.push(10_000);
        assert_eq!(
            percentile_sorted(&v, 9_900),
            1,
            "p99 must not be the outlier"
        );
        assert_eq!(percentile_sorted(&v, 10_000), 10_000);
    }

    #[test]
    fn the_ledger_survives_a_poisoned_mutex() {
        let l = std::sync::Arc::new(Ledger::new());
        let l2 = std::sync::Arc::clone(&l);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = l2.answers.lock().unwrap();
            panic!("worker died holding the ledger");
        }));
        l.tally(|t| t.offered += 1);
        assert_eq!(l.tallies().offered, 1);
        assert_eq!(l.answers().len(), 0);
    }
}
