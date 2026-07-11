# 03 — Database

← [02 Providers](02-providers.md) · [index](README.md) · next: [04 Redis & rate limiting](04-redis-ratelimit.md)

Conventions from [00](00-overview.md), repeated because they matter here: money is `bigint`
nanodollars, timestamps are `timestamptz`, ids are UUIDv7.

---

## 1. Schema

### Tenancy

```sql
CREATE TABLE tenants (
    id                  uuid PRIMARY KEY,
    name                text NOT NULL,
    status              text NOT NULL DEFAULT 'active'
                        CHECK (status IN ('active','suspended')),
    monthly_budget_nano bigint,                       -- NULL = unlimited
    rpm_limit           integer NOT NULL DEFAULT 60,
    tpm_limit           integer NOT NULL DEFAULT 60000,
    created_at          timestamptz NOT NULL DEFAULT now()
);

-- Key format: sk-sb-<prefix:8>.<secret:32>
-- The prefix is indexed and is not a secret. The secret half is never stored.
CREATE TABLE api_keys (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    key_prefix   text NOT NULL UNIQUE,                -- lookup key
    key_hash     text NOT NULL,                       -- argon2id of the secret half
    name         text NOT NULL,
    scopes       text[] NOT NULL DEFAULT '{chat,embeddings}',
    last_used_at timestamptz,
    expires_at   timestamptz,
    revoked_at   timestamptz
);
CREATE INDEX ON api_keys (tenant_id) WHERE revoked_at IS NULL;
```

The prefix/secret split exists so that authentication is one indexed lookup plus one argon2 verify,
rather than an argon2 verify against every row. Argon2id is deliberately slow; a scheme that requires
N of them per request is a self-inflicted denial of service.

`last_used_at` is written at most once a minute per key, from the usage writer task, never on the hot
path.

### Usage and cost

```sql
-- Monthly range partitions. Usage is append-only, queried by time, and dropped
-- by time; that is the exact shape declarative partitioning was built for.
CREATE TABLE usage_records (
    id                uuid        NOT NULL,
    request_id        text        NOT NULL,
    tenant_id         uuid        NOT NULL,
    api_key_id        uuid,
    endpoint          text        NOT NULL,          -- chat|embeddings|search
    model_alias       text        NOT NULL,          -- what was requested
    provider          text        NOT NULL,
    served_by_alias   text        NOT NULL,          -- what actually answered (fallback!)
    prompt_tokens     integer     NOT NULL DEFAULT 0,
    completion_tokens integer     NOT NULL DEFAULT 0,
    cached_tokens     integer     NOT NULL DEFAULT 0,
    reasoning_tokens  integer     NOT NULL DEFAULT 0,
    cost_nano         bigint      NOT NULL DEFAULT 0,
    ttft_ms           integer,
    latency_ms        integer     NOT NULL,
    agent_iterations  smallint    NOT NULL DEFAULT 0,
    finish_reason     text,
    status            text        NOT NULL
                      CHECK (status IN ('completed','client_disconnect','error')),
    error_kind        text,
    cache_hit         text,                          -- NULL | exact | semantic
    created_at        timestamptz NOT NULL DEFAULT now()
) PARTITION BY RANGE (created_at);

CREATE INDEX ON usage_records USING brin (created_at);   -- append-only ⇒ BRIN, ~1/1000th of a btree
CREATE INDEX ON usage_records (tenant_id, created_at DESC);

-- /v1/usage must not scan the fact table. A writer task maintains the rollup.
CREATE TABLE usage_hourly (
    tenant_id         uuid NOT NULL,
    hour              timestamptz NOT NULL,
    model_alias       text NOT NULL,
    requests          bigint NOT NULL DEFAULT 0,
    prompt_tokens     bigint NOT NULL DEFAULT 0,
    completion_tokens bigint NOT NULL DEFAULT 0,
    cost_nano         bigint NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, hour, model_alias)
);
```

`model_alias` and `served_by_alias` are both stored so that "what fraction of last Tuesday's traffic
was served by the fallback model" is answerable. See [08 §3](08-reliability.md#3-fallback--and-the-semantics-question-answered).

