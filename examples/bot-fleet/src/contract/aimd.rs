//! An adaptive in-flight limit — the obligation from `docs/15 §8` that fleets skip.
//!
//! Backoff fixes *when* you retry. It does not fix *how many* requests you have in
//! flight. A fleet that receives `503`, backs off with perfect full jitter, and then
//! returns at the same concurrency is still pointing the same firehose at twelve
//! provider slots. The `503` was information about capacity, and backoff throws it away.
//!
//! So: additive increase on success, multiplicative decrease on shed. The same control
//! law TCP uses, for the same reason — it converges on the bottleneck's real capacity
//! without ever being told what it is.
//!
//! Note what it reacts to. A `503` means *the gateway is saturated*, so back off. A
//! `429` means *this tenant exceeded its own quota*, which says nothing about gateway
//! capacity — reducing concurrency there would be reacting to your own rate limit by
//! shrinking below it, forever. `docs/15 §7` is why the two codes are distinct.

use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// The control law, with no I/O in it, so it can be tested as arithmetic.
#[derive(Debug, Clone)]
pub struct Aimd {
    limit: f64,
    min: usize,
    max: usize,
    /// Multiplicative decrease factor applied on a shed signal.
    beta: f64,
    /// Integer permits currently handed out to the semaphore universe.
    granted: usize,
}

impl Aimd {
    pub fn new(initial: usize, min: usize, max: usize) -> Self {
        assert!(min >= 1 && min <= initial && initial <= max);
        Self {
            limit: initial as f64,
            min,
            max,
            beta: 0.85,
            granted: initial,
        }
    }

    pub fn limit(&self) -> usize {
        (self.limit.floor() as usize).clamp(self.min, self.max)
    }

    /// Additive increase: `+1/limit` per success, i.e. `+1` per limit-many successes.
    /// Probing capacity one slot per round trip, not one per reply.
    ///
    /// Returns how many permits to add.
    pub fn on_success(&mut self) -> usize {
        self.limit = (self.limit + 1.0 / self.limit).min(self.max as f64);
        let want = self.limit();
        if want > self.granted {
            let delta = want - self.granted;
            self.granted = want;
            delta
        } else {
            0
        }
    }

    /// Multiplicative decrease on `503`. Returns how many permits to revoke.
    pub fn on_shed(&mut self) -> usize {
        self.limit = (self.limit * self.beta).max(self.min as f64);
        let want = self.limit();
        if want < self.granted {
            let delta = self.granted - want;
            self.granted = want;
            delta
        } else {
            0
        }
    }

    /// A `429`, a timeout, or a transport error. Capacity told you nothing; do nothing.
    pub fn on_neutral(&mut self) {}
}

/// [`Aimd`] wired to a real semaphore, so the limit actually bounds in-flight requests.
///
/// Growing is easy: `add_permits`. Shrinking is the interesting half — you cannot take a
/// permit back from a task that is already using it. So a reduction is *owed*, and the
/// next permit to be released is retired instead of returned. The limit therefore falls
/// as fast as requests complete, which is as fast as it possibly can.
pub struct AdaptiveLimiter {
    sem: Arc<Semaphore>,
    state: Mutex<Inner>,
    /// How many requests were in flight at once, at the worst moment. A gauge sampled every
    /// five seconds will not see a burst that lasted four hundred milliseconds.
    inflight: std::sync::atomic::AtomicUsize,
    peak: std::sync::atomic::AtomicUsize,
}

struct Inner {
    aimd: Aimd,
    /// Permits we have decided to revoke but have not yet been able to.
    pending_reduce: usize,
}

