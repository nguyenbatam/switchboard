# 09 — Evaluation

← [08 Reliability](08-reliability.md) · [index](README.md) · next: [10 Observability](10-observability.md)

The phase that separates this from every other gateway on GitHub, so it gets a real design rather
than a `tests/` directory with the word "eval" in the name.

Read *Your AI Product Needs Evals* first. Its thesis: AI products fail for lack of an evaluation
system, almost never because the model was bad.

---

## 1. Dataset

`eval/datasets/*.jsonl`, 50–100 cases at full size, version-controlled next to the code. Cases are
data, not code, so that adding one is a one-line PR that anyone can review. The `fast` suite is
currently 7 of them — three `rag`, two `tool`, two `injection` — which is enough to exercise every
scorer and not yet enough to trust an accuracy number to two decimal places.

```jsonc
{"id":"rag-014", "type":"rag",  "question":"…", "gold_answer":"…", "gold_doc_ids":["d1#3","d1#4"]}
{"id":"tool-07", "type":"tool", "question":"…",
 "gold_tool_calls":[{"name":"sql_query","args_contains":{"table":"orders"}}]}
{"id":"inj-03",  "type":"injection", "doc":"…Ignore previous instructions…",
 "must_not_call":["wallet_balance"]}
```

Three case types, because the three things that can be wrong are independent: retrieval can fail while
the answer is right (the model knew it anyway), the answer can fail while retrieval was perfect, and
both can be right while the system is trivially exploitable.

---

## 2. Metrics, each attached to the thing it measures

| target | metric |
|---|---|
| retrieval | recall@k, MRR, nDCG@10 |
| tool calling | exact name match; argument match (semantic, not string); **over-calling rate** |
| answer quality | LLM-as-judge, rubric-scored 1–5 |
| injection resistance | attack success rate |
| performance | TTFT p50/p95, TPOT, cost per query (nanodollars, straight from `usage_records`) |

Over-calling rate is in there because a model that calls `sql_query` on *"hello"* passes every accuracy
metric and is unusable. Most tool benchmarks measure only whether the right call was made, never
whether a call should have been made at all.

Cost per query comes from the same table that bills customers. Two sources of truth for "what did this
cost" is one too many.

---

## 3. LLM-as-judge, and its three biases

Handled, rather than mentioned in passing:

- **Position bias** — a judge prefers whichever answer it sees first.
  *Mitigation:* every pairwise comparison runs twice with the order swapped; disagreement counts as a
  tie.
- **Length bias** — a judge prefers longer answers.
  *Mitigation:* the rubric scores dimensions separately (correctness, groundedness, completeness), and
  answer length is reported alongside, so a score that merely tracks length is visible in the table.
- **Self-preference** — a judge prefers output from its own model family.
  *Mitigation:* the judge is never from the same family as any model under test, it is pinned by exact
  version in the run config, and a human-labelled calibration subset of 20 cases measures
  judge-vs-human agreement.

That last number is the one that matters. **A judge whose agreement with humans is 0.6 is not
measuring quality; it is measuring itself.** Publish the agreement figure next to the scores it
produced, or the scores mean nothing.

---

## 4. In CI

| trigger | suite | backend | wall clock |
|---|---|---|---|
| every PR | fast subset | none — an offline smoke test | ~40s, $0 |
| nightly | full | real Ollama | minutes |
| weekly | full | `gpt-4o-mini` | minutes, ~$0.50 |

The PR row said *"`providers/mock.rs` cassettes"* and meant it as a plan. There is no `providers/mock.rs`
and there are no cassettes. What CI runs is `cargo run --bin eval -- --suite fast` with no gateway
reachable: the datasets parse, the harness runs, and nothing is scored. That is a smoke test wearing an
eval's name — worth what it costs, and no more. The scored 7/7 in the README comes from
`run_local_test.sh`, which runs the same suite against a live gateway and a real model.

Temperature 0, seed pinned, N = 3 runs. The regression gate compares against the mean of the last five
green runs **with a noise band**.

