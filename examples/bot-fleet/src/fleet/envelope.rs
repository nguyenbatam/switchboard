//! What travels on the broker, and the two clocks that govern it.
//!
//! `docs/14 §6` — "the deadline trap" — is the reason there are two:
//!
//! * `expires_at` is **absolute**, generous, and set at submit. It is an *SLA*. Its only
//!   job is killing work that has gone stale, so the consumer never spends a token on an
//!   answer nobody is waiting for.
//! * The attempt timeouts are **relative**, and start at dequeue. They are a *processing
//!   budget*. They live in [`crate::contract::Config`].
//!
//! Conflating them builds a machine that only ever expires things: a deadline of
//! `submit + 30s` against a backlog that takes ten minutes to drain means every job
//! expires before it is ever attempted.
//!
//! Note which clock is absolute and which is relative, and why they differ. Inside this
//! fleet there is one clock domain, so an absolute `expires_at` is safe and correct.
//! Across the organisational boundary to the gateway it is not — so
//! `x-switchboard-deadline-ms` is *relative*, and the remaining budget is recomputed at
//! the moment of the call. See `docs/15 §7`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// A user message, waiting for an answer that will be delivered out of band.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub job_id: Uuid,
    /// Which bot's credentials the reply must be sent with. The consumer holds these;
    /// no reply-routing table exists, so none can be wrong.
    pub bot_id: String,
    pub conversation_id: String,
    /// Stable per user message. Feeds the `Idempotency-Key`, so a redelivery of this
    /// job replays the original answer instead of buying a second one.
    pub message_id: String,
    pub text: String,
    pub enqueued_at_ms: u64,
    /// Absolute. After this instant the answer is worthless: the user has left.
    pub expires_at_ms: u64,
}

impl Job {
    pub fn new(
        bot_id: impl Into<String>,
        conversation_id: impl Into<String>,
        text: impl Into<String>,
        ttl: Duration,
    ) -> Self {
        let now = now_ms();
        Self {
            job_id: Uuid::now_v7(),
            bot_id: bot_id.into(),
            conversation_id: conversation_id.into(),
            message_id: Uuid::now_v7().to_string(),
            text: text.into(),
            enqueued_at_ms: now,
            expires_at_ms: now + ttl.as_millis() as u64,
        }
    }

    pub fn deadline(&self) -> Deadline {
        Deadline {
            expires_at_ms: self.expires_at_ms,
        }
    }

    /// How long this job sat on the broker before a consumer picked it up. This is the
    /// number that says whether the arithmetic in `docs/14 §2` was right.
    pub fn queue_wait(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.enqueued_at_ms))
    }
}

/// An absolute instant, consulted immediately before any expensive call.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    expires_at_ms: u64,
}

impl Deadline {
    pub fn in_(ttl: Duration) -> Self {
        Self {
            expires_at_ms: now_ms() + ttl.as_millis() as u64,
        }
    }

    /// `None` means expired. The caller must not start work — `docs/14 §6`: "never spend
    /// a token on work that has expired", stated once and applied everywhere.
    pub fn remaining(&self) -> Option<Duration> {
        self.expires_at_ms
            .checked_sub(now_ms())
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
    }

    pub fn expired(&self) -> bool {
        self.remaining().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_deadline_has_budget_left() {
        let d = Deadline::in_(Duration::from_secs(30));
        let left = d.remaining().expect("must not be expired");
        assert!(left > Duration::from_secs(25) && left <= Duration::from_secs(30));
    }

    #[test]
    fn an_elapsed_deadline_reports_expired_not_a_huge_duration() {
        // The bug this guards: `expires_at - now` on unsigned integers underflows into
        // roughly 584 million years of remaining budget, and the job runs forever.
        let d = Deadline {
            expires_at_ms: now_ms() - 1_000,
        };
        assert!(d.expired());
        assert!(d.remaining().is_none());
    }

    #[test]
    fn a_job_carries_its_own_deadline() {
        let j = Job::new("bot-1", "conv-1", "hello", Duration::from_secs(30));
        assert!(!j.deadline().expired());
        assert!(j.expires_at_ms > j.enqueued_at_ms);
    }

    #[test]
    fn message_ids_are_distinct_so_idempotency_keys_are_too() {
        let a = Job::new("b", "c", "x", Duration::from_secs(1));
        let b = Job::new("b", "c", "x", Duration::from_secs(1));
        assert_ne!(a.message_id, b.message_id);
    }
}
