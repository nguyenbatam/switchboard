//! GCRA against a real Redis.
//!
//! The signed-cost Lua is the one piece of this system that cannot be tested in
//! process: refunds walk `tat` backwards, and a sign error there either hands out
//! unlimited quota or permanently strands it. Skipped when REDIS_URL is unset.

use deadpool_redis::{Config, Runtime};
use switchboard::ratelimit::RateLimiter;
use uuid::Uuid;

fn pool() -> Option<deadpool_redis::Pool> {
    let url = std::env::var("REDIS_URL").ok()?;
    Config::from_url(url)
        .create_pool(Some(Runtime::Tokio1))
        .ok()
}

macro_rules! redis_pool_or_skip {
    () => {
        match pool() {
            Some(p) => p,
            None => {
                // A skip CI cannot see is a test that cannot fail.
                assert!(
                    std::env::var("CI").is_err(),
                    "REDIS_URL is unset in CI: these tests would skip, not pass"
                );
                eprintln!("REDIS_URL unset, skipping");
                return;
            }
        }
    };
}

#[tokio::test]
async fn reservation_is_refunded_so_the_next_request_can_use_the_quota() {
    let limiter = RateLimiter::new(redis_pool_or_skip!(), false);
    let tenant = Uuid::now_v7();
    let tpm = 1_000;

    // Reserve the worst case, then discover the answer was short.
    let (reservation, snapshot) = limiter
        .reserve_tokens(&tenant, tpm, 900)
        .await
        .expect("first reservation fits");
    let snapshot = snapshot.expect("an allowed reservation reports the bucket it left behind");
    assert_eq!(snapshot.limit, tpm as u32);
    assert!(
        snapshot.remaining <= 100,
        "reserving 900 of 1000 leaves at most 100: {}",
        snapshot.remaining
    );

    // Without a refund, only 100 tokens remain and this must fail.
    limiter.settle_tokens(reservation, 100).await;

    limiter
        .reserve_tokens(&tenant, tpm, 800)
        .await
        .expect("the 800 unused tokens were returned to the bucket");
}

#[tokio::test]
async fn an_unsettled_reservation_still_exhausts_the_bucket() {
    let limiter = RateLimiter::new(redis_pool_or_skip!(), false);
    let tenant = Uuid::now_v7();
    let tpm = 1_000;

    limiter
        .reserve_tokens(&tenant, tpm, 900)
        .await
        .expect("first reservation fits");

    assert!(
        limiter.reserve_tokens(&tenant, tpm, 800).await.is_err(),
        "admission must charge the worst case up front, not after the fact"
    );
}

#[tokio::test]
async fn a_refund_cannot_bank_capacity_beyond_a_full_bucket() {
    let limiter = RateLimiter::new(redis_pool_or_skip!(), false);
    let tenant = Uuid::now_v7();
    let tpm = 1_000;

    // Reserve a little, refund all of it, repeatedly. If refunds accumulated past
    // an empty bucket, `tat` would run far into the past and the next oversized
    // request would sail through.
    for _ in 0..5 {
        let (r, _) = limiter
            .reserve_tokens(&tenant, tpm, 100)
            .await
            .expect("small reservation fits");
        limiter.settle_tokens(r, 0).await;
    }

    assert!(
        limiter.reserve_tokens(&tenant, tpm, 1_500).await.is_err(),
        "a full bucket is the ceiling; refunds must not mint quota"
    );
}

#[tokio::test]
async fn settling_at_the_reserved_amount_refunds_nothing() {
    let limiter = RateLimiter::new(redis_pool_or_skip!(), false);
    let tenant = Uuid::now_v7();
    let tpm = 1_000;

    let (r, _) = limiter.reserve_tokens(&tenant, tpm, 900).await.unwrap();
    limiter.settle_tokens(r, 900).await;

    assert!(
        limiter.reserve_tokens(&tenant, tpm, 800).await.is_err(),
        "the tokens were actually used; nothing comes back"
    );
}

#[tokio::test]
async fn settling_above_the_reservation_does_not_underflow_into_a_refund() {
    let limiter = RateLimiter::new(redis_pool_or_skip!(), false);
    let tenant = Uuid::now_v7();
    let tpm = 1_000;

    let (r, _) = limiter.reserve_tokens(&tenant, tpm, 100).await.unwrap();
    // The model generated more than we reserved for. `reserved - actual` is
    // negative; a `u32` subtraction here would wrap to ~4 billion tokens refunded.
    limiter.settle_tokens(r, 5_000).await;

    assert!(
        limiter.reserve_tokens(&tenant, tpm, 1_500).await.is_err(),
        "an over-run must never be refunded as a windfall"
    );
}

#[tokio::test]
async fn a_zero_limit_disables_the_dimension_entirely() {
    let limiter = RateLimiter::new(redis_pool_or_skip!(), false);
    let tenant = Uuid::now_v7();

    for _ in 0..50 {
        limiter
            .reserve_tokens(&tenant, 0, 100_000)
            .await
            .expect("tpm_limit=0 means unlimited, not zero");
    }
}

#[tokio::test]
async fn request_limits_are_independent_of_token_limits() {
    let limiter = RateLimiter::new(redis_pool_or_skip!(), false);
    let tenant = Uuid::now_v7();

    // Exhaust tokens, leaving the reservation unsettled.
    limiter.reserve_tokens(&tenant, 100, 100).await.unwrap();
    assert!(limiter.reserve_tokens(&tenant, 100, 100).await.is_err());

    // The request bucket is a different key and must be untouched.
    limiter
        .check_request(&tenant, 60)
        .await
        .expect("rl:req and rl:tok are separate buckets");
}
