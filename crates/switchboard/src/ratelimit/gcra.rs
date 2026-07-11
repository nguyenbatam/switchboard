use deadpool_redis::Pool;
use std::time::{SystemTime, UNIX_EPOCH};

/// GCRA over a single `tat` (theoretical arrival time) per key.
///
/// `cost` is signed. A negative cost is a refund: it walks `tat` backwards, which
/// is how a reservation of `max_tokens` gets settled down to what the response
/// actually used. Refunds skip the admission check — you cannot be denied for
/// giving capacity back — and clamp at `now`, since capacity cannot be banked
/// beyond a full bucket.
///
/// Four values come back, not two, because a client that can only learn its limit by
/// exceeding it will exceed it. `remaining` and `reset_ms` are what
/// `x-ratelimit-remaining-*` and `x-ratelimit-reset-*` are built from, and GCRA
/// already holds every number needed to compute them — no extra round trip.
const GCRA_LUA: &str = r#"
local tat = tonumber(redis.call('HGET', KEYS[1], 'tat')) or tonumber(ARGV[1])
local now, cost, cap, rate = tonumber(ARGV[1]), tonumber(ARGV[2]),
                             tonumber(ARGV[3]), tonumber(ARGV[4])
local emission = cost / rate
local burst = cap / rate
local new_tat = math.max(tat, now) + emission
if cost > 0 then
    local allow_at = new_tat - burst
    if now < allow_at then
        -- Denied: `tat` is untouched, so report the bucket as it stands.
        local cur = math.max(tat, now)
        local remaining = math.max(0, math.floor((burst - (cur - now)) * rate))
        return { 0, math.ceil(allow_at - now), remaining, math.ceil(cur - now) }
    end
elseif new_tat < now then
    new_tat = now
end
redis.call('HSET', KEYS[1], 'tat', new_tat)
redis.call('PEXPIRE', KEYS[1], math.ceil(burst * 2))
local remaining = math.max(0, math.floor((burst - (new_tat - now)) * rate))
return { 1, 0, remaining, math.max(0, math.ceil(new_tat - now)) }
"#;

/// What the bucket looked like at admission time.
///
/// `reset_ms` is how long until the bucket is full again — milliseconds, because a
/// tenant at 60 rpm refills one slot every 1000 ms and whole-second resolution would
/// round every useful answer to `0` or `1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    pub remaining: u32,
    pub reset_ms: u64,
}

pub enum GcraResult {
    Allowed(Bucket),
    Denied {
        retry_after_ms: Option<u64>,
        bucket: Bucket,
    },
}

pub async fn check(
    pool: &Pool,
    key: &str,
    cost: i64,
    capacity: u32,
    period_ms: u64,
) -> Result<GcraResult, deadpool_redis::PoolError> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let rate = capacity as f64 / period_ms as f64;
    let mut conn = pool.get().await?;

    let reply: Vec<i64> = redis::Script::new(GCRA_LUA)
        .key(key)
        .arg(now_ms)
        .arg(cost)
        .arg(capacity)
        .arg(rate)
        .invoke_async(&mut *conn)
        .await?;

    let bucket = Bucket {
        remaining: reply.get(2).map(|v| (*v).max(0) as u32).unwrap_or(0),
        reset_ms: reply.get(3).map(|v| (*v).max(0) as u64).unwrap_or(0),
    };

    if reply.first() == Some(&1) {
        Ok(GcraResult::Allowed(bucket))
    } else {
        Ok(GcraResult::Denied {
            retry_after_ms: reply.get(1).map(|ms| (*ms).max(0) as u64),
            bucket,
        })
    }
}