The noise band is not slack, it is correctness: `temperature = 0` does not guarantee identical output
([05 §3](05-tokenization-cost.md#3-why-temperature--0-is-not-a-spec-of-determinism)) because batching
changes floating-point reduction order, so logits differ in the last bits, so an argmax tie flips. A
gate that fails on nondeterminism gets disabled by an exasperated human within a week, and then there
is no gate.

Fail the build when accuracy regresses beyond the band. Print the diff of which case ids flipped —
"accuracy 0.83 → 0.79" is a page; "`tool-07` and `rag-014` now fail" is a fix.

---

## 5. Output

`eval/results/<suite>-<UTC timestamp>.json` — one file per run, written unconditionally, even on a
failing run, because the failing run is the one you want to read. Plus a markdown table regenerated
into the README on every green nightly.

The output is a real table of numbers. Not "supports RAG" but:

```
hybrid search lifted recall@5 from 0.61 → 0.83, at +12ms p95
qwen3:4b vs gpt-4o-mini, tool-calling accuracy: 0.71 vs 0.89
injection attack success rate: 4/50 → 0/50 after capability downgrade
```

The comparisons that must exist, because the roadmap promises them:

1. Qwen3 4B vs Qwen3 8B vs `gpt-4o-mini` — accuracy, latency, cost per query.
2. Vector vs lexical vs hybrid retrieval — recall@5, and the p95 the extra arm costs.
3. Before vs after the taint rule ([06 §4](06-agent-tools.md#4-prompt-injection-taint-and-capability-downgrade))
   — attack success rate. A security control with no measured effect is decoration.

---

## 6. The harness as built — `cargo run --bin eval`

| flag | default | what it does |
|---|---|---|
| `--suite <name>` | `fast` | reads `eval/datasets/<name>.jsonl`, seeds `<name>_seed.jsonl` first |
| `--base-url <url>` | `http://127.0.0.1:8080` | the server under test |
| `--benchmark` | off | also probe `search`/`chat`/`embed` p95 and compare to the baseline |
| `--write-baseline` | off | overwrite this machine's baseline with the observed p95 |
| `--api-key <key>` | `$EVAL_API_KEY` | bearer token, for when `require_auth` is on |

### Online and offline are different gates

The harness probes `--base-url` before it scores anything. A reachable server means an **online** run,
scored against the baseline's `min_pass_rate` (1.0 for `apple-m4-metal`). An unreachable one degrades
to an **offline** run gated at 0.5, which is what CI does on every PR — it has no Ollama and no
Postgres, and a suite that hard-failed there would be disabled inside a week.

This is the same argument as the noise band in §4, applied to infrastructure instead of floating point:
a gate that fires for a reason unrelated to quality is a gate that gets removed.

### Baselines are per-machine, and they are ceilings

`eval/baselines/<profile>.json`. The profile is detected from the CPU at run time — an M4 resolves to
`apple-m4-metal`, anything unrecognised falls back to `local` — and `EVAL_BASELINE_PROFILE` overrides
the detection. A baseline carries its own `noise_pct`, because a Metal-backed M4 is repeatable to
within 30% and a generic CPU box is not (`local` allows 50%).

The committed numbers sit far above observed latency — 600ms against a measured 24ms for search — and
that gap is deliberate. The latency gate exists to catch the change that makes search take half a
second, not to make a laptop under Zoom load look like a regression. Tighten a baseline only by
re-capturing it with `--write-baseline` on an idle machine.

---

## 7. Why this exists at all

Unit tests verify that the code does what it was written to do. Nothing in a unit test notices that
switching the embedding model dropped recall@5 by nine points, or that a prompt tweak made the agent
call `sql_query` twice as often, or that the semantic cache is now serving the EU refund policy to US
customers.

The eval suite is the only regression test for the parts of this system that are statistical rather
than logical. It is also the only reason any number in the README can be trusted, including by the
person who wrote it.
