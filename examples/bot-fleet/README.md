# bot-fleet

Twenty thousand bot accounts, twelve provider slots, and the architecture in between.

This is a **third party** to switchboard, not a part of it. It is `exclude`d from the gateway's Cargo
workspace, carries its own `Cargo.lock`, and reaches the gateway over HTTP like any customer would. It
must never depend on the `switchboard` crate — see the comment at the top of the repository's root
`Cargo.toml`, and [`docs/16-example-bot-fleet.md`](../../docs/16-example-bot-fleet.md) for why that
constraint is the whole point.

The design it implements is Shape B in [`docs/14`](../../docs/14-workloads-and-clients.md). The client
obligations it implements are [`docs/15 §7–§8`](../../docs/15-latency-and-p99.md).

---

## Before you run it: do the arithmetic

```
slots needed = rps × seconds-per-reply
```

Ollama at 2 concurrent (~5s) plus OpenAI at 10 (~2s) is **12 slots, ≈5.4 rps**. `producer` prints your
offered rps at startup. If it exceeds capacity, nothing in this crate will save you — only more capacity,
less work per request, cache, or accepting less.

That is not a disclaimer. It is the most useful thing this example has to say.

---

## Layout

```
src/
  contract/     what ANY client owes the gateway. Nothing here knows what a bot is.
    client.rs        headers, status codes, deadline propagation, the retry loop
    backoff.rs       full jitter — and why half jitter keeps a fleet synchronised
    aimd.rs          the adaptive in-flight limit; the obligation everyone skips
    retry_budget.rs  retries capped at 10% of live traffic, counted in integers
    usage.rs         GET /v1/usage and /v1/usage/records — the gateway's books

  fleet/        how 20k uncoordinated producers share 12 slots
    broker.rs        Redis Streams, bounded; a full stream is a "busy" reply
    envelope.rs      the two clocks: absolute expires_at, relative attempt budget
    coalesce.rs      one in-flight request per conversation_id
    consumer.rs      the bounded pool; halts the fleet when the account runs dry

  economics/    was any of this worth it?
    money.rs         nanodollars, basis points, no f64 anywhere
    prices.rs        the rate card, and the direct-to-OpenAI baseline
    ledger.rs        three latencies, and everything that never became an answer
    audit.rs         per-request: charged once, at the rate card, and never when refused
    report.rs        the reconciliation, the comparison, and the verdict

  platform/     where the answer goes — the consumer sends it, using the bot's own token

  bin/
    simulate.rs      one process: fleet + pool + report. What run_local_test.sh runs.
    producer.rs      just the fleet, for the two-terminal demo
    consumer.rs      just the pool
```

If you are integrating from another language, `src/contract/` is the file set to re-implement. The rest is
one particular fan-in architecture; the contract is not optional.

---

## Running it

The easiest path is `./run_local_test.sh` from the repository root: it drops fresh Postgres and Redis
volumes, brings the stack up, creates a tenant, funds it, mints a key, runs this simulation against the
live gateway, and prints the balance sheet.

By hand, against a gateway that is already up:

```bash
# From the repository root: a tenant, some money, a key.
TENANT=$(cargo run -q --bin sbctl -- create-tenant acme-corp)
cargo run -q --bin sbctl -- set-budget "$TENANT" 0.002     # dollars, parsed exactly
API_KEY=$(cargo run -q --bin sbctl -- create-key "$TENANT" bot-fleet | tail -n1)
```

Then, from this directory:

```bash
cargo test                   # 72 tests; the interesting logic is all unit-tested

# The whole thing, with a verdict at the end.
SWITCHBOARD_API_KEY="$API_KEY" cargo run --bin simulate -- \
  --bots 20000 --per-bot-per-day 10 --seconds 30 --workers 8
```

Or the two-terminal demo, with no report:

```bash
SWITCHBOARD_API_KEY="$API_KEY" cargo run --bin consumer -- --workers 16
cargo run --bin producer -- --bots 20000 --per-bot-per-day 1 --seconds 60
```

---

## What the report says

