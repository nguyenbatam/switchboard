-- RAG: documents + chunks with pgvector and full-text search

-- The `vector` type below (and 006's RAG policies) need pgvector loaded. A dev's
-- long-lived local DB may already have it from an earlier manual `CREATE EXTENSION`,
-- which is why this only ever failed on a fresh database — i.e. every CI run.
CREATE EXTENSION IF NOT EXISTS vector;

CREATE TABLE IF NOT EXISTS documents (
    id         uuid PRIMARY KEY,
    tenant_id  uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    source_uri text,
    sha256     bytea NOT NULL,
    mime       text NOT NULL DEFAULT 'text/plain',
    metadata   jsonb NOT NULL DEFAULT '{}',
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, sha256)
);

CREATE TABLE IF NOT EXISTS chunks (
    id           uuid PRIMARY KEY,
    document_id  uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    tenant_id    uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    ord          integer NOT NULL,
    content      text NOT NULL,
    token_count  smallint NOT NULL,
    embed_model  text NOT NULL,
    embedding    vector(768) NOT NULL,
    content_tsv  tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
    UNIQUE (document_id, ord)
);

CREATE INDEX IF NOT EXISTS chunks_hnsw ON chunks
    USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64);
CREATE INDEX IF NOT EXISTS chunks_fts ON chunks USING gin (content_tsv);
CREATE INDEX IF NOT EXISTS chunks_tenant ON chunks (tenant_id);

-- Eval run history (Phase 8)
CREATE TABLE IF NOT EXISTS eval_runs (
    id          uuid PRIMARY KEY,
    git_sha     text NOT NULL,
    suite       text NOT NULL,
    model       text NOT NULL,
    config      jsonb NOT NULL DEFAULT '{}',
    started_at  timestamptz NOT NULL DEFAULT now(),
    finished_at timestamptz
);

CREATE TABLE IF NOT EXISTS eval_results (
    run_id      uuid NOT NULL REFERENCES eval_runs(id) ON DELETE CASCADE,
    case_id     text NOT NULL,
    passed      boolean NOT NULL,
    score       real,
    latency_ms  integer,
    cost_nano   bigint,
    detail      jsonb,
    PRIMARY KEY (run_id, case_id)
);
