//! One in-flight request per conversation. Coalesce, do not enqueue.
//!
//! `docs/14 §8` calls this "the cheapest win in the entire design and the most commonly
//! skipped". A user who sends three messages while the first is being answered must not
//! produce three completions — they must produce one, because the user is going to read
//! one reply.
//!
//! This is a **correctness** rule, not a capacity rule. It stops you paying three times
//! for one answer. It does nothing to bound your total concurrency across conversations,
//! which is what [`crate::aimd`] is for. `docs/15 §8` insists on both, because a fleet
//! with perfect coalescing and no concurrency cap still saturates the gateway; it just
//! does so with distinct conversations.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

/// Admits at most one in-flight request per `conversation_id`.
#[derive(Default)]
pub struct Coalescer {
    inflight: Mutex<HashSet<String>>,
}

impl Coalescer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// `None` means this conversation is already being answered. Drop the message —
    /// do not queue it. The user's newer message will be picked up by the reply they
    /// are about to receive, and if it will not be, they will send it again.
    pub fn try_begin(self: &Arc<Self>, conversation_id: &str) -> Option<CoalesceGuard> {
        let mut set = self.inflight.lock().expect("coalescer mutex poisoned");
        if set.insert(conversation_id.to_string()) {
            Some(CoalesceGuard {
                owner: Arc::clone(self),
                conversation_id: conversation_id.to_string(),
            })
        } else {
            None
        }
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight
            .lock()
            .expect("coalescer mutex poisoned")
            .len()
    }
}

/// Releases the conversation on drop — including on panic, on early return, and on a
/// cancelled task. That is the only reason this is a guard and not a pair of calls.
pub struct CoalesceGuard {
    owner: Arc<Coalescer>,
    conversation_id: String,
}

impl Drop for CoalesceGuard {
    fn drop(&mut self) {
        if let Ok(mut set) = self.owner.inflight.lock() {
            set.remove(&self.conversation_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_message_in_the_same_conversation_is_refused() {
        let c = Coalescer::new();
        let _first = c.try_begin("conv-1").expect("first must be admitted");
        assert!(c.try_begin("conv-1").is_none(), "second must be refused");
    }

    #[test]
    fn different_conversations_do_not_block_each_other() {
        let c = Coalescer::new();
        let _a = c.try_begin("conv-a").expect("a admitted");
        let _b = c.try_begin("conv-b").expect("b admitted");
        assert_eq!(c.inflight_count(), 2);
    }

    #[test]
    fn dropping_the_guard_readmits_the_conversation() {
        let c = Coalescer::new();
        {
            let _g = c.try_begin("conv-1").unwrap();
        }
        assert_eq!(c.inflight_count(), 0);
        assert!(
            c.try_begin("conv-1").is_some(),
            "must be admitted after release"
        );
    }

    #[test]
    fn a_panicking_worker_still_releases_its_conversation() {
        let c = Coalescer::new();
        let c2 = Arc::clone(&c);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _g = c2.try_begin("conv-1").unwrap();
            panic!("worker died mid-request");
        }));
        // The guard's Drop ran during unwind. Without it, this conversation would be
        // wedged until process restart — the classic in-flight-set leak.
        assert!(c.try_begin("conv-1").is_some());
    }
}