```
money (the gateway's ledger is the authority)
  funded            $0.00        spent (this run)  $0.00      remaining  $0.00
  answers           N            cost per answer   $0.00      answers per $1  N

reconciliation (client's books vs the gateway's)
  delta             $0.00   ✓ the books agree

vs. calling the provider directly (no gateway, no cache, no local model)
  saved             $0.00  (82.35%)

what the saving cost, in latency
  model                        answers      cost       p50      p95
  qwen3:4b                          N    $0.000    4800ms   7100ms

throughput and concurrency (the bot's side)
  run duration      30.0s    replies delivered N (0.40/s sustained)
  peak concurrent   4 in flight against the gateway at once

per-request audit (every message the bot sent, against the gateway's row)
  ✓ N requests: each recorded exactly once, charged the rate-card amount,
    and every free answer free for a reason the customer can read.

verdict
  ✓ WORTH IT — the cheap answers are the slow ones.
```

It is arranged so that it can say **no**. Three verdicts mean *do not conclude anything yet*:

* **`BillingDisagreement`** — the client's arithmetic and the gateway's ledger disagree. Stop reading; one
  of the two has a bug. Nothing else on the page is trustworthy.
* **`CapacityShort`** — more than 20% of offered load never became an answer. The run measured capacity,
  not price. Re-run inside capacity, then ask about money.
* **`NotWorthIt`** — it saved nothing, or cost *more* than going direct. **Go direct.** Keep switchboard
  only for what it does besides price: fallback, budgets, one wire format, one place usage is recorded.

And the audit fails the run outright — regardless of verdict — if any single request was charged twice,
charged the wrong amount, charged after being refused, or never recorded at all.

### Make it fail on purpose

```bash
# 1. Capacity short: 23 rps against ~5.4 rps of capacity.
cargo run --bin simulate -- --bots 20000 --per-bot-per-day 100 --seconds 60

# 2. Out of money: the fleet halts on 429 insufficient_quota rather than retrying into a wall.
cargo run -q --bin sbctl -- set-budget "$TENANT" 0.00002

# 3. No saving: route everything to the paid model at list price.
SWITCHBOARD_MODEL=gpt-4o-mini cargo run --bin simulate
```

Watch, in the consumer's log during (1):

* `backlog` climbing. The arithmetic was wrong, and no queue depth fixes that.
* `busy_replies` climbing. The bots are degrading in the product, which is correct.
* `expired` climbing. You are accepting work you cannot do. Refuse earlier, never enlarge the queue.
* `inflight_limit` settling **below** where it started. AIMD found the real bottleneck.

The architecture is behaving perfectly. The capacity is not there. Those are different problems, and being
able to tell them apart at a glance is the reason the four numbers are logged together.

---

## Environment

| variable | default | notes |
|---|---|---|
| `SWITCHBOARD_BASE_URL` | `http://127.0.0.1:8080` | |
| `SWITCHBOARD_API_KEY` | — | required by `consumer` |
| `SWITCHBOARD_MODEL` | `qwen3:4b` | local capacity, priced — see below |
| `REDIS_URL` | `redis://127.0.0.1:6379` | the broker, not the gateway's |
| `RUST_LOG` | `info` | `bot_fleet=debug` to see every reply |

**Every model on the rate card is billed.** Nothing is free. `qwen3:4b` costs the bot $0.05/$0.10 per
million tokens; that it runs on a GPU switchboard already owns is switchboard's business, not the bot's.
A free model would make every bill zero and every saving look like 100%, which is not a measurement.

Note the asymmetry the report is built to expose: `qwen3:4b` through the gateway is ~82% cheaper than
`gpt-4o-mini` direct from OpenAI, while `gpt-4o-mini` *through the gateway* is **more expensive** than
going direct, because switchboard resells it at a markup. Run `SWITCHBOARD_MODEL=gpt-4o-mini` and the
verdict says so, in those words.

---

## A note on what is not implemented yet

Two things this client uses are specified in [`docs/15 §7`](../../docs/15-latency-and-p99.md) and not yet
built in the gateway: the `x-switchboard-deadline-ms` header, and `409` on an idempotency conflict (it
returns `400` today). The client sends and handles both regardless.

This is safe, and deliberate. A gateway that ignores an unknown request header degrades into "the header
did nothing"; one that returns `400` where `409` is specified degrades into "the conflict was fatal" —
which is what the client does with a `409` anyway. **The example is written against the specification,
not against the current code.** When [`docs/13 §P0`](../../docs/13-stability-performance-plan.md) lands,
this gets better without a diff here.

Two things it uses that *were* missing, and are not any more, because writing this example is what found
them: `response.model` now names the model that actually answered, and `x-switchboard-cache` marks a
replayed response. Without both, a client pricing its own tokens silently bills itself for free answers,
and the reconciliation in `report.rs` fails with a `BillingDisagreement`. There is a unit test that
reproduces exactly that, so the regression cannot come back quietly.
