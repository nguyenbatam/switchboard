# 13 — Stability and performance plan

← [12 Build order](12-build-order.md) · [index](README.md) · next: [14 Workloads & clients](14-workloads-and-clients.md) · [15 Latency & p99](15-latency-and-p99.md)

The first twelve documents describe the end state. This one described the gap between that end state
and what was in `src/`, and the order in which to close it.

**P0 through P5 are done.** What follows is kept as the record of *why* each change was made, because the
reasoning outlives the diff. Read the past tense as "this was true, and here is what it cost".

[P5](#p5--measure-then-claim--done) — the load test that turns every number in
[15 §1](15-latency-and-p99.md#the-slo) from a target into a measurement — has now run. Its results are at
the bottom of this document, and two of them contradict things written above: the load-shed layer P0
installs is not what refuses a request under real load, and [1.6](#16-fairness--whose-p99-is-it)'s
completion criterion could not have failed. Both are corrected in place.

The ordering principle is **risk retired per hour spent**, not interest. P0 is what can take the
cluster down today. P4 is what only hurts once you are large.

Each item keeps the roadmap's invariant: it ships **something demoable, a test, and a README entry**,
or it is not done.

---

## The one-paragraph summary

Under sustained overload switchboard did not shed load — it queued, without bound and without a
deadline, and health checks queued with it. Worse, every request paid an uncached argon2 verify inline
on a runtime worker, so the gateway's real concurrency limit was the core count, not the configured 256.
That was the whole of P0 and P1. Separately, five behaviours in `src/` had drifted from what these
documents specify: streaming had no fallback, the usage sink was not a sink, the circuit breaker
published state nobody read, cancellation was never signalled, and the request-duration histogram was
never recorded at all. Per [the index's own rule](README.md#the-rule-when-a-document-and-the-code-disagree),
the documents are the specification and the code was the bug. That was P2.

One consequence was worse than any of its causes, and it is the reason P0 was urgent. Because no code path
produced a `503`, an open circuit breaker surfaced as `ProviderError::Overloaded → ApiError::Server → 500`.
[`examples/bot-fleet`](16-example-bot-fleet.md) — this repository's own reference client, written against
[15 §7](15-latency-and-p99.md#7-the-wire-contract) — classifies `500` as `Outcome::Fatal` and abandons the
request, and its AIMD controller reduces in-flight concurrency **only** on a `503`. So a provider outage
became a permanent, silent, fleet-wide failure that backoff could not recover from, in a fleet that had
implemented every obligation the documents place on it. The client was right. The gateway was wrong.

---

## P0 — Stop the cascade (half a day) — **done**

The only phase that was urgent.

`/health`, `/ready` and `/metrics` are `.merge`d **before** the `.layer(...)` call in
[`http/mod.rs`](../crates/switchboard/src/http/mod.rs), so they sit inside the same `ConcurrencyLimitLayer` as `/v1`.
When the gateway saturates, the liveness probe queues too, Kubernetes kills the pod, its traffic
lands on the survivors, and they saturate in turn.

Compounding it: `tower`'s `ConcurrencyLimit` returns `Poll::Pending` when the semaphore is empty. It
does not reject. Request 257 parks indefinitely, memory grows, clients time out and retry, and the
gateway never emits a single `503`. It does not fail — it slows until it dies.

| # | change | where |
|---|---|---|
| 0.1 | Move `ops` routes outside every limiter | [`http/mod.rs`](../crates/switchboard/src/http/mod.rs) |
| 0.2 | `LoadShedLayer` + `TimeoutLayer` + `HandleErrorLayer` on `/v1`; `Overloaded → 503`, `Elapsed → 504`, both with `Retry-After` | [`http/mod.rs`](../crates/switchboard/src/http/mod.rs) |
| 0.3 | `/ready`: ping Postgres instead of `db.is_some()`; cache provider health for 2s; a missing `REDIS_URL` must report **not ready** | [`http/routes/ops.rs`](../crates/switchboard/src/http/routes/ops.rs), [`app.rs`](../crates/switchboard/src/app.rs) |
| 0.4 | Add `sb_inflight_requests`, `sb_requests_shed_total` | [`telemetry/metrics.rs`](../crates/switchboard/src/telemetry/metrics.rs) |
| 0.5 | Get argon2 off the executor: verified-key cache + `spawn_blocking` on the miss | [`auth/mod.rs`](../crates/switchboard/src/auth/mod.rs) |

0.2 needs the `load-shed` feature enabled on `tower` in `Cargo.toml`. The code is already vendored;
only the feature flag is missing.

On 0.3, note the current failure mode precisely: `AppState::redis_ok()` returns `true` when
`redis == None`. A pod that booted without `REDIS_URL` reports ready while serving traffic with
rate limiting failed open, no idempotency, and no budget enforcement.

### 0.5 — the item that makes 0.1 actually work

[`auth/mod.rs`](../crates/switchboard/src/auth/mod.rs) runs `key::verify_secret` inline on the async handler, on **every
request**, with no cache. `Argon2::default()` is Argon2id at `m_cost = 19 MiB`, `t_cost = 2` — tens of
milliseconds of deliberately-expensive CPU, and it is not wrapped in `spawn_blocking`.

The consequence is worth stating slowly, because it inverts the rest of this plan. Tokio runs one
worker thread per core. Argon2 is CPU-bound and never yields. So **eight concurrent requests on an
eight-core box occupy every worker thread**, and nothing else runs — not `/v1`, not the timer that
would fire a timeout, and *not `/health`*. The configured `concurrency_limit` of 256 is a fiction; the
real limit is the core count.

Which means 0.1 alone does not achieve what 0.1 exists to achieve. Moving `/health` outside the
limiter gives it permission to run, but there is no thread free to run it on, so Kubernetes kills the
pod anyway. **0.5 is not an optimisation. It is the other half of 0.1.**

It is also an amplification vector. Argon2 runs only *after* the `key_prefix` row is found, but the
prefix is the non-secret half of `sk-sb-<prefix>.<secret>` — anyone who has seen one in a log can force
an unbounded number of argon2 verifies with wrong secrets. The only thing standing in front of it is
`ip_rpm`, which [P4](#p4--correctness-and-security-half-a-day--done-with-one-correction) shows collapses every caller without an
`x-forwarded-for` into a single `"unknown"` bucket.

The fix, in order:

1. **A verified-key cache.** `moka`, bounded, keyed by `sha256(full_token)` — the *whole* token, never
   the prefix alone, or you would accept a wrong secret. Value is the `AuthenticatedKey`. TTL 60s, so a
   revocation takes at most a minute to bite. Steady-state auth cost becomes one hash and one map
   lookup.
2. **A bounded negative cache**, same key, for verify failures. Without it, the amplification above
   survives the positive cache untouched.
3. **`spawn_blocking` on the miss path**, so the one verify that does run cannot pin a worker.

Do not solve this by lowering the argon2 parameters. The parameters are correct; running them on the
request path uncached is what is wrong.

**Done when** 2000 concurrent requests against a 256-limit server produce clean `503`s with
`Retry-After`, `/health` still answers in single-digit milliseconds *while the gateway is saturated*,
and no pod restarts.

---

## P1 — Put the limit where the scarcity is (1–2 days) — **done**

The architectural change, and the most valuable item in this plan.

A gateway spends almost no CPU. It blocks on upstreams. The scarce resource is therefore **provider
capacity, not HTTP request count** — and `ConcurrencyLimitLayer(256)` measures the wrong quantity.
Ollama on one box runs a handful of generations in parallel and queues the rest internally, invisibly,
slowing every request together. OpenAI enforces org RPM/TPM and answers overflow with `429`s that
then count against your own circuit breaker.

Pushing 256 requests at one provider produces 256 people waiting. The alternative — eight served
quickly and the rest given a `503` they can act on — is strictly better for every one of them.

### 1.1 A semaphore per provider

In [`providers/registry.rs`](../crates/switchboard/src/providers/registry.rs), alongside the existing per-provider
`RetryBudget` and `CircuitBreaker`.

Use `try_acquire()`, **never** `acquire().await`. Awaiting the permit rebuilds the unbounded queue one
layer down, which is the bug this phase exists to fix. On failure: try the fallback provider, then
shed.

### 1.2 The permit must span the whole stream

A streaming request holds an upstream slot until its last token, not until its response headers. This
is the easy thing to get wrong.

The codebase already contains the right pattern to copy: `StreamGuard` rides the response body and its
`Drop` fires whenever the stream ends — clean completion, dead socket, cancelled task alike. The permit
travels the same way, either alongside the guard or inside `guard_stream` in
[`providers/timeouts.rs`](../crates/switchboard/src/providers/timeouts.rs).

### 1.3 Deadline propagation

`CallCx` carries a deadline, checked **immediately before** the provider call.

A request whose deadline has expired must not reach the LLM. That is dead work: real tokens, real
money, for an answer nobody is still waiting for. No deadline exists anywhere today outside the agent
loop.

### 1.4 Keep two signals distinct

A full semaphore means the provider is **healthy but busy** — fall back. An open breaker means the
provider is **broken** — do not call it. Today `check_breaker` returns `ProviderError::Overloaded` for
the second case, which invites conflating them.

### 1.5 The other external calls

MCP child processes and the wallet RPC client are external calls with **no concurrency limit at all**.
The SQL tool is already bounded, incidentally, by `connect_tool_ro`'s `max_connections(2)`.

Both *do* have timeouts, so this is a concurrency gap and not a hang risk. Two adjacent bugs, however,
were hang risks and belonged here. Both are fixed, and both now have a test whose name is the bug.

- [`agent/react.rs`](../crates/switchboard/src/agent/react.rs) wrapped the **synchronous**
  `spec.run_blocking(...)` in `tokio::time::timeout(timeout, async { ... })`. The async block reaches no
  `.await` before `run_blocking` returns, so the future was already complete the first time the timer
  polled it, and the timeout **could never fire**. A tool that wedged ignored its deadline entirely and
  hung the agent loop behind it. `run_blocking` now goes to `spawn_blocking`, and the timeout races a real
  `JoinHandle`. The blocking thread itself still cannot be interrupted — nothing in Rust preempts a
  synchronous call — so a wedged tool keeps a blocking-pool thread. What changed is that the request no
  longer waits with it. See `a_wedged_tool_actually_times_out`.
- The three tool bodies — [`sql_query.rs`](../crates/switchboard/src/agent/tools/sql_query.rs),
  [`mcp.rs`](../crates/switchboard/src/agent/tools/mcp.rs),
  [`wallet_balance.rs`](../crates/switchboard/src/agent/tools/wallet_balance.rs) — each re-enter the
  runtime from inside a synchronous call, and each parallel tool call is `set.spawn`ed. A fan-out of tool
  calls under load therefore blocked and re-entered the runtime once per call: the same failure as 0.5,
  arriving by a different door. Moving `run_blocking` onto `spawn_blocking` moves that construct onto a
  blocking-pool thread, where it is legal.

  The three do not re-enter the same way, and the difference decides how loudly they fail if that ever
  stops being true. `sql_query` and `mcp` use `block_in_place` + `Handle::current().block_on(...)`, which
  on a worker thread would be merely expensive; `wallet_balance` uses a bare
  `Handle::current().block_on(...)`, which on a worker thread **panics**. On the blocking pool both are
  fine — `block_in_place` becomes a passthrough, and `block_on` is simply allowed.
  **That is a property of tokio's internals, not of ours, and three tools depend on it**, so
  `a_tool_may_re_enter_the_runtime_from_the_blocking_pool` pins it: a tokio upgrade that changes it fails
  in CI rather than the first time somebody runs an allowlisted query in production.

### 1.6 Fairness — whose p99 is it?

A per-provider semaphore is first-come-first-served. It has no notion of a tenant. One caller opening
two hundred connections takes all twelve slots, and **every other tenant's p99 becomes the queue depth
of the noisiest one**.

[P5](#p5--measure-then-claim--done) lists "one tenant attempting to consume the whole gateway — expect
fairness" as a load-test scenario. Nothing in P1 as written produces fairness, so that test asserts a
property no phase builds. Either the scenario goes or the design does; the design should stay.

Two mechanisms, in order of cost:

1. **Wire the per-tenant rate limits that already exist.** `ratelimit.default_rpm` and `default_tpm` were
   declared and never read (see [P4](#p4--correctness-and-security-half-a-day--done-with-one-correction)). GCRA per tenant is the
   cheap first line: it bounds a tenant's *arrival rate* before the semaphore ever sees it. This is most
   of the win for none of the work. **Built**, and [P5](#p5--measure-then-claim--done) measures it holding
   a victim tenant at a 100% success rate against a hog offering 10× capacity.
2. **Reserve semaphore slots per tenant.** Rate limits bound arrivals, not concurrency — a tenant
   within its RPM can still hold every slot if its requests are slow. Give each active tenant a floor
   of `⌊C / active_tenants⌋` permits and let the remainder be contended. A tenant at its floor that
   wants more waits behind tenants below theirs. **Not built.** With the hog's GCRA raised out of the way,
   P5 measures the victim's success rate falling below 10%.

Do not reach for a full weighted-fair-queueing scheduler. The floor is enough, and a scheduler that
reorders admission needs a deadline to reorder against — which is [1.3](#13-deadline-propagation).

**Done when** `sb_provider_inflight{provider}` exists and is hard-capped, killing Ollama under load moves
streaming traffic onto the fallback instead of returning `503`, and a tenant issuing 200 concurrent
requests does not move a second tenant's **success rate**.

That last clause used to read *"does not move a second tenant's p99"*, and it was unfalsifiable. A gateway
that refuses rather than queues has no queue for the victim to wait in, so its p99 cannot rise no matter
how thoroughly it is starved — P5 measures the victim's p99 going *down*, from 209 ms to ~205 ms, while its
success rate collapses from 100% to under 10%. The old clause was satisfied by a gateway with no fairness at
all, which is what we have until mechanism 2 lands. **Starvation is a shed rate. It was never a latency.**

---

## P2 — Reconcile the code with these documents (1–2 days) — **done**

Five divergences, each one a place where a document promised behaviour that `src/` did not implement.

### 2.1 `stream()` has no fallback

[`providers/registry.rs`](../crates/switchboard/src/providers/registry.rs) implements `try_fallback_chat` for `chat()`
and nothing for `stream()`.

The key fact: `stream()` returns **before the first byte leaves the server**. Response headers are not
yet committed, so falling back there cannot duplicate an answer — the objection that correctly forbids
*retrying* a stream does not apply to *choosing a different provider* before it starts.

Production streams. So the entire fallback mechanism, as built, essentially never runs.

### 2.2 The usage sink is not a sink

[`docs/01 §5`](01-backend.md#5-cancellation-and-backpressure) specifies a bounded `mpsc` feeding a
background writer that batches inserts — 500 rows or 200ms, whichever comes first — so that the
request path never blocks on Postgres.

[`usage/mod.rs`](../crates/switchboard/src/usage/mod.rs) instead spawns one task and issues one `INSERT` **per request**,
plus a second task for `budget::add_spend`. Two tasks and a database round-trip on every call,
contending for the same ten-connection pool as auth lookups and RAG search.

Fixing this is also an overload fix: it gives the request path its pool back.

It repairs a metric, too. `sb_usage_records_dropped_total` — which the README names as *the one to
page on* — currently increments only when an `INSERT` errors. The queue-full condition it is meant to
measure does not exist, because there is no queue.

### 2.3 The breaker publishes state nobody reads

`publish_breaker_state` spawns a task and issues a Redis `SET` from **both** `check_breaker` and
`record_outcome` — two Redis writes per request, to store a value that almost never changes. Nothing
in the codebase reads `cb:{provider}`.

Publish on state transitions only.

While there: `CircuitBreaker.window` was a `Vec` pruned with `retain()` on every `record()`. A 30-second
window at 200 rps holds ~6000 entries, so that was an O(n) scan under a global mutex on the hot path.
It is now a `VecDeque` drained with `pop_front()` — the window is already time-ordered, so everything
expired is a prefix. `pruning_the_breaker_window_drops_exactly_the_expired_prefix` pins the ordering the
`pop_front` depends on, because a future change that pushed out of order would silently keep stale entries
where `retain` would merely have been slow.

(This one outlived the section it was written in. P2 was marked done with the `Vec` still in place; the
load test's finding 1 sent someone back to read the code, and the code disagreed with this paragraph.)

### 2.4 Cancellation is specified, and never signalled

[`docs/01 §5`](01-backend.md#5-cancellation-and-backpressure) prints the `StreamGuard::drop` body, and
its first line is `self.cancel.cancel()` — *"fan out to reader + tool tasks"*.

`.cancel()` is called nowhere in `src/`. Cancellation works only as a side effect of axum dropping the
handler future, which propagates down the response body but **not** into `set.spawn`ed tool tasks. A
client that disconnects while three tools are running leaves three tools running, and the `select!` arm
at [the `select!` in `agent/react.rs`](../crates/switchboard/src/agent/react.rs) that exists to notice is unreachable.

This was previously filed under P4 as *"wire the token to the disconnect signal or remove it"*. That was
wrong on the index's own rule: 01 §5 is the specification, so the token gets wired. Removing it would be
deciding that a disconnected client should keep paying for tool calls, and nobody decided that.

### 2.5 The request-duration histogram is never recorded

[`docs/10 §3`](10-observability.md#3-metrics) names `sb_request_duration_seconds{route}`.
`telemetry/metrics.rs` described it to Prometheus. A `TimedFuture` in `reliability/mod.rs` would have
recorded it — and had **zero call sites**.

So the histogram was described and never observed. It rendered as an empty panel, which looks like "no
traffic" rather than "no instrumentation". Every p99 alert this plan proposes in
[P5](#p5--measure-then-claim--done), and the one [10 §5](10-observability.md#5-what-gets-an-alert-and-what-does-not)
already proposes, was an alert on a series that did not exist.

**As built:** `TimedFuture` was not wired in — it was deleted. The histogram is recorded by
[`telemetry::inflight_layer`](../crates/switchboard/src/telemetry/mod.rs), the outermost layer on `/v1`,
so it sees the status a client sees, including the `503`s produced beneath it. It carries the `outcome`
label that [15 §4](15-latency-and-p99.md#4-the-p99-metric-trap) requires — otherwise P0's load shedding
would make the histogram *look better* as the gateway got sicker. `outcome` is chosen by `ApiError` itself
([`http/error.rs`](../crates/switchboard/src/http/error.rs)), which is how a breaker-open `503` lands in
`outcome="error"` while a shed `503` lands in `outcome="shed"` — two very different failures that a status
code alone cannot tell apart.

---

## P3 — Performance (2–3 days) — **done**

Not before P0–P2. Optimising an unstable system measures noise.

**Turn the exact cache on.** `cache.enabled = false` today. And
[`http/routes/chat.rs`](../crates/switchboard/src/http/routes/chat.rs) skips the cache for *every* request whenever
`agent_mode` is set, including requests that call no tool. Under overload the cache is the cheapest
lever available.

**Give moka a `weigher`.** `max_capacity(10_000)` counts entries, and a `ChatResponse` can be tens of
kilobytes.

**Narrow the reservation.** `estimated.upper + max_tokens.unwrap_or(4096)` against a `default_tpm` of
60000 admits roughly fourteen concurrent requests from a tenant whose answers actually run 200 tokens.
`settle_tokens` refunds correctly, but only *after* the request completes. Reserving against a tenant's
measured p95 completion length would be tighter.

**Profile the tokenizer — do not assume.** `token_counter.estimate()` runs inline in the handler
([`chat.rs`](../crates/switchboard/src/http/routes/chat.rs)). If encoding a long prompt is meaningfully CPU-bound
it is stalling an executor thread, and `spawn_blocking` is the answer. This has not been measured. It is
the first place to look, not a stated finding. (The *encoders* are correctly built once at boot,
[`app.rs`](../crates/switchboard/src/app.rs) — only the per-request encode is in question.)

**Take two Redis round trips off the tail.** The cache `SET`
([`seed_cache_detached`](../crates/switchboard/src/http/routes/chat.rs)) and the `rl:tok` settle refund
([`settle_detached`](../crates/switchboard/src/http/routes/chat.rs)) are both `await`ed *after* the provider has answered
and *before* the response is written. The client pays for both. Neither result is needed to build the
response: detach them, as the usage write already is. See the budget in
[15 §2](15-latency-and-p99.md#2-where-the-tail-actually-comes-from).

**Tune the provider connection pool.** The `reqwest::Client`s are correctly built once per provider at
boot (built once in [`registry.rs`](../crates/switchboard/src/providers/registry.rs)), but set only `connect_timeout` and
`timeout` ([`providers/timeouts.rs`](../crates/switchboard/src/providers/timeouts.rs)). No `pool_max_idle_per_host`,
no `pool_idle_timeout`, no `tcp_keepalive`, no explicit HTTP/2. A cold TCP+TLS handshake is a pure p99
event, and the defaults decide how often you pay one.

---

## P4 — Correctness and security (half a day) — **done, with one correction**

`client_ip()` in [`http/extract.rs`](../crates/switchboard/src/http/extract.rs) trusted `x-forwarded-for`
unconditionally, so any caller could forge it to evade `ip_rpm` — the very limiter whose comment explains
it exists to protect the argon2 verify behind it. With no XFF present, all callers collapsed into a single
`"unknown"` bucket, which is a limiter that throttles the innocent. `ConnectInfo<SocketAddr>` is now the
source of truth, and XFF is read only when the peer appears in `server.trusted_proxies` — empty by default,
which is the safe reading of *"we do not know what is in front of us"*. `main.rs` must serve with
`into_make_service_with_connect_info`, or there is no socket to trust and every caller shares one bucket
again.

Four config keys were declared and never read: `redis.pool_size`, `agent.tool_timeout_ms`,
`ratelimit.default_rpm`, `ratelimit.default_tpm`. A config that lies is worse than a config that is
missing. Three are now wired. `tool_timeout_ms` is a **ceiling** rather than an override — each tool
carries a deadline chosen for what it does, and one global number is a worse choice than any of them, so
the global number only clamps. `default_rpm` and `default_tpm` apply to any tenant row that leaves a limit
unset, and are the first line of [1.6](#16-fairness--whose-p99-is-it). `redis.pool_size` still runs on
deadpool's default sizing.

**One item in this section was wrong, and is corrected rather than deleted.** It claimed two `.unwrap()`s
on the OpenAI decode path would panic the handler on a malformed upstream `tool_call`. Both live inside
`#[cfg(test)]` in [`openai/mod.rs`](../crates/switchboard/src/providers/openai/mod.rs); the decode path
itself has none. The finding was never verified against the compiled module, only read off a line number,
and a line number is not a proof. The claim it made — that a provider is untrusted input and must be parsed
like it — remains right; the bug it named did not exist.

(`CallCx.cancel` was filed here. It moved to [2.4](#24-cancellation-is-specified-and-never-signalled),
because 01 §5 specifies it and the rule says the specification wins.)

---

## P5 — Measure, then claim — **done**

It exists, it runs, and it is [`crates/switchboard/src/bin/loadtest.rs`](../crates/switchboard/src/bin/loadtest.rs).

```
cargo run --release --bin loadtest -- --scenario all --json target/loadtest.json
```

It is not `k6` and not `oha`, and the reason is the second and third scenarios. Killing a provider
*while requests are in flight* and then asserting the answer was a fallback rather than a `503` is not
load generation; neither is holding two tenants' latencies apart. An external generator can produce the
traffic for both and observe neither. So the load test boots the **real router** — `LoadShedLayer` over
`ConcurrencyLimitLayer` over `TimeoutLayer`, the real auth middleware, the real GCRA, the real batching
usage writer — against a stub upstream whose latency it chooses.

The stub is the other half of the method. The SLO in [15 §1](15-latency-and-p99.md#the-slo) is on
*gateway overhead*, which is a subtraction, and a subtraction needs both terms. A stub that sleeps for a
known 200 ms makes provider time a constant we chose; everything above it is ours. It is also what makes
10× capacity runnable on a laptop, in CI, with no GPU — a real Ollama answers `qwen3:4b` in about two
minutes.

The generator is **open-loop**: requests launch on a schedule fixed in advance, whether or not the
previous one has returned. A closed-loop generator cannot overload a system that sheds — shed responses
come back in a millisecond, the workers cycle faster, and the offered rate silently becomes whatever the
gateway felt like serving. That is coordinated omission, and it is the ordinary way a load test reports
that an overloaded system is healthy.

### What it measured

`--provider-cap 8 --stub-delay-ms 200 --duration-secs 20`, so rated capacity is `8 / 0.2 s` = **40 rps**.
Apple Silicon, Postgres and Redis in Docker. Redis `PING` p50 is 0.13 ms on this box.

| scenario | offered | result |
|---|---|---|
| baseline | 32 rps (0.8×) | 640/640 served, **0 shed**. Overhead p50 **4.0 ms**, p99 **5.9 ms** |
| cache | 32 rps, one prompt | 100% hit rate. Cache-hit total p50 **4.1 ms**, p99 **5.5 ms** |
| burst | 400 rps (10×) | 39.6 rps served, 7208 × `503`. `/health` p99 **0.16 ms**, 1000/1000 answered. RSS flat |
| shed layer | 800 rps (10× of a 16-limit) | 14448 × `503` from the layer, decision p99 **0.33 ms** |
| provider death | 20 rps, killed at t+6s | 256/256 served after the kill, **all by fallback**, zero `500`, zero shed |
| fairness | victim 2 rps, hog 400 rps | with GCRA: victim 100%. Without: victim **<10%**, p99 unchanged |

Every row of [15 §1](15-latency-and-p99.md#the-slo)'s table is met. Four findings are worth more than
the numbers.

**1. Under provider-bound load, the `LoadShedLayer` never fires.** Every one of the burst's 7208 `503`s
came from a full provider semaphore, not from P0's shed layer: peak in-flight stayed under **20** against
a `concurrency_limit` of 256. This is not a defect — it is [1.1](#11-a-semaphore-per-provider) working, and
it means the limit that P0 fixed is not, in practice, the limit that binds. It is the reason the load
test carries a fifth scenario that makes the *gateway* the scarce resource, so that the layer P0 installs
is actually exercised. The two refusals are told apart on the wire by their `Retry-After` —
[15 §7](15-latency-and-p99.md#7-the-wire-contract) says they differ on purpose, and this is what that
purpose is for.

A load-shed `503` costs **0.33 ms** p99 and is decided before auth and before Redis. A provider-busy
`503` costs about **2 ms** p99, because it is decided inside the handler, after the argon2 cache, three
GCRA round trips and the tokenizer. Both are inside the 5 ms the SLO allows for a shed decision. Only one of
them is free.

**2. A model with a fallback has twice the rated capacity of its own provider.** The first draft of this
test pointed at `gpt-4o-mini`, whose `fallback = ["qwen3:4b"]` crosses providers, and dutifully served
79.5 rps against what it called a 40 rps gateway: a full openai semaphore returns `Busy`, and `Busy`
falls back ([1.4](#14-keep-two-signals-distinct)). Correct behaviour, wrong arithmetic. The capacity of a
model is the capacity of its whole fallback chain, and [14 §2](14-workloads-and-clients.md#2-do-the-arithmetic-before-drawing-the-boxes)
already sums providers for exactly this reason. The load test now uses aliases with no fallback wherever
it needs the arithmetic to hold.

**3. The gateway's own overhead is ~4 ms p50, and it is almost entirely serial Redis.** Two independent
measurements agree to within 0.1 ms: the baseline's `total − provider_call` is 4.04 ms, and a **cache
hit** — which never calls a provider at all — takes 4.05 ms end to end. Redis `PING` is 0.13 ms here, so
the six-to-eight serial round trips of [15 §2.3](15-latency-and-p99.md#23-six-to-eight-serial-redis-round-trips-is-the-floor)
are about a millisecond of it, and the rest is connection checkout, `EVALSHA`, and the two loopback HTTP
legs this test itself introduces. §2.3's remedy — pipeline the two independent limiter calls — is worth
roughly one round trip, which is worth doing and is not where the p50 went.

**4. [1.6](#16-fairness--whose-p99-is-it)'s "done when" clause was unfalsifiable, and is now fixed.** It
read *"a tenant issuing 200 concurrent requests does not move a second tenant's p99"*. Measured: a hog at
10× capacity, with its arrival-rate limiter removed, drove the victim's success rate from **100% to under
10%** — 3% to 7% across runs — while the victim's p99 moved from 209 ms to ~205 ms, *downward*. It cannot rise. A gateway that refuses rather
than queues has no queue for the victim to wait in, so starvation shows up as a **shed rate**, never as a
latency. The old clause is satisfied by a gateway with no fairness at all, which is exactly the gateway we
have: mechanism 1 (per-tenant GCRA) is built and holds the victim at 100%; mechanism 2 (per-tenant permit
floors) is **not built**, and scenario 3 is what its absence costs.

This is [15 §4](15-latency-and-p99.md#4-the-p99-metric-trap)'s trap wearing a different hat. There, load
shedding makes p99 *look better* as the gateway gets sicker. Here, it makes p99 *look fine* while one
tenant is being starved by another. Both times, the fix is the same: read p99 beside the shed rate, and
never alone.

### Running it

`run_local_test.sh` runs it, and CI runs it with `--no-latency-slo`: a shared GitHub runner is a noisy
neighbour with an unknown number of other tenants on the box, and a p99 measured there is not a fact
about this gateway. The structural assertions — *did it shed rather than queue*, *did the fallback answer*,
*did `/health` ever fail*, *does `sb_requests_shed_total` equal the number of `503`s we counted* — hold on
any machine and are enforced everywhere. The percentiles are printed either way, because the number is
the point of the run.

### Alerts

Beyond today's: sustained shed rate above zero, `sb_provider_inflight` saturation, p99 latency, breaker
open. All four now exist in [`ops/prometheus/alerts.yml`](../ops/prometheus/alerts.yml).

One caution on that p99 alert, from [15](15-latency-and-p99.md). P0's shed responses return in about a
millisecond into the same histogram, so a saturating gateway shows a *falling* p99 — the alert gets
quieter as the outage gets worse. `sb_request_duration_seconds` is labelled by `outcome`
([2.5](#25-the-request-duration-histogram-is-never-recorded)), the alert reads `outcome="served"` only,
and it sits beside the shed rate. Never on its own.

**One caution about scaling out.** The circuit breaker and the retry budget are per-process. Across N
replicas the effective thresholds multiply — twenty errors to open a breaker becomes 20N errors
cluster-wide. The `cb:{provider}` Redis key already exists for this and has no reader (see 2.3). That
is where shared state would go, when it is needed.

---

## Deliberately not doing

**Kafka on the request path**, and **persisting requests to Postgres to process later.** Neither helps,
for the same reason: a durable queue only pays off when the *consumer* of the result is durable, and an
open HTTP connection is not. Persisting a request moves the queue from memory to disk, adds a write to
the critical path, and leaves an orphaned job whose answer has nowhere to go after a restart. Little's
Law finishes the argument — when arrival rate exceeds service rate for any sustained period, no queue
depth saves you. A queue converts fast failure into slow failure, and slow failure is worse.

Kafka has one legitimate home here: the usage sink, which really is a durable, replayable,
multi-consumer event stream. But the first step is the bounded `mpsc` in 2.2. If usage volume ever
outgrows a single Postgres, that sink becomes a producer and the batch writer becomes a consumer.
Not before.

Requests that genuinely do not need a live connection deserve their own endpoint rather than a
reinterpretation of this one. Document ingestion is the obvious first candidate: `/v1/documents` chunks
and embeds N chunks and can run for minutes. `202` plus a `doc_id` and a poll is the right shape, and
Postgres `SELECT … FOR UPDATE SKIP LOCKED` or Redis Streams is ample. Both are already deployed.

**The semantic cache**, per [08 §4](08-reliability.md#4-caching): two weeks of shadow-mode false-hit
data before it may serve traffic. That rule is the point of the rule.

**A sequential worker pool.** A semaphore is a worker pool with less machinery, and sequential
processing of IO-bound work is concurrency 1.

---

## Cost

```
P0 ▓▓                done          ← cascade failure (0.5 was half of it)
P1 ▓▓▓▓▓             done          ← the architectural fix; fairness only half of it (1.6)
P2 ▓▓▓▓              done          ← code vs. these documents
P3 ▓▓▓▓▓▓            done          ← only after the above
P4 ▓                 done          ← one finding in it was itself wrong; see P4
P5 ▓▓                done          ← and it is what makes the rest a claim
```

**P5 was the phase that decided whether any of the rest worked, and it says they did.** Everything above
changed a mechanism; P5 measured one. The distinction between a target and a measurement is the same
distinction [08 §4](08-reliability.md#4-caching) draws between *"34% hit rate, 0.4% false-hit rate"* and
*"has a semantic cache"*, and it is the one this repository keeps choosing to respect.

So the claim is no longer *"the queue was replaced with a shed"*. It is: at 10× rated capacity the gateway
sheds 90% of arrivals in under 2 ms each, serves the other 10% at its rated 39.6 rps, answers `/health` in
0.16 ms p99 while doing it, and does not grow. Gateway overhead is 4.0 ms p50 and 5.9 ms p99. A provider
killed mid-flight costs zero `500`s. Those are measurements, on the hardware named in P5, taken by a load
test anyone can rerun.

**What P5 found that no mechanism above fixed** is the one thing still open: per-tenant permit floors
([1.6](#16-fairness--whose-p99-is-it) mechanism 2). Without them, one tenant with its rate limit raised
refuses over 90% of another tenant's requests, and no latency metric anywhere in this repository moves.
