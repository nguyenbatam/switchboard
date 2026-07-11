# 04 — Redis and rate limiting

← [03 Database](03-database.md) · [index](README.md) · next: [05 Tokenization & cost](05-tokenization-cost.md)

---

## 1. Redis is a cache and a coordination point, never a source of truth

Every key has a TTL. Losing the entire instance degrades latency and loosens rate limits; it loses no
money and no data.

`/ready` reports Redis health, but the gateway still serves chat if Redis is down. **Fail-open on
rate limiting** is the correct trade for a gateway whose upstream is metered anyway — the budget check
in Postgres still bounds the damage. Fail-*closed* is a config flag for anyone who disagrees, and
some will.

| key | type | TTL | purpose |
|---|---|---|---|
| `rl:req:{tenant}` | hash `{tat}` | 2× burst period | GCRA bucket, requests/min |
| `rl:tok:{tenant}` | hash `{tat}` | 2× burst period | GCRA bucket, tokens/min |
| `rl:ip:{ip}` | hash | 60s | pre-auth flood protection |
| `cache:chat:{sha256}` | string (json) | configurable, 1h | exact-match response cache |
| `idem:{tenant}:{key}` | string | 24h | `Idempotency-Key` replay |
| `cb:{provider}` | hash | 60s | circuit-breaker state, published across replicas |
| `budget:{tenant}:{yyyymm}` | string (int) | 40d | fast-path spend counter; Postgres is the truth |

---

## 2. Why a fixed window is wrong

A 60 req/min fixed window resets on the minute boundary. A client sends 60 requests at 11:59:59.9 and
60 more at 12:00:00.1 — **120 requests in 200ms**, every one of them "within limit".

The burst that the limit exists to prevent is not merely possible, it is what a retrying client
*naturally produces*, because everyone's retry backoff aligns to the same wall clock. The failure
mode is not an adversary; it is Tuesday.

- A sliding-window **log** fixes it exactly and costs O(requests) memory per tenant.
- A sliding-window **counter** approximates it, and is wrong in a smaller way.
- **GCRA** — the leaky-bucket-as-meter formulation — fixes it exactly, in O(1) memory and one round
  trip, and additionally yields a precise `Retry-After`.

That last property is not a footnote. `Retry-After` is the difference between a client that backs off
correctly and a client that hammers, and a rate limiter that induces hammering has failed at its job
regardless of how correct its accounting was.

---

## 3. The GCRA script

One Lua script, `EVALSHA`'d, atomic by virtue of Redis's single-threaded execution:

```lua
-- KEYS[1] = bucket   ARGV = { now_ms, cost, capacity, refill_per_ms }
local tat = tonumber(redis.call('HGET', KEYS[1], 'tat')) or tonumber(ARGV[1])
local now, cost, cap, rate = tonumber(ARGV[1]), tonumber(ARGV[2]),
                             tonumber(ARGV[3]), tonumber(ARGV[4])
local emission = cost / rate                 -- ms this request "occupies"
local burst    = cap  / rate                 -- how far ahead of now the TAT may run
local new_tat  = math.max(tat, now) + emission
local allow_at = new_tat - burst
if now < allow_at then
    return { 0, math.ceil(allow_at - now) }  -- denied, retry after N ms
end
redis.call('HSET', KEYS[1], 'tat', new_tat)
redis.call('PEXPIRE', KEYS[1], math.ceil(burst) + 1000)
return { 1, 0 }
```

`tat` is the theoretical arrival time — the instant at which the bucket would next be empty. Storing
one number instead of a list of timestamps is the whole trick.

---

## 4. Reserve and settle: the token bucket is called twice

The two buckets are not consulted at the same point in the pipeline, and `rl:tok` is consulted twice.
The second `rl:tok` call is the interesting one.

1. **`rl:req`, cost 1, immediately after auth.** It needs nothing but the tenant, so it runs before
   the cache lookup and gates it.

2. **`rl:tok`, reserve, after the token count.** `cost = estimated_prompt_tokens + max_tokens`. The
   estimate is deliberately pessimistic: it reserves the worst case, because the actual completion
   length is unknowable here.

   It cannot run any earlier, because the estimate does not exist any earlier — and it must not,
   because a request served from the cache generates no upstream tokens and so reserves none. A
   cache hit skips this step entirely.

3. **`rl:tok`, settle, after the response.** Refunded the difference between the reservation and the
   actual completion tokens, by admitting a negative-cost call. It rides the same `Drop` impl as the
   usage record ([01 §5](01-backend.md#5-cancellation-and-backpressure)), so a client that
   disconnects mid-stream gets its unused reservation back exactly like one that did not.

Without the refund, a tenant whose `max_tokens` is 4096 but who typically generates 100 tokens is
limited at roughly 4% of their real quota, and the bug reports read "your rate limiting is broken"
because it is.

This is the same reserve/settle pattern as a card pre-authorisation, and for the same reason: the
final amount is not knowable at authorisation time.

The refund is why the GCRA script above takes `cost` as a signed value. A negative `cost` yields a
negative `emission`, which walks `tat` backwards; `math.max(tat, now)` then keeps it from being
walked back past the present, which is the correct floor — a bucket cannot be more empty than empty.

---

## 5. What is deliberately *not* in Redis

- **Circuit breaker state on the hot path.** The breaker reads from process memory. Redis is written
  to, for observability and to let a new replica start warm. A breaker that requires a Redis round
  trip has added a dependency to the failure path it exists to protect. See
  [08 §2](08-reliability.md#2-circuit-breaker).
- **The semantic cache.** It needs ANN; it lives in pgvector. See
  [03 §1](03-database.md#semantic-cache).
- **Money.** `budget:{tenant}:{month}` is a *fast path*, rebuilt from `usage_hourly` when missing.
  Postgres is the ledger. A cache that can be wrong must never be the thing that decides what a
  customer owes.