### Conversations (optional persistence, off by default)

```sql
CREATE TABLE conversations (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL REFERENCES tenants(id),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE messages (
    id uuid PRIMARY KEY,
    conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    ord smallint NOT NULL, role text NOT NULL, content jsonb NOT NULL,
    tool_calls jsonb, tool_call_id text,
    UNIQUE (conversation_id, ord)
);
```

### The agent's audit trail

Non-negotiable: **a tool loop you cannot replay is a tool loop you cannot debug at 2am.**

```sql
CREATE TABLE tool_invocations (
    id uuid PRIMARY KEY,
    request_id text NOT NULL,
    tenant_id  uuid NOT NULL,
    iteration  smallint NOT NULL,
    tool_name  text NOT NULL,
    arguments  jsonb NOT NULL,
    result     jsonb,                     -- truncated to 8KB; the full blob lives in the trace
    trust      text NOT NULL,             -- trusted | untrusted
    latency_ms integer NOT NULL,
    status     text NOT NULL,             -- ok | invalid_args | unknown_tool | error | timeout
    error      text,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON tool_invocations (request_id);
```

`trust` is recorded on every row so that "which requests were tainted, and what did they do
afterwards" is one SQL query. See [06 §4](06-agent-tools.md#4-prompt-injection-taint-and-capability-downgrade).

### RAG

```sql
CREATE TABLE documents (
    id         uuid PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source_uri text,
    sha256     bytea NOT NULL,
    mime       text NOT NULL DEFAULT 'text/plain',
    metadata   jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, sha256)   -- re-uploading the same bytes is a no-op, not a duplicate
);

CREATE TABLE chunks (
    id           uuid PRIMARY KEY,
    document_id  uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    tenant_id    uuid NOT NULL,           -- denormalised: every search filters on it
    ord          integer NOT NULL,
    content      text NOT NULL,
    token_count  smallint NOT NULL,
    embed_model  text NOT NULL,
    embedding    halfvec(768) NOT NULL,   -- fp16: half the index, ~0 recall loss at this dimension
    content_tsv  tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    UNIQUE (document_id, ord)
);

CREATE INDEX chunks_hnsw ON chunks
    USING hnsw (embedding halfvec_cosine_ops) WITH (m = 16, ef_construction = 64);
CREATE INDEX chunks_fts    ON chunks USING gin (content_tsv);
CREATE INDEX chunks_tenant ON chunks (tenant_id);
```

`embed_model` is stored on every row so a model change is a detectable, backfillable event rather than
a silent corruption of the index — half the rows in one vector space and half in another produces
plausible, wrong results with no error anywhere.

### Semantic cache

Lives in pgvector, not Redis, because it needs ANN.

```sql
CREATE TABLE semantic_cache (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,            -- NEVER shared across tenants. This is a leak vector.
    prefix_hash bytea NOT NULL,           -- exact hash of all turns EXCEPT the last user turn
    params_hash bytea NOT NULL,           -- model + temperature + top_p + tools + response_format
    query_embedding halfvec(768) NOT NULL,
    response    jsonb NOT NULL,
    hits        integer NOT NULL DEFAULT 0,
    created_at  timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX ON semantic_cache USING hnsw (query_embedding halfvec_cosine_ops);
CREATE INDEX ON semantic_cache (tenant_id, prefix_hash, params_hash);
```

### Eval

```sql
CREATE TABLE eval_runs (
    id uuid PRIMARY KEY, git_sha text NOT NULL, suite text NOT NULL,
    model text NOT NULL, config jsonb NOT NULL,
    started_at timestamptz NOT NULL, finished_at timestamptz
);
CREATE TABLE eval_results (
    run_id uuid NOT NULL REFERENCES eval_runs(id) ON DELETE CASCADE,
    case_id text NOT NULL,
    passed boolean NOT NULL, score real,
    latency_ms integer, cost_nano bigint,
    detail jsonb,
    PRIMARY KEY (run_id, case_id)
);
```

