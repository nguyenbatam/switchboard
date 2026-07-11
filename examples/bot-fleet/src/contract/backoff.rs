//! Full jitter, and the reason half jitter is wrong.
//!
//! `docs/14 §8` and `docs/15 §8` both require full jitter. The distinction is not
//! pedantry. Half jitter computes `ceiling/2 + rand(0, ceiling/2)`, which leaves a
//! **fixed floor**: no client ever returns before `ceiling/2`. Twenty thousand bots
//! that received the same `503` at the same instant therefore all return inside the
//! same half-window, which is the clumping that backoff exists to destroy.
//!
//! Full jitter draws uniformly from `[0, ceiling]`. There is no floor, so the fleet
//! spreads across the whole window.
//!
//! The gateway's own `backoff()` in `crates/switchboard/src/reliability/mod.rs` is the
//! reference implementation. This is a re-implementation, not a re-use: a third party
//! does not link the gateway's internals.

use std::time::Duration;

use rand::Rng;

/// Uniform in `[0, min(base * 2^attempt, cap)]`. `attempt` is 0-based.
pub fn full_jitter(attempt: u32, base: Duration, cap: Duration) -> Duration {
    let ceiling = base
        .saturating_mul(2u32.saturating_pow(attempt.min(31)))
        .min(cap);

    let ms = ceiling.as_millis() as u64;
    if ms == 0 {
        return Duration::ZERO;
    }
    Duration::from_millis(rand::rng().random_range(0..=ms))
}

/// `Retry-After` always wins over the computed backoff.
///
/// The gateway can compute this header exactly — its GCRA limiter knows the instant
/// the bucket next admits a request (`docs/04 §2`). Guessing when the server has told
/// you is how a fleet re-synchronises itself.
pub fn next_delay(
    attempt: u32,
    retry_after: Option<Duration>,
    base: Duration,
    cap: Duration,
) -> Duration {
    match retry_after {
        // Still jitter *within* the honoured delay, or the fleet returns in lockstep at
        // exactly `Retry-After` — obeying the header and stampeding anyway.
        Some(ra) => ra + full_jitter(0, base, cap.saturating_sub(ra).min(base)),
        None => full_jitter(attempt, base, cap),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: Duration = Duration::from_millis(200);
    const CAP: Duration = Duration::from_secs(8);

    #[test]
    fn never_exceeds_the_cap() {
        for attempt in 0..40 {
            for _ in 0..200 {
                assert!(full_jitter(attempt, BASE, CAP) <= CAP);
            }
        }
    }

    #[test]
    fn does_not_overflow_on_absurd_attempt_counts() {
        assert!(full_jitter(u32::MAX, BASE, CAP) <= CAP);
    }

    #[test]
    fn has_no_floor_which_is_the_whole_point() {
        // Half jitter would make this impossible: with a ceiling of 800ms at attempt 2,
        // it could never return below 400ms. Full jitter must reach the bottom of the
        // window, or a synchronised fleet stays synchronised.
        let floor = Duration::from_millis(50);
        let reached_bottom = (0..500).any(|_| full_jitter(2, BASE, CAP) < floor);
        assert!(reached_bottom, "full jitter must be able to draw near zero");
    }

    #[test]
    fn spreads_across_the_window() {
        let ceiling = Duration::from_millis(800); // base * 2^2
        let draws: Vec<_> = (0..500).map(|_| full_jitter(2, BASE, CAP)).collect();
        let below_half = draws.iter().filter(|d| **d < ceiling / 2).count();
        // Uniform over [0, 800ms] puts ~half the mass below 400ms. Generous bounds:
        // this asserts a distribution, not a constant.
        assert!(
            (100..400).contains(&below_half),
            "expected roughly half the draws below the midpoint, got {below_half}/500"
        );
    }

    #[test]
    fn retry_after_is_honoured_and_still_jittered() {
        let ra = Duration::from_secs(2);
        for _ in 0..200 {
            let d = next_delay(0, Some(ra), BASE, CAP);
            assert!(d >= ra, "must never return before Retry-After");
            assert!(d <= ra + BASE, "jitter on top of Retry-After stays bounded");
        }
    }
}