impl AdaptiveLimiter {
    pub fn new(initial: usize, min: usize, max: usize) -> Arc<Self> {
        Arc::new(Self {
            sem: Arc::new(Semaphore::new(initial)),
            state: Mutex::new(Inner {
                aimd: Aimd::new(initial, min, max),
                pending_reduce: 0,
            }),
            inflight: std::sync::atomic::AtomicUsize::new(0),
            peak: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Blocks until the fleet is allowed one more concurrent request.
    pub async fn acquire(self: &Arc<Self>) -> Permit {
        let permit = Arc::clone(&self.sem)
            .acquire_owned()
            .await
            .expect("limiter semaphore is never closed");
        use std::sync::atomic::Ordering::Relaxed;
        let n = self.inflight.fetch_add(1, Relaxed) + 1;
        self.peak.fetch_max(n, Relaxed);
        Permit {
            limiter: Arc::clone(self),
            permit: Some(permit),
        }
    }

    /// The most concurrent requests this fleet ever had in flight against the gateway.
    pub fn peak_inflight(&self) -> usize {
        self.peak.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn on_success(&self) {
        let mut s = self.lock();
        let add = s.aimd.on_success();
        if add == 0 {
            return;
        }
        // Do not grow while a reduction is still owed — that would hand back exactly the
        // capacity the last `503` told us we did not have.
        let cancelled = add.min(s.pending_reduce);
        s.pending_reduce -= cancelled;
        let net = add - cancelled;
        if net > 0 {
            self.sem.add_permits(net);
        }
    }

    pub fn on_shed(&self) {
        let mut s = self.lock();
        let revoke = s.aimd.on_shed();
        s.pending_reduce += revoke;
    }

    pub fn on_neutral(&self) {
        self.lock().aimd.on_neutral();
    }

    pub fn limit(&self) -> usize {
        self.lock().aimd.limit()
    }

    pub fn available_permits(&self) -> usize {
        self.sem.available_permits()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

pub struct Permit {
    limiter: Arc<AdaptiveLimiter>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.limiter
            .inflight
            .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let mut s = self.limiter.lock();
        if s.pending_reduce > 0 {
            s.pending_reduce -= 1;
            // Retire it instead of returning it. This is the shrink.
            if let Some(p) = self.permit.take() {
                p.forget();
            }
        }
        // Otherwise the permit's own Drop returns it to the semaphore.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrease_is_multiplicative_and_floors_at_min() {
        let mut a = Aimd::new(64, 2, 128);
        for _ in 0..200 {
            a.on_shed();
        }
        assert_eq!(a.limit(), 2, "must never fall below min");
    }

    #[test]
    fn increase_is_additive_and_ceilings_at_max() {
        let mut a = Aimd::new(4, 1, 16);
        for _ in 0..10_000 {
            a.on_success();
        }
        assert_eq!(a.limit(), 16, "must never rise above max");
    }

    #[test]
    fn increase_is_slower_than_decrease() {
        // The asymmetry is the safety property: it gives back capacity fast and takes
        // it back slowly. A symmetric controller oscillates into the shed threshold.
        let mut a = Aimd::new(100, 1, 100);
        a.on_shed();
        let after_one_shed = a.limit();
        assert!(after_one_shed <= 85);

        let mut b = Aimd::new(100, 1, 200);
        b.on_success();
        assert_eq!(
            b.limit(),
            100,
            "one success must not move an integer limit of 100"
        );
    }

    #[test]
    fn granted_permits_track_the_integer_limit() {
        let mut a = Aimd::new(10, 1, 20);
        let revoked = a.on_shed();
        assert_eq!(revoked, 10 - a.limit());

        let mut added = 0;
        for _ in 0..1000 {
            added += a.on_success();
        }
        assert_eq!(a.limit(), a.granted);
        assert!(added > 0);
    }

    #[test]
    fn neutral_signals_do_not_move_the_limit() {
        let mut a = Aimd::new(8, 1, 16);
        let before = a.limit();
        for _ in 0..100 {
            a.on_neutral();
        }
        assert_eq!(
            a.limit(),
            before,
            "a 429 is not evidence about gateway capacity"
        );
    }

    #[tokio::test]
    async fn the_limiter_hands_out_exactly_the_limit() {
        let l = AdaptiveLimiter::new(3, 1, 8);
        let _a = l.acquire().await;
        let _b = l.acquire().await;
        let _c = l.acquire().await;
        assert_eq!(l.available_permits(), 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), l.acquire())
                .await
                .is_err(),
            "the fourth request must wait"
        );
    }

    #[tokio::test]
    async fn a_shed_retires_the_next_released_permit_rather_than_returning_it() {
        let l = AdaptiveLimiter::new(4, 1, 8);
        let a = l.acquire().await;

        l.on_shed(); // 4 * 0.85 = 3.4 -> limit 3, one permit owed
        assert_eq!(l.limit(), 3);

        // The permit is in use, so the reduction cannot happen yet.
        assert_eq!(l.available_permits(), 3);

        drop(a); // released -> retired, not returned
        assert_eq!(
            l.available_permits(),
            3,
            "the released permit must be retired, leaving the semaphore at the new limit"
        );
    }

    #[tokio::test]
    async fn success_does_not_undo_an_owed_reduction() {
        let l = AdaptiveLimiter::new(8, 1, 16);
        let held = l.acquire().await;
        l.on_shed(); // owes a reduction

        // Enough successes that the raw control law would want to grow.
        for _ in 0..200 {
            l.on_success();
        }
        drop(held);

        assert_eq!(
            l.available_permits(),
            l.limit(),
            "the semaphore must settle at the control law's limit, not above it"
        );
    }

    #[test]
    fn converges_downward_under_sustained_shedding() {
        // Sixteen consumers against a gateway that sheds above four concurrent.
        let mut a = Aimd::new(16, 1, 32);
        for _ in 0..50 {
            if a.limit() > 4 {
                a.on_shed();
            } else {
                a.on_success();
            }
        }
        assert!(
            (3..=5).contains(&a.limit()),
            "should settle around the real bottleneck, got {}",
            a.limit()
        );
    }
}
