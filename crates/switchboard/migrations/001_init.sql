-- Tenancy
CREATE TABLE IF NOT EXISTS tenants (
    id                  uuid PRIMARY KEY,
    name                text NOT NULL,
    status              text NOT NULL DEFAULT 'active'
                        CHECK (status IN ('active','suspended')),
    monthly_budget_nano bigint,
    rpm_limit           integer NOT NULL DEFAULT 60,
    tpm_limit           integer NOT NULL DEFAULT 60000,
    created_at          timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS api_keys (
    id           uuid PRIMARY KEY,
    tenant_id    uuid NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    key_prefix   text NOT NULL UNIQUE,
    key_hash     text NOT NULL,
    name         text NOT NULL,
    scopes       text[] NOT NULL DEFAULT '{chat,embeddings}',
    last_used_at timestamptz,
    expires_at   timestamptz,
    revoked_at   timestamptz
);

CREATE INDEX IF NOT EXISTS api_keys_tenant_active_idx
    ON api_keys (tenant_id) WHERE revoked_at IS NULL;

-- Usage (non-partitioned for Phase 3; partition in sbctl later)
CREATE TABLE IF NOT EXISTS usage_records (
    id                uuid PRIMARY KEY,
    request_id        text NOT NULL,
    tenant_id         uuid NOT NULL REFERENCES tenants(id),
    api_key_id        uuid REFERENCES api_keys(id),
    endpoint          text NOT NULL,
    model_alias       text NOT NULL,
    provider          text NOT NULL,
    served_by_alias   text NOT NULL,
    prompt_tokens     integer NOT NULL DEFAULT 0,
    completion_tokens integer NOT NULL DEFAULT 0,
    cached_tokens     integer NOT NULL DEFAULT 0,
    reasoning_tokens  integer NOT NULL DEFAULT 0,
    cost_nano         bigint NOT NULL DEFAULT 0,
    ttft_ms           integer,
    latency_ms        integer NOT NULL DEFAULT 0,
    agent_iterations  smallint NOT NULL DEFAULT 0,
    finish_reason     text,
    status            text NOT NULL
                      CHECK (status IN ('completed','client_disconnect','error')),
    error_kind        text,
    cache_hit         text,
    created_at        timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS usage_records_tenant_created_idx
    ON usage_records (tenant_id, created_at DESC);

-- Dev seed tenant
INSERT INTO tenants (id, name)
VALUES ('00000000-0000-7000-8000-000000000001', 'dev')
ON CONFLICT (id) DO NOTHING;
