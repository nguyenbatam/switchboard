# 15 — Latency, the SLO, and the p99 contract

← [14 Workloads & clients](14-workloads-and-clients.md) · [index](README.md) · next: [16 Example: the bot fleet](16-example-bot-fleet.md)

[14](14-workloads-and-clients.md) says *shed, never queue*, and it says *measure p99*. It never joins
those two sentences. This document joins them, and then says what number the p99 is supposed to be, who
owes it, and what a client has to do to get it.

It is the **normative** document for third parties. 14 explains the shapes; this one is what an
integration is checked against. Where it gives a header name or a status code, that is the wire, not a
suggestion.

---

## 1. What a gateway can and cannot promise

An LLM gateway does not generate tokens. It authenticates, meters, routes, and waits. Almost all of a
client's observed latency is the provider's, and switchboard cannot make `gpt-4o-mini` faster.

So the SLO splits in two, and conflating the halves is what makes latency SLOs for gateways worthless:

- **Gateway overhead** — everything switchboard does that is not waiting on a provider. This is owned,
  bounded, and belongs in an SLO and an alert.
- **Provider latency** — TTFT and decode. This is *measured and published per model*, and it is
  **not promised**. It is not switchboard's to promise.

`sb_gateway_overhead_seconds` = `sb_request_duration_seconds` − provider time. The minuend now exists
and carries an `outcome` label; the subtrahend does not, so the overhead metric is still unbuilt.

### The SLO

Every row is now a **measurement**, taken by
[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done)'s load test, which anyone can
rerun with `cargo run --release --bin loadtest -- --scenario all`.

| what | target | measured | owner |
|---|---:|---:|---|
| gateway overhead, p50 | ≤ 5 ms | **4.0 ms** | switchboard |
| gateway overhead, p99 | ≤ 50 ms | **5.9 ms** | switchboard |
| cache-hit total latency, p99 | ≤ 30 ms | **5.5 ms** | switchboard |
| shed decision (`503` returned), p99 | ≤ 5 ms | **2.0 ms** provider-busy, **0.33 ms** load-shed | switchboard |
| `/health`, p99, **while saturated** | ≤ 10 ms | **0.16 ms** | switchboard |
| shed rate at or below rated capacity | ≤ 0.1 % | **0 %** | switchboard |
| TTFT p99, total p99, per model | published, reviewed monthly | — | the provider |

