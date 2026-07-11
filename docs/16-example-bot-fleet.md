# 16 — A worked example: the bot fleet

← [15 Latency & the p99 contract](15-latency-and-p99.md) · [index](README.md)

[14](14-workloads-and-clients.md) describes Shape B in the abstract. [15](15-latency-and-p99.md)
specifies the wire. This document points at the thing that actually runs:
[`examples/bot-fleet/`](../examples/bot-fleet/) — twenty thousand bot accounts, twelve provider slots,
and the architecture in between.

---

## 1. Why it is not a member of this workspace

Open the repository's root `Cargo.toml` and the example is `exclude`d, not `members`ed. It carries its
own `Cargo.lock`. `cargo test` here never builds it. That is not tidiness; it is the claim.

> **Shapes B and C are systems that *call* the gateway. They are not features of it.**
> — [14 §1](14-workloads-and-clients.md#1-the-question-that-decides-everything)

An example that could `use switchboard::` would be demonstrating the opposite of the thing it exists to
teach. So it cannot. Its only access to the gateway is the HTTP wire in
[15 §7](15-latency-and-p99.md#7-the-wire-contract) — the same access your customers have.

This is the repository-level form of [00's anti-corruption line](00-overview.md#1-the-anti-corruption-line),
and it is held to 00's own standard: *the claim is falsifiable, which is the point.* Adding a path
dependency is a visible act, in a commit, that someone has to defend.

It also means the example may lag the gateway, and it says so out loud rather than pretending. When this
was written, three things in [15 §7](15-latency-and-p99.md#7-the-wire-contract) were specified and not
implemented — `x-switchboard-deadline-ms`, the `409` on idempotency conflict, and `x-switchboard-hedged`.
The client sent and handled all three anyway. A gateway that ignores an unknown request header and returns
`400` where `409` is specified degrades gracefully into "the header did nothing" and "the conflict was
fatal", which are the right behaviours regardless. **The example is written against the specification.**

Two of the three have since landed, and the client started working better without changing a line, which
is what writing against a specification buys. Only `x-switchboard-hedged` ([15 §6](15-latency-and-p99.md#6-hedged-requests))
remains unbuilt.

---

## 2. Do the arithmetic before reading the code

The example will run at any scale you ask for. It cannot make the numbers work.

```
concurrency = throughput × latency        →        slots needed = rps × seconds-per-reply
```

Ollama at 2 concurrent (~5s) plus OpenAI at 10 (~2s) is **12 slots, ≈5.4 rps**. Against that:

| messages per account per day | offered load | slots needed | verdict |
|---|---:|---:|---|
| 1 | 0.23 rps | 1 | comfortable |
| 10 | 2.3 rps | 7 | tight |
| 100 | 23 rps | 69 | **5× short** |

`bin/producer.rs` prints your offered rps at startup for exactly this reason. If it exceeds capacity, no
broker, pool, or controller in this crate will save you — only more capacity, less work per request,
cache, or accepting less. [14 §2](14-workloads-and-clients.md#2-do-the-arithmetic-before-drawing-the-boxes)
comes before every diagram in that document, and this section comes before every file in this one.

---

## 3. The shape

```
20k bots ──(publish, fire-and-forget)──► broker ──► consumer pool (N ≈ 16)
                                           │              │
                                    bounded stream        ▼
                                           │        gateway /v1/chat  (synchronous)
                                    full → "busy"         │
                                                   Ollama(2) · OpenAI(10)
                                                          │
                                       consumer sends the reply via the bot's own API
```

The unlocking observation, and everything follows from it: **the reply does not have to travel back over
the request's connection.** The bot holds a Telegram token. The consumer of the answer is durable. So the
bots publish and forget, a fixed pool of `N` consumers drains the broker, and the consumer *is* the thing
that talks to the user — which means no reply-routing table exists to get wrong.

Two consequences worth restating: `N` is a constant you choose, not twenty thousand, and it does not move
when the fleet reaches 200k. And the gateway can stay synchronous, because it never sees more than `N`
concurrent requests.

---

## 4. What is in there, and which document each file answers

The crate splits three ways, because three different questions are being answered.

### `src/contract/` — what any client owes the gateway

Nothing here knows what a bot is. This is [15 §7](15-latency-and-p99.md#7-the-wire-contract) and
[§8](15-latency-and-p99.md#8-what-a-client-must-do-to-get-a-good-p99) as code, and it is the part worth
copying into your own language.

| file | what it settles |
|---|---|
| `client.rs` | headers, status-code semantics, deadline propagation, the retry loop |
| `headers.rs` | the headroom the gateway publishes on every response, and how to spend it |
| `models.rs` | the rate card, and whether this client still knows what it is paying |
| `backoff.rs` | full jitter, and why half jitter keeps a fleet synchronised |
| `aimd.rs` | the adaptive in-flight limit — the obligation everyone skips |
| `retry_budget.rs` | retries capped at 10% of live traffic |

Five of these deserve a sentence.

**`headers.rs` is the obligation that comes *before* the other two.** `backoff.rs` and `aimd.rs` are
reactions: they run after a `429` or a `503`. But the gateway puts `x-ratelimit-remaining-requests`,
`-remaining-tokens` and `x-switchboard-budget-remaining-nano` on **every** response — success and refusal
alike — so a client that reads them never has to learn its limit by hitting it in the middle of somebody's
sentence. When a bucket reads zero, sit out its `x-ratelimit-reset-*`. When the balance cannot cover one
more answer, stop: the previous response already said so, and firing anyway spends a user's patience and a
provider's slot to hear it again.

Two of those headers fail silently if you skim them. `x-ratelimit-reset-*` carries its unit — `4171ms`
read as seconds stalls a fleet for seventy minutes. And `x-switchboard-budget-remaining-nano` is
**signed**, because the gateway's budget check is a pre-check against batched spend and a tenant overshoots
by one batch interval of its traffic plus its in-flight requests
([15 §7](15-latency-and-p99.md#7-the-wire-contract)); parse it unsigned and a small overspend reads as an
astronomical balance.

**`x-switchboard-cache` and `x-switchboard-replay` mean opposite things about money**, and the type in
`headers.rs` refuses to let you merge them. Both replay a stored body, `usage` object and all. A cache hit
means no provider ran and nothing was billed — count zero. A replay means a provider ran on *this
request's first attempt* and was billed exactly once, on the row carrying the same `x-request-id` — count
the answer once, at full price. Price a replay at zero and your books come in under the invoice. Price a
cache hit at full rate and they come in over it. Neither error announces itself, which is why
`Provenance::gateway_billed_zero()` is true for one and false for the other and there is no method that
returns true for both.

A replay is also a proof: it is the `Idempotency-Key` working. A replay with **no retry behind it** is a
key collision, and the user just read an answer generated for somebody else's question. `economics/audit.rs`
reports it.

**`models.rs` checks the rate card before the run spends against it.** `economics/prices.rs` holds a
client-side copy of what the gateway charges, and says why it must not simply read the gateway's numbers:
the question this crate answers is *"would I be better off going direct"*, and answering it with the
gateway's own figures would assume the conclusion. That reasoning is right, and it leaves a copy that goes
stale silently. So the **direct** baseline stays this client's own belief about OpenAI's list prices, and
the **gateway's** rate card — a fact the gateway is authoritative about — is fetched from `/v1/models` and
compared against what we believed. One request at boot, and the drift is a warning rather than a
reconciliation failure at the end of a run that already bought the tokens.

**`aimd.rs` is the one people do not build.** Backoff fixes *when* you retry; it does nothing about *how
many requests you have in flight*. A fleet that receives `503`, backs off with perfect jitter, and returns
at the same concurrency is still pointing the same firehose at twelve slots. Additive increase on success,
multiplicative decrease on `503` — TCP's control law, for TCP's reason: it finds the bottleneck without
being told where it is.

Note what it does **not** react to. A `429` means *this tenant exceeded its own quota* and says nothing
about gateway capacity; shrinking there would mean reacting to your own rate limit by falling below it
forever. That `429` and `503` are distinct status codes is what makes the distinction expressible —
[15 §7](15-latency-and-p99.md#7-the-wire-contract).

**Shrinking a live limit is the interesting half.** You cannot take a permit back from a task that is
using it. So a reduction is *owed*, and the next permit released is retired instead of returned. The limit
falls as fast as requests complete, which is as fast as it can.

**`retry_budget.rs` counts in integers**, and the test that forced it is worth the detour.
`0.1 + 0.1 + … ` ten times is `0.9999999999999999` in `f64`, so the retry you earned is never affordable —
and the drift *grows with traffic*, meaning the control you added for 3am is least accurate at 3am.
[00's money rule](00-overview.md#money-time-and-ids--three-conventions-set-once) — *bigint nanodollars,
never `f64`* — is not about money. It is about ledgers. A retry budget is a ledger.

### `src/fleet/` — how twenty thousand producers share twelve slots

| file | what it settles |
|---|---|
| `broker.rs` | Redis Streams, bounded; `Publish::Full` is the "busy" signal |
| `envelope.rs` | the two clocks — absolute `expires_at`, relative attempt budget |
| `coalesce.rs` | one in-flight request per `conversation_id` |
| `consumer.rs` | the bounded pool, and the four numbers to watch |

**The two clocks are [14 §6's deadline trap](14-workloads-and-clients.md#6-the-primitives-all-three-share).**
`expires_at` is absolute and set at submit: an *SLA*, whose only job is killing stale work. The attempt
timeouts are relative and start at dequeue: a *processing budget*. Conflate them and you build a machine
that only ever expires things.

There is a second, quieter reason they differ, and it is the same reason
`x-switchboard-deadline-ms` is relative. Inside one fleet there is one clock domain, so an absolute
`expires_at` is safe. Across an organisational boundary it is not — your clock and the gateway's disagree,
and an absolute timestamp would encode the skew. So the fleet stores absolute and sends relative,
recomputed at the instant of the call.

**Coalescing is a correctness rule, not a capacity rule.** It stops one user's three messages becoming
three completions. It does nothing to bound your total concurrency, which is `aimd.rs`'s job. A fleet with
perfect coalescing and no concurrency cap still saturates the gateway; it just does so with distinct
conversations. [15 §8](15-latency-and-p99.md#8-what-a-client-must-do-to-get-a-good-p99) requires both.

### `src/economics/` — was any of this worth it?

A third party does not adopt a gateway because its architecture is tasteful. It adopts it because the
same answers cost less, or arrive faster, or because one wire format is worth a small tax. This module
measures which of those happened, in integers, and is built so that the answer can be **no**.

| file | what it settles |
|---|---|
| `money.rs` | nanodollars; percentages as basis points; no `f64` anywhere |
| `prices.rs` | the rate card the client believes, and the direct-to-provider baseline |
| `ledger.rs` | the client's own book: three different latencies, and what never became an answer |
| `report.rs` | the reconciliation, the comparison, and the verdict |

**Three latencies, and they are not the same number.** `gateway_ms` is the HTTP call. `queue_wait_ms` is
how long the job sat on *your* broker — invisible from the gateway's side, and not its fault.
`end_to_end_ms` is enqueue to reply: what the human actually experienced. A report that quotes only the
first is quoting the number that flatters it.

**`money.rs` counts in integers, and `retry_budget.rs` already taught us why.**
[00's money rule](00-overview.md#money-time-and-ids--three-conventions-set-once) — *bigint nanodollars,
never `f64`* — is not about money. It is about ledgers, and the drift *grows with traffic*, so the
arithmetic is least accurate exactly when the bill is largest.

### The verdict, and the order it is decided in

Four questions, checked in this order, because a later answer is worthless if an earlier one is bad:

1. **Do the books agree?** If the client's arithmetic and the gateway's ledger disagree, every number
   below is suspect and nothing may be concluded. `Verdict::BillingDisagreement`.
2. **Was there enough capacity?** A workload that was mostly refused has not been priced, it has been
   rationed. Re-run inside capacity, then ask about money. `Verdict::CapacityShort`.
3. **Did it save anything** against calling the provider directly? If every answer came from the paid
   model at list price, switchboard added a hop, a queue and an argon2 verify for nothing, and the report
   says **go direct**. `Verdict::NotWorthIt`.
4. **What did the saving cost?** The per-model table. The cheap answers are the slow ones.

Two caveats the report prints rather than hides. The local model is priced at whatever
`config/models.toml` says, so a rate of zero makes any workload look free — electricity, GPUs and the
pager are real and are not in the table. And it says nothing about **answer quality**; that is what
[09](09-eval.md)'s eval suite is for. A cheaper wrong answer is not a saving.

### Three sets of books, and they must agree to the nanodollar

`bin/simulate.rs` reconciles three independently-kept ledgers:

1. **The client's own arithmetic** — every answer priced at `response.model`, with `x-switchboard-cache`
   hits at zero, against the published rate card.
2. **The gateway's rollup** — `GET /v1/usage`, summed from `usage_hourly`.
3. **The gateway's fact table** — `GET /v1/usage/records`, one row per call.

Any disagreement fails the run. This is not ceremony. Two of the three checks only became *possible*
when the gateway started telling the truth about which model answered and whether a response came from
cache — before that, a client pricing its own tokens would silently bill itself for free answers, and the
delta would have looked like a rounding error rather than the cache hit rate.

The third check — fact table against rollup — is the one that catches switchboard rather than the client.
Both are written in one transaction ([03](03-database.md)). If they ever disagree, the rollup has drifted
and every aggregate the gateway has ever served is wrong.

### Two kinds of money, and confusing them is how a gateway loses money quietly

`config/models.toml` carries **two prices per model**, and they answer different questions:

| | who pays | who sees it |
|---|---|---|
| `price_in` / `price_out` | the tenant pays **us** | the tenant, on `/v1/usage` |
| `cost_in` / `cost_out` | we pay **the provider** | only `sbctl stats` |

**Every request is billed.** There is no free model on the rate card. A local answer costs the bot
`$0.05/$0.10` per million tokens; it costs *us* a GPU we already own. A `gpt-4o-mini` answer costs the bot
`$0.20/$0.80`; it costs us OpenAI's `$0.15/$0.60`. `margin = price − cost`, and it is the only reason the
business exists.

The registry **refuses to load** a model priced below its cost. The failure that prevents is not a crash:
it is a gateway that serves happily for a month and loses money on every request, which nobody notices
until the provider's invoice exceeds the customers'.

The margin never crosses the wire. `/v1/usage` and `/v1/usage/records` expose `cost_nano` — what the
tenant paid — and nothing else. [Migration 007](../crates/switchboard/migrations/007_margin.sql) revokes
`upstream_cost_nano` from the read-only role that `sql_query` reaches, because that role is reachable from
anything an attacker can write into a prompt ([06 §4](06-agent-tools.md#4-prompt-injection-taint-and-capability-downgrade)).
A tenant who can see your margin can negotiate against it.

This is also what makes the report's comparison honest. `qwen3:4b` through the gateway is **82.35% cheaper**
than `gpt-4o-mini` direct from OpenAI. `gpt-4o-mini` *through the gateway* is **more expensive** than going
direct, because we resell it at a markup — and the report says exactly that, in those words. The value
switchboard sells is the local model, the fallback, the budget, and one wire format across a price curve
that runs from a GPU in the corner to a frontier model. It is not undercutting OpenAI on OpenAI's own
inference, and a report that implied otherwise would be a brochure.

### What a bot actually wants to know

> *"I put $A in. How many answers did I get, what is left, was every request charged correctly, and would I
> have been better off calling OpenAI directly?"*

The report prints: funded, spent, remaining, answers, cost per answer, answers per dollar, the
direct-to-provider baseline, the saving as a percentage, the latency that saving cost split by the model
that actually served each answer — and:

**Throughput and concurrency.** Run duration, replies per second sustained, and the **peak number of
requests the fleet ever had in flight at once**. Little's Law reads backwards here: if peak concurrency
sits far below your AIMD ceiling, the bottleneck is upstream, not your pool size.

**A per-request audit**, which is the check that actually protects a customer. Aggregates can agree while
every row is wrong: two answers overcharged by a nanodollar and two undercharged reconcile perfectly, and
the customer reading the invoice is still right to complain. So every message the bot sent is matched, by
`x-request-id`, against the gateway's own row, and five things are asserted:

1. **It was recorded.** An answer with no row is revenue you lost.
2. **It was recorded once.** Two rows for one `Idempotency-Key` means the retry bought a second completion.
   That is the failure idempotency exists to prevent, it costs real money, and it is invisible in totals.
3. **The amount is right**, to the nanodollar, against the rate card.
4. **The reason is right.** A cache hit must read `free_cache_hit`, not `free_zero_cost`. A zero the
   customer has to explain to themselves is a support ticket.
5. **Nothing refused was charged.** A `503`, a `429`, or an abandoned request never reached a provider.

There is no *refund* to check, because there is nothing to reverse: switchboard bills only for tokens a
provider generated. What a bot calls "a refund" is really "a request that must never have been charged",
and checks 4 and 5 are how that is verified.

### And what the operator wants to know

`sbctl stats [tenant]` — the other side of the same rows, and it never leaves your machine:

```
revenue    what tenants paid you
cogs       what you paid the providers
margin     the difference, and a warning if it is negative
latency    p50 / p95 / p99 / max, over successful requests only
```

Concurrency comes from `/metrics`: `sb_inflight_requests_peak`, an in-process high-water mark. A gauge
scraped every fifteen seconds cannot see a four-hundred-millisecond saturation spike, and a percentile of
a concurrency number is a category error.

Latency percentiles exclude errors and use `percentile_disc`, not `percentile_cont`: an interpolated
latency is a number no request ever experienced. They also exclude shed `503`s, which return in about a
millisecond and would drag every percentile down as the gateway got sicker
([15 §4](15-latency-and-p99.md#4-the-p99-metric-trap)).

### `src/platform/` — where the answer goes

A mock. A real one is a `POST` to `api.telegram.org/bot<token>/sendMessage`, and it teaches nothing about
switchboard. What it *does* carry is `BUSY_REPLY`, and the rule behind it:

> **Degrade in the product, not in the infrastructure.** When the broker is full or the deadline has
> passed, answer the user immediately. People forgive a fast refusal. They do not forgive silence, and
> they have already left by the time a late answer arrives.

---

## 5. Three traps this example is arranged to make visible

**A per-process limit is not a limit.** Three consumer pods each holding `N = 16` send forty-eight
concurrent requests at a gateway sized for twelve. Run one consumer deployment at `replicas: 1`, or divide
`N` by the replica count. The same trap lives inside the gateway — `CircuitBreaker` and `RetryBudget` are
per-process, so across N replicas the thresholds multiply
([13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done)).

**A `503` is not the gateway failing you.** It is
[the mechanism that keeps p99 a constant](15-latency-and-p99.md#3-shedding-is-the-p99-mechanism). A
gateway that queued instead would hand you the same wait with no way to see it coming, and your AIMD
controller would have nothing to learn from.

**Backlog depth, not p99.** For a fan-in workload p99 latency is nearly meaningless; the numbers that are
not are backlog depth, measured throughput, expired-before-attempted, and cache hit rate
([14 §9](14-workloads-and-clients.md#9-what-to-measure)). `bin/consumer.rs` logs them every five seconds.
A growing backlog means §2's arithmetic was wrong, and enlarging the queue is never the fix.

---

## 6. Running it

**`./run_local_test.sh` runs the whole thing.** Its last phase drops fresh Postgres and Redis volumes,
creates a tenant, funds it with `$0.002`, mints an API key, points a bot fleet at the live gateway, and
prints the balance sheet. If the three sets of books disagree by one nanodollar, the script fails.

By hand, against a gateway that is already up:

```bash
sbctl create-tenant acme-corp                     # prints a tenant id
sbctl set-budget <tenant-id> 0.002                # fund it, in dollars, parsed exactly
KEY=$(sbctl create-key <tenant-id> bot-fleet)

cd examples/bot-fleet
cargo test                                        # 72 tests; the interesting logic is all unit-tested
SWITCHBOARD_API_KEY=$KEY cargo run --bin simulate -- \
  --bots 20000 --per-bot-per-day 10 --seconds 30 --workers 8

sbctl budget <tenant-id>                          # limit / spent / remaining
```

### Make it fail on purpose

Three different failures, each of which the report distinguishes:

```bash
# 1. Capacity short. 23 rps against ~5.4 rps of capacity — the "5× short" row of §2.
#    Backlog climbs, busy replies climb, AIMD settles below where it started.
#    Verdict: CapacityShort. The architecture is fine; the arithmetic was wrong.
cargo run --bin simulate -- --bots 20000 --per-bot-per-day 100 --seconds 60

# 2. Out of money. Fund almost nothing and watch the fleet halt on 429 insufficient_quota
#    rather than retry into a wall until the billing month turns over.
sbctl set-budget <tenant-id> 0.00002

# 3. No saving. Route everything to the paid model at list price.
#    Verdict: NotWorthIt — "go direct".
SWITCHBOARD_MODEL=gpt-4o-mini cargo run --bin simulate
```

The third is the one worth running before you believe anything else in this repository.

---

## 7. Deliberately not doing

**Shape C (batch).** It belongs in a `jobs` table with `SELECT … FOR UPDATE SKIP LOCKED`, and
[14 §5](14-workloads-and-clients.md#5-shape-c--batch) designs it. It is a different program, and the first
thing that section says is to check whether OpenAI's Batch API dissolves the problem for half the price
before writing a scheduler — because a scheduler is more fun to write.

**A real Telegram client.** It would double the surface to maintain for something that is already an
example, and it would teach nothing about the gateway.

**Making this the load test.** Tempting — the producer already generates the burst and the one-noisy-tenant
scenario. But a load test should be a small, boring thing you trust, and this is a system with a control
loop in it. If the fleet and the gateway are ever both wrong, a load test built from the fleet will agree
with itself.

[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done)'s load test is a separate binary
with no AIMD controller, no retries and no backoff: it fires at a rate fixed in advance and records what
comes back. That is the whole reason it can measure a shed rate. This fleet, pointed at the same overload,
would back off until the gateway looked healthy — which is the client behaving correctly and the
measurement being worthless.