---

## 2. Filtered vector search — the quiet recall killer

```sql
SELECT ... FROM chunks WHERE tenant_id = $1 ORDER BY embedding <=> $2 LIMIT 10;
```

This is the query that quietly destroys recall: HNSW walks the graph globally, then the filter throws
most of the results away, and the ten rows that come back are the ten *survivors*, not the ten
nearest. pgvector 0.8's iterative scan is the fix, and it must be set per session on the RAG pool:

```sql
SET hnsw.iterative_scan = relaxed_order;
SET hnsw.ef_search = 100;
SET hnsw.max_scan_tuples = 20000;
```

If a single tenant ever grows past a few million chunks, the answer is partitioning `chunks` by
`tenant_id` — one HNSW index per partition — not a bigger `ef_search`. Not now.

---

## 3. Row-level security, and why it exists even though the app filters too

Applied to every table carrying a `tenant_id`: `chunks`, `documents`, `conversations`, `messages`,
`tool_invocations`, `usage_records`, `semantic_cache`.

```sql
ALTER TABLE chunks ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunks FORCE  ROW LEVEL SECURITY;   -- see below. Without this line, none of it runs.

CREATE POLICY tenant_isolation ON chunks
    USING (tenant_id = current_setting('app.tenant_id', true)::uuid);
```

The app already writes `WHERE tenant_id = $1` everywhere. RLS is the second lock: the day someone
writes a query that forgets the clause — and someone will, probably in the eval harness or a
migration script — RLS turns a cross-tenant data leak into an empty result set.

The gateway issues `SET LOCAL app.tenant_id = $1` at the top of every request transaction. `SET LOCAL`
is transaction-scoped, which is what makes it safe under connection pooling. `SET` is not, and using
it here would hand one tenant's rows to the next request that borrowed the same connection.

### Two ways to write this and have it do nothing

Both are silent. Neither fails a test, because the app's own `WHERE tenant_id = $1` hides the fact
that the second lock was never installed.

**`ENABLE` does not apply to the table's owner.** The `app` pool connects as `switchboard`, which is
the role that ran the migrations and therefore owns every table — and a table owner bypasses its own
RLS policies. `ENABLE ROW LEVEL SECURITY` alone protects the gateway against everyone except the
gateway. `FORCE ROW LEVEL SECURITY` is the line that subjects the owner to the policy too, and it is
the only line here that does any work.

**`current_setting('app.tenant_id')` raises on an unset GUC**, so a background job that never issued
`SET LOCAL` — the usage writer, `sbctl`, a migration — dies with an error instead of seeing zero rows.
The three-argument form returns `NULL` instead, `tenant_id = NULL` is `NULL`, and the policy denies.
Fail closed, quietly, which is the behaviour a second lock is supposed to have.

---

## 4. Two pools, two roles

| pool | role | grants | session settings |
|---|---|---|---|
| `app` | `switchboard` | full DML on the app schema | `statement_timeout = 5s` |
| `tool_ro` | `switchboard_tool_ro` | `SELECT` on schema `tool_ro` only | `default_transaction_read_only = on`, `statement_timeout = 2s`, `idle_in_transaction_session_timeout = 5s` |

The `sql_query` tool never touches the `app` pool. Even if the AST allowlist in
[06 §2](06-agent-tools.md#2-sql_query--a-read-only-tool-that-assumes-it-will-be-attacked) is defeated
entirely, the worst a compromised prompt achieves is a read of the views deliberately exposed in
`tool_ro`.

Defense in depth means the model must beat two independent controls, one of which is enforced by
Postgres and not by us.

---

## 5. Migrations

`sqlx migrate`, forward-only, applied at boot inside `pg_advisory_lock(0x5742)` so that N replicas
starting simultaneously do not race.

`cargo sqlx prepare` commits the query metadata, so CI typechecks every SQL string at compile time
without a live database.

Partition maintenance (`usage_records_2026_08`, …) runs from `sbctl`, a month ahead, from a cron. A
partition that does not exist when the month rolls over is an outage, and it is an outage at
midnight.