Rated capacity is the number [14 §2](14-workloads-and-clients.md#2-do-the-arithmetic-before-drawing-the-boxes)
computes: ≈ 5.4 rps for Ollama-at-2 plus OpenAI-at-10. An SLO above that line is a promise about
somebody else's hardware.

Three things the `measured` column does not say, and which matter more than the numbers in it.

**It was measured against a stub upstream, not against OpenAI.** Deliberately: overhead is a subtraction,
and you cannot do a subtraction without knowing both terms. The load test's upstream sleeps for a chosen
200 ms, so provider time is a constant and everything above it is ours. It is measured on one developer's
machine, with Postgres and Redis in Docker. **The numbers are a property of that box**, and the load test
prints them rather than asserting them under CI for exactly that reason.

**Gateway overhead is dominated by serial Redis, not by anything clever.** Two independent measurements
agree: the baseline's `total − provider_call` is 4.04 ms, and a *cache hit* — which never calls a provider
— is 4.05 ms end to end. §2.3 is where that millisecond-and-change of round trips lives, and it is still
the honest answer to "where did the p50 go".

**The shed-decision row has two numbers because there are two sheds**, and until the load test ran nobody
had noticed that the cheap one is not the one that fires. §3 and
[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done) finding 1.

Two rules that follow from the split, and that decide what you may page a human for:

**Alert on overhead, dashboard the provider.** A p99-total alert fires when OpenAI is slow. Nobody on
call can do anything about that at 3am, and after the third time they stop reading the page.

**p99 of what, for whom.** A streaming client's experience is `p99 TTFT` — it sees tokens appear and
stops caring. A `stream: false` bot's experience is `p99 total`. [10 §4](10-observability.md#4-the-one-dashboard-panel-that-matters)
already separates prefill from decode for exactly this reason; the SLO inherits the separation. One
"latency" number serves neither client.

---

## 2. Where the tail actually comes from

Traced through `src/`, non-streaming `POST /v1/chat/completions`, happy path, in execution order.

| # | step | cost | where | on the tail? |
|---|---|---|---|---|
| 1 | `rl:ip` GCRA | 1 Redis RTT | [`extract.rs`](../crates/switchboard/src/http/extract.rs) | yes |
| 2 | api-key lookup | 1 Postgres RTT, cached 60s | [`auth/mod.rs`](../crates/switchboard/src/auth/mod.rs) | only on a cache miss |
| 3 | argon2id verify | one hash + a map lookup, cached 60s; `spawn_blocking` on the miss | [`auth/mod.rs`](../crates/switchboard/src/auth/mod.rs) | no longer |
| 4 | `rl:req` GCRA | 1 Redis RTT | [`chat.rs`](../crates/switchboard/src/http/routes/chat.rs) | yes |
| 5 | budget `GET` | 1 Redis RTT | [`chat.rs`](../crates/switchboard/src/http/routes/chat.rs) | yes |
| 6 | exact-cache `GET` | 1 Redis RTT (after moka miss) | [`exact.rs`](../crates/switchboard/src/cache/exact.rs) | yes |
| 7 | tokenize prompt | inline BPE, O(prompt) | [`chat.rs`](../crates/switchboard/src/http/routes/chat.rs) | yes |
| 8 | `rl:tok` reserve | 1 Redis RTT | [`chat.rs`](../crates/switchboard/src/http/routes/chat.rs) | yes |
| 8b | acquire a provider permit | in-process `try_acquire` | [`registry.rs`](../crates/switchboard/src/providers/registry.rs) | no — refuses, never waits |
| 9 | **provider call** | seconds | — | yes, and not ours |
| 10 | exact-cache `SET` | 1 Redis RTT | [`chat.rs`](../crates/switchboard/src/http/routes/chat.rs) | no — detached |
| 11 | `rl:tok` settle refund | 1 Redis RTT | [`chat.rs`](../crates/switchboard/src/http/routes/chat.rs) | no — detached |

Detached, correctly, and therefore free: the usage write — now a bounded `mpsc` into a batching writer
rather than a task and an `INSERT` per request ([`usage/mod.rs`](../crates/switchboard/src/usage/mod.rs)) —
`budget::add_spend`, the breaker publish, which now fires on state transitions only rather than twice per
request ([`registry.rs`](../crates/switchboard/src/providers/registry.rs)), `touch_last_used`, the cache
`SET`, the quota refund, and the idempotency commit
([`idempotency.rs`](../crates/switchboard/src/http/idempotency.rs)).

Three findings, in order of size. All three are now fixed, and
[P5](13-stability-performance-plan.md#p5--measure-then-claim--done) has measured what replaced them. They
are kept because the reasoning is what generalises.

### 2.1 Argon2 *was* the p99, and it was also the concurrency limit

`Argon2::default()` is Argon2id at `m_cost = 19 MiB`, `t_cost = 2`
([`key.rs`](../crates/switchboard/src/auth/key.rs)). It is *designed* to cost tens of milliseconds. It used
to run on every request, uncached, and not in `spawn_blocking`.

Argon2 is CPU-bound and never yields. Tokio runs one worker per core. Therefore eight concurrent
requests on an eight-core box occupied every worker thread, and the configured `concurrency_limit` of 256
never came into play — **the real concurrency limit of this gateway was its core count.**

That is why moving the ops routes outside the limiter was necessary but not sufficient: permission to run
is not a thread to run on.

[`auth/mod.rs`](../crates/switchboard/src/auth/mod.rs) now holds a bounded verified-key cache keyed on
`sha256(full_token)` — the **whole** token, never the prefix alone, or a known prefix plus any secret would
be served a cached success — a bounded negative cache, and `spawn_blocking` on the miss. Steady-state auth
is one hash and one map lookup.

The argon2 parameters were not lowered. They defend the hashes at rest. Caching the verification is the
fix; weakening the primitive is a trade of security for latency that nobody asked for and that the cache
makes unnecessary.

### 2.2 Two Redis round trips *were* on the client's clock for no reason

Rows 10 and 11 ran *after* the provider had answered and *before* the response was written. The client
waited for both. Neither result is needed to construct the response — a cache write and a quota refund are
both fire-and-forget by nature, and the usage insert next to them already was. They are detached now.

This is worth roughly two Redis RTTs of p99, which at a p99 RTT of ~2 ms is 4 ms — small next to argon2,
and large next to a 50 ms budget once argon2 is gone. Tails are paid for in the order you find them.

### 2.3 Six to eight serial Redis round trips is the floor

Rows 1, 4, 5, 6, 8, 10, 11 — plus one more if the client sends an `Idempotency-Key`. Each is fast; each
has a tail; and they are serial, so the tails add. Rows 4 and 5 are independent of each other and could
be pipelined. Row 6 is skipped on a moka hit. Row 5 is skipped when the tenant has no budget set.

**Measured**, on the box in §1: Redis `PING` p50 is 0.13 ms, so the serial chain is roughly a millisecond,
and the gateway's whole pre-provider path is 4.0 ms p50. The round trips are the largest single share of
that and are not the whole of it — connection checkout and `EVALSHA` are in there too. Pipelining rows 4
and 5 is worth about one round trip. It is worth doing. It is not where the p50 went, and if this section
had been optimised before it was measured, that is the sentence nobody would have written.

[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done)'s rule applied to this document
too: measure, then claim.

---

## 3. Shedding *is* the p99 mechanism

The link [14](14-workloads-and-clients.md) leaves implicit, stated once:

> A request's latency is queue wait plus service time. A hard concurrency cap that **refuses** rather
> than queues sets queue wait to zero. So `p99_total ≈ p99_provider` — a number set by physics, not by
> load.

Little's Law is usually deployed to prove a queue cannot save you. It has a second, more useful edge.
With a hard cap `C` and service time `S`, the system serves `C/S` requests per second and **no request
ever waits**. Latency stops being a function of arrival rate. It becomes a property of the provider,
which is to say, a number you can put in a table and defend.

Queue, and the opposite happens. `tower`'s `ConcurrencyLimit` returns `Poll::Pending` rather than
rejecting ([13 §P0](13-stability-performance-plan.md#p0--stop-the-cascade-half-a-day--done)), so request 257
parks. Under sustained overload its wait grows without bound, and p99 becomes a measurement of how long
the incident has been running. That is not a latency metric. It is a stopwatch.

This is the whole reason [14 §3](14-workloads-and-clients.md#3-shape-a--interactive) says shed. Not
because refusal is polite. Because **refusal is what makes p99 a constant.**

Two corollaries a client must internalise, and §8 restates as obligations:

- A `503` is not a degradation of your p99. It is the mechanism that produces it. A gateway that never
  sheds under overload has not given you a better p99; it has hidden a worse one inside a queue.
- Therefore **shed rate and p99 must be read together.** Alone, each can be made perfect by ruining the
  other.

---

## 4. The p99 metric trap

Which leads directly to a way of being lied to by your own dashboard.

A shed request returns `503` in about a millisecond. If it lands in the same histogram as served
requests, then as the gateway saturates the shed fraction rises, the fast responses dominate the
buckets, and **p99 falls**. The graph improves. The alert falls silent. The outage continues.

[13 §P0](13-stability-performance-plan.md#p0--stop-the-cascade-half-a-day--done) introduces the shedding.
[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done) and
[10 §5](10-observability.md#5-what-gets-an-alert-and-what-does-not) introduce a p99 alert. Together, and
without an `outcome` label, they build a smoke detector that switches itself off in a fire.

The rules:

1. `sb_request_duration_seconds{route,outcome}`, `outcome ∈ {served, shed, timeout, error}`.
2. p99 alerts read `outcome="served"` only.
3. Every latency panel carries `sb_requests_shed_total` beside it. Not on a different dashboard.
4. `sb_gateway_overhead_seconds` is what the SLO in §1 is measured against, and what pages a human.

Rules 1–3 are built. `sb_request_duration_seconds` is recorded by
[`telemetry::inflight_layer`](../crates/switchboard/src/telemetry/mod.rs), the outermost layer on `/v1`, so
it sees the status the client sees; the `outcome` label is chosen by `ApiError` itself, which is how a
breaker-open `503` (`outcome="error"`) stays out of a shed-rate alert that a gateway-full `503`
(`outcome="shed"`) belongs in. [`ops/prometheus/alerts.yml`](../ops/prometheus/alerts.yml) reads
`outcome="served"` and pairs every latency alert with the shed rate.
[13 §2.5](13-stability-performance-plan.md#25-the-request-duration-histogram-is-never-recorded).

**Rule 4 is not built.** `sb_gateway_overhead_seconds` needs a subtrahend — the provider's own time —
and nothing records it, so the metric the SLO in §1 is *stated* in terms of is the one metric the gateway
does not emit. §1's numbers come from a load test that fabricates the subtrahend by controlling it. That
is a fine way to establish a baseline and no way to run an alert. Timing the provider call and emitting
the difference is the missing piece, and it is small.

**A third trap, found by the load test rather than by reasoning.** §5 says a global semaphore cannot spell
"tenant". It follows that when one tenant starves another, the victim's p99 does not rise — there is no
queue for it to rise in. It *falls*, because the victim's surviving requests are the ones that found a free
permit. Measured: 209 ms → ~205 ms while the victim's success rate went 100% → under 10%. The same rule saves
you: read the shed rate beside it, per tenant, or the dashboard will show you a healthy gateway starving
one of your customers.

---

## 5. Fairness — p99 is a per-tenant number

A global semaphore is first-come-first-served. It cannot spell "tenant".

So one caller with two hundred open connections holds all twelve provider slots, and every other
tenant's p99 becomes the noisy tenant's queue depth. The SLO in §1 is then true on average and false for
everyone, which is the characteristic failure of a multi-tenant system measured globally.

Percentiles must be computed **per tenant and then aggregated**, or the largest tenant simply *is* the
percentile. A tenant sending 90% of traffic sets p99 by itself, and the ten small tenants it is starving
never appear in the number.

The design is [13 §1.6](13-stability-performance-plan.md#16-fairness--whose-p99-is-it): wire the
per-tenant GCRA limits that are already written and never read, then give each active tenant a floor of
`⌊C / active_tenants⌋` provider permits with the remainder contended. Rate limits bound arrivals;
permit floors bound concurrency; a tenant can violate your p99 through either.

---

## 6. Hedged requests

The one technique that attacks p99 directly, rather than by removing queueing. It is absent from these
documents, and this codebase is unusually well set up for it.

**The idea.** Send the request. If no answer by the time the p95 for that model has elapsed, send a
second copy to a *different* provider. Take whichever answers first; cancel the loser. The tail of a
`min()` of two independent draws is dramatically shorter than the tail of one: p99 collapses toward p95,
for roughly 5% more requests. (Dean & Barroso, *The Tail at Scale*.)

**Why it is legal here, precisely.** [08 §1](08-reliability.md#1-retry--and-when-it-is-not-safe)
establishes the boundary already: a second attempt is safe **before the first byte reaches the client**,
and unsafe after. It even encodes it in the type system — `ModelProvider::stream` returns
`Result<Stream, _>`, not a stream of results, so the unsafe case is unexpressible. That is exactly the
boundary a hedge lives inside. It is the same argument [13 §2.1](13-stability-performance-plan.md#21-stream-has-no-fallback)
uses to permit provider fallback on a stream that has not started. A hedge is a fallback that did not
wait for an error.

**Why it is not free.** You pay for two completions and keep one. This is a latency-for-money trade, and
it must be a decision, not a default.

The rules, each with the failure it prevents:

- **Opt-in per model**, alongside `fallback = [...]` in the registry. Same reason
  [08 §3](08-reliability.md#3-fallback--and-the-semantics-question-answered) makes fallback opt-in: a
  caller who is silently served by a weaker model has had a decision taken for them.
- **Budgeted**, exactly like retries. The `RetryBudget` in `reliability/mod.rs` is the reference — cap
  hedges at ~5% of live traffic. Unbudgeted hedging doubles your load at precisely the moment load is
  the problem.
- **Never hedge when the breaker is open or the provider semaphore is full.** A hedge under saturation
  is a self-inflicted DDoS. This is why [13 §1.4](13-stability-performance-plan.md#14-keep-two-signals-distinct)
  insists *busy* and *broken* stay distinguishable: a hedge is legal against neither, and a fallback is
  legal against both.
- **Pre-first-byte only.** Non-streaming always; streaming only before TTFT.
- **Cancel the loser**, or you have bought latency with money *and* capacity. The machinery exists —
  `StreamGuard`'s `Drop` and `guard_stream` in `providers/timeouts.rs` — and
  [13 §2.4](13-stability-performance-plan.md#24-cancellation-is-specified-and-never-signalled) is what
  makes it actually fire.
- **Threshold is measured, not chosen.** The trigger is this model's observed p95. A hedge at p50
  doubles your traffic; a hedge at p99.9 never fires.
- **`x-switchboard-hedged: <provider>` on the response**, for the same reason `x-switchboard-fallback`
  exists: a caller measuring their own latency deserves to know why it was good.

Not before [13 §P0–P2](13-stability-performance-plan.md). Hedging an unstable system doubles the
instability, and hedging against an argon2 bottleneck (§2.1) hedges into the same saturated thread pool.

---

## 7. The wire contract

What is on the wire. `status` says whether the code does it today; where it does not, the divergence is
a bug against this document per the [index's rule](README.md#the-rule-when-a-document-and-the-code-disagree).

### Managing your own keys

| endpoint | does |
|---|---|
| `POST /v1/keys` | mints one. The secret appears **once**, in this response, and never again. |
| `GET /v1/keys` | lists them, revoked ones included. No hashes, no secrets. |
| `POST /v1/keys/{id}/rotate` | mints a replacement; `grace_ms` is how long the old one keeps working. |
| `DELETE /v1/keys/{id}` | revokes. Idempotent: revoking twice is `204`, not `404`. |

All four require the `keys` scope, which the `{chat, embeddings}` default does not grant and the dev
tenant does not hold. The first key is minted by `sbctl`, out of band — bootstrapping an admin API from
inside itself is a chicken-and-egg problem whose usual answer is a default password.

A key may only grant scopes it already carries, or one administrative key would be an escalation
primitive rather than an administrative one. And a key cannot revoke itself: locking yourself out of your
own gateway is `sbctl`'s job, not an API call's.

Revocation bites **immediately** on the replica that processes it, and within 60 seconds everywhere else
— the lifetime of the verified-key cache in [`auth`](../crates/switchboard/src/auth/mod.rs). Making it
instant cluster-wide would cost a Redis round trip on every authenticated request, which is the cost that
cache exists to avoid.

### Request headers a client may send

| header | value | meaning | status |
|---|---|---|---|
| `Authorization` | `Bearer sk-sb-<prefix>.<secret>` | required | implemented |
| `Idempotency-Key` | opaque, ≤ 255 bytes | replay protection, 24h | implemented |
| `x-request-id` | UUID | echoed, joins your logs to ours | implemented |
| `x-switchboard-deadline-ms` | integer ms, relative | do not start work that cannot finish in time | implemented |

**The deadline is relative, not absolute, and that is not a style choice.** An absolute timestamp is
wrong the moment your clock and ours disagree, and they do. `grpc-timeout` is relative for this reason.
Send the time you are still willing to wait, measured from when you sent it; the gateway starts its clock
on receipt and eats the network leg itself.

[14 §8](14-workloads-and-clients.md#8-the-client-contract) currently instructs clients to *"send a
deadline, and expect it to be enforced"* without naming a header. This row is that header.
[13 §1.3](13-stability-performance-plan.md#13-deadline-propagation) is what enforces it.

### Response status codes

| condition | status | `error.type` | `Retry-After` | status |
|---|---|---|---|---|
| tenant over RPM | `429` | `rate_limit_error`, code `requests_rate_limit_exceeded` | yes, from GCRA | implemented |
| tenant over TPM | `429` | `rate_limit_error`, code `tokens_rate_limit_exceeded` | yes, from GCRA | implemented |
| monthly budget exhausted | `429` | `insufficient_quota` | no | implemented |
| gateway saturated, load shed | `503` | `overloaded` | yes | implemented |
| all providers broken (breaker open) | `503` | `provider_unavailable` | yes | implemented |
| provider healthy but at its in-flight cap | `503` | `overloaded` | yes, short | implemented |
| deadline expired while queued or upstream | `504` | `deadline_exceeded` | no | implemented |
| deadline already in the past on arrival | `400` | `invalid_request_error` | no | implemented |
| `Idempotency-Key` reused, **different** body | `409` | `idempotency_conflict` | no | implemented |
| `Idempotency-Key` reused, **identical** body, original still running | — | — | — | gateway waits up to 10s, then replays |
| …and the original is *still* running after that | `503` | `overloaded` | yes, 2s | implemented |
| `Idempotency-Key` reused, identical body, original finished | `200` | — | — | replayed verbatim |

Four of these need a word.

**`429` versus `503` is the most important distinction on this page**, and [14 §8](14-workloads-and-clients.md#8-the-client-contract)
already says why: `429` means *you* went too fast, so slow down; `503` means *the gateway* is saturated,
which is not your fault and which **every other client received at the same instant**. That is precisely
why you must not all come back together. Full jitter, not half.

It is also why `ProviderError::Overloaded` may not map to `500`, which is what it did until the layers in
[`http/mod.rs`](../crates/switchboard/src/http/mod.rs) were built. A `500` says *"this is a bug, do not
retry"*. [`bot-fleet`](../examples/bot-fleet/src/contract/client.rs) reads it as `Outcome::Fatal` and
abandons the request, and its AIMD controller — which reduces in-flight concurrency **only** on a `503` —
never fires. A provider outage therefore became a permanent, silent, fleet-wide failure that no amount of
backoff could recover from. The reference client was written against this document; the gateway was not.

**A `429` names the limiter that refused it**, on `error.code`. `429` alone tells a client to slow down
without saying *what* to slow down, and the two remedies are opposite: a tenant over its RPM must send
fewer requests, and a tenant over its TPM must send smaller prompts. Doing either in place of the other
makes things worse. The `x-ratelimit-*` headers on every response — success and refusal alike — are how a
client avoids finding this out the hard way.

**The idempotency conflict is `409`.** It used to be `400`, which was a real bug and not a documentation
gap: against a `400` a client cannot distinguish *"your JSON is malformed"* (fix the request) from
*"you reused a key"* (mint a new one), and those have opposite remedies. `409` is Stripe's convention and
the IETF idempotency-key draft's.

**`Retry-After` is emitted for every `429` and every `503`**, and it is accompanied by `retry-after-ms`.

The pair is not redundancy. RFC 9110 §10.2.3 defines `Retry-After` as `delta-seconds`, so a gateway that
wants to say *"250 ms"* must round it **up** to `1` — every proxy, CDN and standard client reads a bare
integer there as seconds, and writing `250` would tell a fleet to sleep for four minutes. So the exact
figure travels in `retry-after-ms`, which is the header the OpenAI and Anthropic SDKs prefer when both are
present, and which [`bot-fleet`'s `parse_retry_after`](../examples/bot-fleet/src/contract/client.rs) reads
first. A client that only understands `Retry-After` still backs off correctly, just coarsely.

[04 §2](04-redis-ratelimit.md#2-why-a-fixed-window-is-wrong) exists in large part to be able to compute
this header exactly, and [14 §4](14-workloads-and-clients.md#4-shape-b--fan-in-the-bot-fleet) depends on
it to keep a bot fleet from retrying in lockstep.

**The `503`s carry different waits, on purpose.** A shed request is told to come back in a second; a
request refused because a provider is at its in-flight cap is told half a second, because a permit frees
the moment some other request finishes; a request refused because the circuit breaker is open is told ten
seconds, because that is when the breaker next admits a probe and an earlier retry learns nothing.

### Response headers switchboard sets

| header | meaning | status |
|---|---|---|
| `x-request-id` | echo, or a fresh UUIDv7 | implemented |
| `x-switchboard-fallback` | `requested→served`, per [08 §3](08-reliability.md#3-fallback--and-the-semantics-question-answered) | implemented |
| `x-switchboard-cache` | `hit`. Absent means the request reached a provider. | implemented |
| `x-switchboard-truncation` | context was truncated | implemented |
| `x-switchboard-replay` | `idempotent`. The answer came from an idempotency record; nothing was billed. | implemented |
| `retry-after-ms` | exact backoff in milliseconds, beside the RFC-mandated whole-second `Retry-After` | implemented |
| `x-ratelimit-limit-requests` / `-tokens` | the tenant's RPM / TPM | implemented |
| `x-ratelimit-remaining-requests` / `-tokens` | what is left in the bucket | implemented |
| `x-ratelimit-reset-requests` / `-tokens` | when the bucket refills, e.g. `250ms` | implemented |
| `x-switchboard-upstream` | which vendor produced a `5xx`, per [01](01-backend.md) | implemented |
| `x-switchboard-budget-limit-nano` / `-spend-nano` / `-remaining-nano` | month-to-date balance, on every chat response | implemented |
| `x-switchboard-hedged` | a hedge won, §6 | proposed |

`response.model` always names the model that **actually answered**, not the one requested
([08 §3](08-reliability.md#3-fallback--and-the-semantics-question-answered)). The OpenAI SDK exposes it.
A client measuring quality or latency per model must read it rather than assume.

**`x-switchboard-cache` exists because a cache hit is otherwise undetectable, and that is a billing
problem, not a curiosity.** A hit replays the original response — including its `usage` object, with real
token counts — while the gateway bills it at zero. A client that prices its own spend from tokens will
therefore charge itself for answers that were free, its books will drift from ours, and it will discover
this on an invoice. It also makes the cache hit rate measurable, which
[14 §9](14-workloads-and-clients.md#9-what-to-measure) calls the only lever that adds throughput while
subtracting cost.

### Reading your own bill

| endpoint | answers |
|---|---|
| `GET /v1/usage` | totals, and **the balance**: `monthly_budget_nano`, `month_spend_nano`, `remaining_nano` |
| `GET /v1/usage/records?limit=&before=` | one row per call: which were charged, which were free, and why |
| `GET /v1/usage/records?user=` | …narrowed to one of *your* end users |
| `GET /v1/models` | the rate card: `pricing.prompt_nano_per_token`, `completion_nano_per_token`, `context_window` |

**Attribution below the tenant.** A tenant is who pays us. It is not who caused the spend. Send OpenAI's
`user` field on the request and it lands on the usage row, so a fleet serving ten thousand people can
re-bill its own customers — or find the one whose runaway loop ate the month — without keeping a second
ledger of its own and hoping it agrees with ours. It is capped at 128 characters, never a metric label
(unbounded cardinality), and never part of the cache key: two users asking the same question share one
cached answer, which is the whole economics of the cache.

**The rate card is on `/v1/models`, and the cost card never is.** `pricing` carries what a tenant pays,
as USD-per-million for humans and nanodollars-per-token for arithmetic. What *we* pay upstream is absent
from the wire type by construction — [migration 007](../crates/switchboard/migrations/007_margin.sql) goes
as far as revoking that column from the role a prompt-injected `sql_query` can reach.

**The balance is pushed as well as pulled.** Every chat response carries
`x-switchboard-budget-remaining-nano`, so nobody has to poll a number that is uninteresting 99% of the
time and arrives, the other 1% of the time, as a `429` in the middle of a sentence. It may go **negative**
— the check is a pre-check — so read it as signed. Operators can additionally configure
`billing.budget_webhook_url` to be told once per tenant, per month, per threshold. That URL is the
operator's, never a tenant's: a customer-supplied callback is a server-side request forgery primitive
aimed at the inside of your own cluster.

All money is **nanodollars**, `bigint`, never a float
([00](00-overview.md#money-time-and-ids--three-conventions-set-once)).

The balance is exposed for a blunt reason: a client that cannot read its own limit discovers it by
hitting it, as a `429 insufficient_quota` in the middle of a user's sentence. One that can read it warns,
throttles, or tops up first.

`remaining_nano` may go **negative**, and how far is worth stating precisely, because the answer changed
when the usage sink became a sink.

The budget check is a pre-check against *already-recorded* spend. Recording is no longer synchronous: a
usage row goes onto a bounded queue and a background writer commits it in batches — 500 rows or 200 ms,
whichever comes first ([01 §5](01-backend.md#5-cancellation-and-backpressure)) — so that Postgres is never
on a client's clock. Only after that commit does the Redis spend counter move.

So the overshoot is not "one in-flight request". It is **one batch interval of that tenant's traffic**, plus
its in-flight requests. At the rated capacity of [14 §2](14-workloads-and-clients.md#2-do-the-arithmetic-before-drawing-the-boxes)
that is on the order of a single request; at ten times that, it is a handful. It is bounded, it is small
against any budget worth having, and it buys the request path its database connections back. A tenant that
cannot tolerate it wants a hard prepaid wallet, not a monthly budget, and that is a different product.

The same lag is why `/v1/usage` read immediately after a completion may not show it yet. `bot-fleet`'s
`usage_settled()` polls until the ledger stops moving, which is what any reconciliation must do.

Each history row carries `billing`, which is the field that stops a support ticket:

| `billing` | meaning |
|---|---|
| `charged` | a provider generated tokens, and they were billed |
| `free_cache_hit` | served from cache; no provider was called |
| `free_error` | the request failed; errors are never billed |
| `free_zero_cost` | completed, but priced at zero — a local model, or an empty completion |

There is **no refund**, because there is nothing to reverse: a request that produced no upstream tokens
was never charged. The one case that surprises people: `status: "client_disconnect"` **is** charged for
the tokens generated before the socket died. The provider produced them; somebody pays. Better to read
that here than to infer it from an invoice.

`GET /v1/cost` appears in [01](01-backend.md)'s module tree in earlier drafts and was never built.
Deliberately: the balance fields above and the history below cover it, and a third endpoint whose only
job is to re-add numbers Postgres has already added is a cache with no invalidation story.

---

## 8. What a client must do to get a good p99

[14 §8](14-workloads-and-clients.md#8-the-client-contract) lists the obligations that keep the *gateway*
healthy. These are the ones that keep the *client's own* p99 low. They are not the same list, and the
second is the one people skip.

**Reuse connections.** A cold TCP + TLS handshake is one to three round trips of pure, avoidable p99. If
your HTTP client builds a new connection per request — the default for naive `requests`/`fetch` usage —
you are adding a handshake to every single call. Use a pooled, keep-alive client; the gateway's own
provider clients do exactly this ([`registry.rs:61`](../crates/switchboard/src/providers/registry.rs#L61), built once at
boot). Size the pool at your in-flight cap, not above it.

**Set the client timeout *above* the server's p99.** This is the most common self-inflicted p99 wound.
A timeout below p99 converts a slow-but-succeeding request into a failure *and* a retry — which adds
load, which raises p99, which trips more timeouts. Pick a timeout above p99 and below the point at which
the human leaves. [14 §7](14-workloads-and-clients.md#7-timeouts-in-one-table) has the table; the row for
a bot is 25s, and the row for a patient SDK is 300s, and they are different because the humans are.

**Read the headroom the gateway puts on every response.** `x-ratelimit-remaining-requests`,
`-remaining-tokens`, and `x-switchboard-budget-remaining-nano` are on *every* answer and on every refusal.
Backoff and `Retry-After` are reactions — they happen after a `429`. These are the thing that makes the
`429` unnecessary. When a bucket reads zero, sit out its `x-ratelimit-reset-*` instead of firing into it;
when the balance cannot cover one more answer, stop, because the previous response already told you so and
spending a user's patience and a provider's slot to hear it again buys nothing.
[`bot-fleet`'s `contract/headers.rs`](../examples/bot-fleet/src/contract/headers.rs) is this, as code.

Two traps in those headers, both of which fail silently:

- `x-ratelimit-reset-*` carries its unit — `250ms`. Strip the digits and ignore the `ms` and `4171ms`
  becomes seventy minutes of stall.
- `x-switchboard-budget-remaining-nano` is **signed**. The budget check is a pre-check against spend that
  a background writer commits in batches, so a tenant overshoots by one batch interval of its own traffic
  plus its in-flight requests — see §7. Parse it as unsigned and a small overspend reads as an
  astronomical balance, and the fleet keeps spending.

**Do not confuse `x-switchboard-cache` with `x-switchboard-replay`.** Both replay a stored body, including
its `usage` object with real token counts. They mean opposite things about money. A cache hit means no
provider ran and nothing was billed: count zero. A replay means a provider ran on *this request's first
attempt* and was billed exactly once, on the usage row carrying the same `x-request-id`: count the answer
once, at full price. Price a replay at zero and your books come in under the invoice; price a cache hit at
full rate and they come in over it. Neither error announces itself.

A replay is also a proof: it is the `Idempotency-Key` doing its job. A replay with **no retry behind it**
is a key collision, and the user just read an answer generated for somebody else's question.

**Bound your own in-flight requests, and adapt the bound.** Backoff fixes *when* you retry. It does not
fix *how many* requests you have in flight. A fleet that receives `503`, backs off with perfect jitter,
and returns at the same concurrency is still pointing the same firehose at twelve slots. Run an AIMD
controller: on `503`, multiplicatively reduce your in-flight cap; on sustained success, additively
increase it. This is what `tower`'s load-shed is telling you, and the only correct reply.

[14 §8](14-workloads-and-clients.md#8-the-client-contract)'s *"cap in-flight per conversation at one"* is
a correctness rule — it stops one user's three messages becoming three completions. It is not a capacity
rule. You need both.

**Keep a retry budget on your side too.** The gateway has one (`reliability/mod.rs`) so that a provider
brownout does not become a self-inflicted DDoS. The identical argument applies one layer out: cap total
retries at ~10% of live traffic and fail fast when the budget is empty. Without it, exponential backoff
still lets a large fleet multiply a brownout.

**Honour `Retry-After`, and back off with full jitter.** Half jitter leaves a fixed floor, and a fixed
floor keeps a synchronised fleet synchronised — which is the exact behaviour backoff exists to destroy.

**Send `Idempotency-Key` on anything you might retry.** At-least-once delivery plus an LLM means paying
twice for one answer. And expect `409`, not `400`, if you reuse a key with a different body (§7).

**Send `x-switchboard-deadline-ms`.** Work you no longer want is work neither of us should pay for.

**Send `user`, if you have end users.** It costs nothing, it lands on the usage row, and it is the only way
`GET /v1/usage/records?user=` can tell you which of *your* customers spent this. The alternative is keeping
a second ledger of your own and hoping it agrees with ours.

**Check the rate card before you spend against it.** `GET /v1/models` publishes `pricing` in nanodollars
per token. A client-side price table goes stale silently, and the first symptom is a reconciliation failure
at the end of a run that has already bought the tokens. One request at boot is cheaper than an invoice.

**Prefer `stream: false` unless a human is watching tokens appear.** Streaming forfeits retry, and today
forfeits fallback ([13 §2.1](13-stability-performance-plan.md#21-stream-has-no-fallback)), hedging (§6),
and cache seeding. If nobody is watching the cursor, you are paying for the privilege of a worse tail.

**Measure the right percentile.** If you stream, your users experience `p99 TTFT`; total latency is a
function of answer length and tells you nothing. If you do not stream, they experience `p99 total`.
Instrument the one your users feel. And compute it per tenant if you are yourself multi-tenant, for the
reason in §5.

---

## 9. Deliberately not doing

**A latency SLO on total request time.** It would be a promise about OpenAI's capacity planning. §1
splits it instead.

**Hedging by default, or hedging before [13 §P0–P2](13-stability-performance-plan.md).** It doubles cost
and, on an unstable system, doubles the instability. §6 gates it on a measured p95, a budget, a closed
breaker, and a free permit — four preconditions, none of which exists today.

**Lowering the argon2 parameters to fix §2.1.** The parameters defend the key hashes at rest. Caching
the verification result is the fix; weakening the primitive is a trade of security for latency that
nobody asked for and that a cache makes unnecessary.

**A weighted-fair-queueing scheduler for §5.** Permit floors are enough. A scheduler that reorders
admission needs a deadline to order against, which is
[13 §1.3](13-stability-performance-plan.md#13-deadline-propagation), which does not exist yet. Build the
clock before the thing that reads it.

**Publishing any number in §1 as *measured* before
[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done) runs.** They are targets. The
distinction between a target and a measurement is the same distinction
[08 §4](08-reliability.md#4-caching) draws between *"34% hit rate, 0.4% false-hit rate"* and *"has a
semantic cache"*, and it is the one this repository keeps choosing to respect.
