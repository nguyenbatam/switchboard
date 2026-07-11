# 11 — Config, deploy, and testing

← [10 Observability](10-observability.md) · [index](README.md) · next: [12 Build order](12-build-order.md)

---

## 1. Configuration

`figment`, layered: `config/switchboard.toml` ← `config/local.toml` (gitignored) ← env
(`SB__SERVER__PORT=9000`).

Secrets come only from the environment. Everything else is a committed file, so that a config change
is a reviewable diff.

Two env namespaces, deliberately:

| | example | why |
|---|---|---|
| `SB__SECTION__KEY` | `SB__SERVER__PORT=9000` | overrides any key in the TOML, mechanically derived from its path |
| bare, conventional names | `DATABASE_URL`, `REDIS_URL`, `OPENAI_API_KEY`, `ANTHROPIC_API_KEY` | secrets, plus the names the ecosystem already demands |

`DATABASE_URL` is not renamed to `SB__DATABASE__URL` because `sqlx migrate` and `cargo sqlx prepare`
read `DATABASE_URL`, and a project where the tooling and the binary disagree about where the database
is has invented a bug for itself. The same argument covers the vendor key names: `OPENAI_API_KEY` is
what every example on the internet sets. `.env.example` is the authority on this list.

Sections: `server`, `database`, `redis`, `auth`, `ratelimit` (defaults + per-tenant overrides),
`providers`, `models` (the registry and price table — see [02 §3](02-providers.md#3-routing)), `rag`,
`cache`, `agent`, `telemetry`.

Config is parsed into typed structs and **validated at boot**. An invalid config is a startup failure,
never a runtime surprise at 4pm on a Friday. Validation includes the cross-field rules that a schema
cannot express: every `fallback` alias must exist; every model's `tokenizer` must be loadable; a
`price_in` of zero on a non-local provider is a warning.

No hot reload. `SIGHUP` reload is a feature with a failure mode (half-applied config) and no demand.

---

## 2. Deploy

Multi-stage Docker → distroless. `cargo-chef` for dependency caching, so a one-line change does not
rebuild 400 crates.

### Boot sequence

```
load config → connect PG → pg_advisory_lock → migrate → unlock
    → connect Redis → warm tokenizers → probe providers (non-fatal) → bind
```

Tokenizers are warmed at boot rather than lazily, because the alternative is that the first request of
each model pays 300ms and the p99 chart has an unexplained step in it every deploy.

Provider probing is non-fatal: OpenAI being unreachable at boot should not stop a gateway whose main
traffic is Ollama.

### Health

- `/health` — liveness. The process is up. Returns 200 as long as it can serve a response.
- `/ready` — readiness. Checks Postgres, Redis, and **≥1 reachable provider**. A replica with zero
  providers should not receive traffic; it can only produce 502s.

The distinction matters under a load balancer: a failing `/health` gets the container killed, a failing
`/ready` gets it removed from rotation. Conflating them turns a transient Redis blip into a restart
loop.

### Shutdown

`SIGTERM` → stop accepting new connections → drain in-flight streams, up to 30s → flush the usage
queue → exit. A gateway that drops the usage queue on shutdown loses money on every deploy, and it
loses it silently.

Fly.io or a VPS, HTTPS, CI/CD from GitHub Actions. Kubernetes deferred per the roadmap, and probably
forever: a k8s manifest nobody runs is weaker evidence than one live endpoint.

---

## 3. Testing, in the order that catches the most bugs per hour

### 1. The conformance suite — `tests/conformance/`

The real Python `openai` SDK, against the real binary. `stream=True` and `stream=False`, tool calling,
parallel tool calls, embeddings, and the error paths.

**This is the acceptance test for the entire project.** The roadmap's success criterion is a stranger
pointing the SDK at this server and having everything work; this suite is that criterion, executable.
It must never be skipped, and a red conformance run blocks every merge.

Not curl. The SDK is stricter than curl — it validates chunk shapes, it distinguishes absent from
null, and it raises typed exceptions that depend on our error envelope being right.

### 2. Golden SSE transcripts — `tests/golden/` — **not built**

Recorded once from real OpenAI, replayed by a mock provider. Byte-for-byte comparison of our chunk shape
against theirs. This is what the $20 of OpenAI credit is really for: the transcripts outlive the credit.

Neither `tests/golden/` nor `providers/mock.rs` exists. What covers this ground today is layer 1 (the real
OpenAI SDK against a live gateway, 59 conformance tests) and layer 3 (`wiremock` fixtures written by hand
rather than recorded). The gap those two leave is exactly the one this layer was for: our chunk shape is
checked against what the SDK *accepts*, never against what OpenAI actually *sent*.

### 3. Provider contract tests — `wiremock`

Per provider, including the ugly cases that only happen in production:

- a 429 arriving *mid-stream*
- a truncated final chunk with no `[DONE]`
- invalid UTF-8 split across a chunk boundary (a multi-byte character halved by the transport)
- `content-type: text/plain` on an error response, where JSON was promised

### 4. Property tests — `proptest`

On the SSE codec (`decode(encode(x)) == x`) and on the request canonicaliser: `canonical(x) ==
canonical(x)` always, and `canonical(x) != canonical(y)` whenever `x` and `y` differ semantically. The
second property is the one that keeps the cache from returning another request's answer
([08 §4](08-reliability.md#4-caching)).

### 5. Repository tests — `sqlx::test`

Against a real Postgres, in a transaction that always rolls back. `cargo sqlx prepare` means every SQL
string is typechecked at compile time, including in CI, without a live database.

### 6. The eval suite

The regression test for everything that is statistical rather than logical, and structurally invisible
to the five layers above. See [09](09-eval.md).

### Where each layer actually stands

| layer | status |
|---|---|
| conformance | **built** — 60 tests in `tests/conformance/`, the acceptance test for the project |
| unit + integration | **built** — 18 tests: `src/` unit tests, `tests/agent_phase4.rs`, `tests/rag_phase5.rs` |
| eval suite | **built** — `cargo run --bin eval`, 7 cases, latency baselines |
| golden SSE transcripts | not built — `tests/golden/` does not exist yet |
| provider contract tests | not built — `wiremock` is a dev-dependency with no tests behind it |
| property tests | not built — same, for `proptest`. The cache canonicaliser property in [08 §4](08-reliability.md#4-caching) is the first one to write |

The four missing layers are ordered by what they would have caught. Two dev-dependencies sitting
unused is a to-do list, not a test strategy.

---

## 4. What CI runs

`.github/workflows/ci.yml`, on every push to `main` and every PR. Three jobs; `conformance` and
`eval-fast` both need `rust` to go green first.

| job | what | gate |
|---|---|---|
| `rust` | `cargo fmt --all -- --check`, `cargo clippy --all-targets --all-features -D warnings`, `cargo test --all-features` — against Postgres and Redis service containers | blocking |
| `conformance` | `cargo build --bin switchboard`, then pytest + the real `openai` SDK | blocking |
| `eval-fast` | `cargo run --bin eval -- --suite fast` — no server, so it scores offline at the 0.5 gate ([09 §6](09-eval.md#6-the-harness-as-built--cargo-run---bin-eval)) | blocking on regression |

Not yet wired, and named here so the gap stays visible rather than being rediscovered: nightly and
weekly full-eval runs, `cargo audit` / `cargo deny`, and `cargo build --locked` as a distinct stage.

The one command that runs all of it *plus* live Ollama, migrations, a seeded tenant, the latency
benchmark and [13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done)'s load test is
`./run_local_test.sh`. It is strictly stronger than CI, because CI has no GPU, no model, and no p99 worth
measuring on a shared runner.
