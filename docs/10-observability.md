# 10 — Observability

← [09 Evaluation](09-eval.md) · [index](README.md) · next: [11 Config, deploy, testing](11-config-deploy-testing.md)

---

## 1. Logs

`tracing`, JSON to stdout. `request_id` — from `x-request-id`, or a fresh UUIDv7 — sits on the root
span, so it is on every line beneath it without being passed anywhere.

**Never log prompt content.** It is customer data, and a gateway is exactly the place where every
customer's data passes through one process's stdout. Log token counts, hashes, model names, and ids.

There is no `log_prompts` flag, and that is the safe shape: a debugging convenience that exfiltrates
customer prompts when somebody forgets to unset it is not a convenience. If one is ever added it must
refuse to start under a production configuration, which means a notion of environment that this config
does not currently have either.

---

## 2. Traces

OTLP export. One trace, one request:

```
http.request  ─ auth ─ ratelimit ─ cache.lookup ─ agent.loop
                                                   ├─ provider.chat  (attrs: model, ttft_ms, tokens)
                                                   ├─ tool.sql_query ─ db.query
                                                   └─ provider.chat
```

Span attributes follow the OpenTelemetry **GenAI semantic conventions** — `gen_ai.system`,
`gen_ai.request.model`, `gen_ai.usage.input_tokens` — so that any OTel-native backend renders this
without a custom dashboard. Inventing attribute names here costs nothing today and costs a migration
the first time someone points Datadog or Honeycomb at it.

