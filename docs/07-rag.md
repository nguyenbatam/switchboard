# 07 — RAG

← [06 Agent & tools](06-agent-tools.md) · [index](README.md) · next: [08 Reliability](08-reliability.md)

---

## 1. Ingestion

```
parse → normalise → chunk → dedupe(sha256) → embed(batch) → store
```

### Chunking decides quality more than the embedding model does

Recursive structural splitting: split on markdown headings, then paragraphs, then sentences, packing
greedily to ~512 tokens with 64 tokens of overlap.

Hard rules, each learned the same way:

- **Never split inside a code fence.** Half a function retrieved is worse than no function.
- **Never split a table row from its header.** The row is meaningless alone and the model will not say
  so.
- **Always prepend the document title and the heading path** — `# Guide > ## Auth > ### Rotation` — to
  every chunk. A chunk retrieved without its context is a chunk the model misreads, confidently.

Chunk parameters are config, and Phase 8 measures them rather than assuming them. "512 with 64
overlap" is a starting point, not a finding.

### Embedding

`nomic-embed-text` requires **task prefixes**: `search_document: ` when indexing, `search_query: `
when searching. Getting this backwards silently costs several points of recall and produces no error
anywhere — the vectors are still vectors, they are just in the wrong region of the space.

The prefix therefore belongs in `rag/embed.rs`, applied by the code, never by the caller.

Batch 64 chunks per call. Store `embed_model` on every row ([03](03-database.md#rag)) so a model
change is a detectable, backfillable event rather than a silent corruption of the index.

---

## 2. Retrieval: hybrid, fused with RRF

Vector search finds *"how do I rotate credentials"* → *"key rotation procedure"*. Lexical search finds
the exact error code `SB-4021` that no embedding has ever seen. Real queries contain both, so run
both.

```sql
WITH vec AS (
  SELECT id, row_number() OVER (ORDER BY embedding <=> $1) AS rank
  FROM chunks WHERE tenant_id = $2 ORDER BY embedding <=> $1 LIMIT 50
),
fts AS (
  SELECT id, row_number() OVER (ORDER BY ts_rank_cd(content_tsv, q) DESC) AS rank
  FROM chunks, websearch_to_tsquery('english', $3) q
  WHERE tenant_id = $2 AND content_tsv @@ q ORDER BY 2 LIMIT 50
)
SELECT id, COALESCE(1.0/(60 + vec.rank), 0) + COALESCE(1.0/(60 + fts.rank), 0) AS rrf
FROM vec FULL OUTER JOIN fts USING (id) ORDER BY rrf DESC LIMIT $4;
```

### Reciprocal Rank Fusion, not score normalisation

A cosine distance of 0.31 and a `ts_rank_cd` of 0.089 are not commensurable. Min-max normalising them
makes the fusion depend on the score distribution of whatever happened to be in the candidate set —
add one outlier document and every ranking shifts.

RRF uses only ranks, so it cannot be perturbed that way. `k = 60` is the value from the original
paper; it is a knob, and [09](09-eval.md) measures it rather than assuming it.

### An honesty note that belongs in the README too

**`ts_rank_cd` is not BM25.** Postgres full-text search lacks true document-length normalisation and
IDF saturation. It is a good lexical signal and a bad BM25.

If the measurement in Phase 8 shows the lexical arm underperforming, the fix is `VectorChord-BM25` or
`pg_search` — not a bigger embedding model. Naming the weakness now is what makes it possible to test
the right hypothesis later.

---

## 3. Context assembly

Two steps after fusion, both cheap, both worth more than most model upgrades.

### MMR

Maximal marginal relevance, λ = 0.7, to drop near-duplicate chunks. Three copies of the same paragraph
waste the context that a second perspective could have used.

### Ordering: lost in the middle

Models attend to the head and tail of a long context far better than the middle
([arXiv:2307.03172](https://arxiv.org/abs/2307.03172)). So chunks go in as:

```
[1st, 3rd, 5th, …, 6th, 4th, 2nd]
```

Best-scoring at both edges, worst buried in the middle. Two lines in `rag/context.rs`.

### Citations and trust

Every injected chunk carries a citation id (`doc:a1b2#3`), returned alongside the answer so a caller
can verify it.

Every retrieved chunk is `Untrusted`, which taints the request and downgrades its tool capabilities.
That rule lives in [06 §4](06-agent-tools.md#4-prompt-injection-taint-and-capability-downgrade) and is
the reason a RAG document containing *"ignore previous instructions"* cannot reach `wallet_balance`.

---

## 4. The endpoints

| endpoint | shape |
|---|---|
| `POST /v1/documents` | upload → chunk → embed → store. Returns `{id, chunks}`. Idempotent on `sha256`. |
| `GET/DELETE /v1/documents/{id}` | cascade-deletes chunks |
| `POST /v1/search` | `{query, k, mode: vector\|lexical\|hybrid}` → chunks with scores and citations. Ours, not OpenAI's. |
| `POST /v1/embeddings` | OpenAI-compatible, including the `input: string \| string[] \| int[][]` union and `encoding_format: float \| base64` |

`mode` on `/v1/search` exists so that the Phase 8 comparison — vector vs lexical vs hybrid — is a
parameter of the running system, not a branch someone has to uncomment.

Embeddings deserve one warning: the OpenAI `input` field accepts pre-tokenised integer arrays. Most
compatible gateways forget this and fail on a client that uses it. The union is in `domain::EmbedRequest`
from the start.

---

## 5. What is deliberately not built

- **A reranker.** A cross-encoder pass would probably help. It is not built until Phase 8 produces a
  number showing where the recall is being lost, because a reranker fixes a precision problem and the
  measurement may find a recall problem.
- **Parent-document retrieval, sentence-window retrieval, graph RAG.** Each is a reasonable idea. Each
  is unfalsifiable without the eval suite. The suite comes first.

This is the roadmap's "building wide instead of deep" failure mode, and RAG is where it is most
tempting.