The trace is also where the full, untruncated tool result lives; `tool_invocations.result` in Postgres
stores only the first 8KB ([03](03-database.md#the-agents-audit-trail)).

---

## 3. Metrics

Prometheus. The names, chosen once:

```
sb_requests_total{route,model,status}          sb_request_duration_seconds{route,outcome}  (histogram)
sb_ttft_seconds{model}          (histogram)    sb_output_tokens_per_second{model}    (histogram)
sb_tokens_total{model,kind}                    sb_cost_nanodollars_total{model}
sb_provider_errors_total{provider,kind}        sb_circuit_breaker_state{provider}
sb_cache_hits_total{kind}                      sb_ratelimit_denied_total{dimension}
sb_agent_iterations{model}      (histogram)    sb_retrieval_latency_seconds{stage}   (histogram)
sb_usage_records_dropped_total                 sb_token_count_drift_total{model}
sb_inflight_requests                           sb_requests_shed_total{route}
sb_inflight_requests_peak                      sb_provider_inflight{provider}
sb_provider_inflight_limit{provider}
```

That is the whole list, and it is exactly what
[`telemetry/metrics.rs`](../crates/switchboard/src/telemetry/metrics.rs) emits. Two names that used to sit
in this block have been taken out rather than left as aspiration, because a metric block reads as an
inventory and a dashboard built from it would render two permanently empty panels:

- **`sb_gateway_overhead_seconds{route}`** — the metric [15 §1](15-latency-and-p99.md#1-what-a-gateway-can-and-cannot-promise)
  states its SLO in terms of, and the one metric the gateway does not emit. It is
  `sb_request_duration_seconds` minus provider time, and nothing records provider time. Time the provider
  call, emit the difference, and it exists. Until then §1's numbers come from
  [13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done)'s load test, which manufactures
  the subtrahend by controlling it.
- **`sb_cache_false_hits_total{kind}`** — belongs to the semantic cache, which
  [08 §4](08-reliability.md#4-caching) forbids shipping without two weeks of shadow-mode false-hit data.
  There is no semantic cache, so there are no false hits to count.

### `outcome`, and why `sb_request_duration_seconds` cannot do without it

`outcome` is one of `served | shed | timeout | error`, and it is the only label on this page that
exists to stop a metric from lying.

A shed request returns a `503` in about a millisecond. Bucketed together with real requests, it drags
the histogram down — so a gateway that is shedding half its traffic reports a **better** p99 than a
healthy one. The alert in §5 would get quieter as the outage got worse. Alert on
`outcome="served"` only, and put the shed rate on the panel beside it. Neither number means anything
alone. Full argument in [15 §4](15-latency-and-p99.md#4-the-p99-metric-trap).

**Two different things return `503`, and only one of them is a shed.** The gateway refusing because it is
full is `outcome="shed"` and increments `sb_requests_shed_total`. A provider that is down is
`outcome="error"` and increments `sb_provider_errors_total`. A status code cannot tell them apart, so
[`ApiError`](../crates/switchboard/src/http/error.rs) labels itself and the label rides the response. If
the two were merged, the 0.1% shed-rate SLO would fire whenever OpenAI had a bad afternoon — and §5 below
is precisely the rule that says not to page a human for that.

`sb_gateway_overhead_seconds` is `duration − provider_duration`: the part of the latency switchboard
actually owns, and therefore the only part it can put in an SLO. See
[15 §1](15-latency-and-p99.md#1-what-a-gateway-can-and-cannot-promise).

`sb_request_duration_seconds` is recorded by
[`telemetry::inflight_layer`](../crates/switchboard/src/telemetry/mod.rs), which wraps `/v1` only —
a Kubernetes probe is not traffic. `sb_gateway_overhead_seconds` still does not exist: the minuend is
there now, the subtrahend is not.

`sb_provider_inflight_limit` is published once at boot and is the denominator of the saturation alert.
Without it, "is this provider full?" has to be written `sb_provider_inflight > 0`, which fires whenever
anybody is using the gateway. An alert that cannot distinguish *busy* from *full* is one people learn to
ignore.

### Cardinality discipline

**Never** label by `request_id`, `api_key_id`, or `user`. Each one turns a counter into a database, and
a bad one.

`tenant` is labelled on **no** metric. It was going to go on two counters, capped so that beyond 1000
tenants the label became `other`; the cap is the sort of thing that is easy to write down and easy to get
wrong, and nothing needed it. Per-tenant numbers come from Postgres, where they belong. A metrics system
is for aggregates. `usage_records` is for facts about a customer.

One consequence is worth naming, because
[13 §P5](13-stability-performance-plan.md#p5--measure-then-claim--done) measured it: with no `tenant`
label, no metric on this page can show you one tenant starving another. The victim's p99 does not move —
it cannot, since a shedding gateway has no queue — and its shed rate is summed with everyone else's. That
is an argument for per-tenant permit floors, not for a high-cardinality label.

---

## 4. The one dashboard panel that matters

`sb_ttft_seconds` plotted **against** `sb_output_tokens_per_second`, because they measure the two
different halves of inference and a single "latency" number conflates them:

- **TTFT is dominated by prefill** — one parallel pass over the whole prompt. It grows with *prompt
  length*. Attention is O(n²) in sequence length, so it grows faster than linearly once the prompt is
  long.
- **TPOT is dominated by decode** — one sequential pass per token, memory-bandwidth-bound, largely
  independent of prompt length until the KV cache stops fitting in memory. The KV cache grows linearly
  with context, which is why TPOT degrades on long conversations and not on long single prompts.

A dashboard that shows only "p95 latency" cannot tell a long-prompt problem from a long-answer
problem, and those have opposite fixes. Splitting them is why every strange latency behaviour this
project produces becomes explicable rather than mysterious.

The other panels: cost per tenant per hour, error rate by provider and kind, circuit breaker state,
cache hit rate with the false-hit rate beside it, agent iterations histogram (a shifting tail means the
tools got worse), rate-limit denials by dimension.

Grafana dashboard JSON is committed to `ops/grafana/`, and a screenshot of it goes at the top of the
README. A dashboard nobody can see is evidence of nothing.

---

## 5. What gets an alert, and what does not

Alert on: error rate by provider, circuit breaker open for >5m, `sb_usage_records_dropped_total > 0`
(billing data is being lost), p99 TTFT beyond [the SLO](15-latency-and-p99.md#1-what-a-gateway-can-and-cannot-promise),
and a sustained non-zero `sb_requests_shed_total`. All of these exist in
[`ops/prometheus/alerts.yml`](../ops/prometheus/alerts.yml). A false-hit alert joins them the day a
semantic cache does.

Every latency alert here is scoped to `outcome="served"`, for the reason given in §3. A latency alert
that silently includes shed traffic is worse than no alert, because it will be green during the outage
it was written for.

Do not alert on: individual request failures, a single provider 429 (that is what the fallback is
for), or latency during a cold Ollama model load. An alert that fires on normal operation trains the
one person who reads it to ignore all the others.

And do not alert on p99 *total* latency for a model whose p99 is set by the provider. You did not cause
it and you cannot fix it at 3am. Alert on `sb_gateway_overhead_seconds`, which is yours, and put the
provider's tail on a dashboard where it belongs.
