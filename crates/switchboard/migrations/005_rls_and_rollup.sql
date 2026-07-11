-- Tenant isolation in the database, and the usage rollup /v1/usage reads.
--
-- Until now the only thing keeping one tenant's rows away from another was a
-- `WHERE tenant_id = $1` in application code. That is one lock. The tool_ro views
-- below had no such clause at all, so `sql_query` -- an Untrusted, model-driven
-- tool -- could read every tenant's usage with `SELECT * FROM usage_by_model`.

-- ── Rollup ────────────────────────────────────────────────────────────────────
-- /v1/usage and the monthly budget counter both aggregate spend. Neither should
-- scan the fact table to do it.

CREATE TABLE IF NOT EXISTS usage_hourly (
    tenant_id         uuid        NOT NULL REFERENCES tenants(id),
    hour              timestamptz NOT NULL,
    model_alias       text        NOT NULL,
    requests          bigint      NOT NULL DEFAULT 0,
    prompt_tokens     bigint      NOT NULL DEFAULT 0,
    completion_tokens bigint      NOT NULL DEFAULT 0,
    cost_nano         bigint      NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, hour, model_alias)
);

CREATE INDEX IF NOT EXISTS usage_hourly_tenant_hour_idx
    ON usage_hourly (tenant_id, hour DESC);

-- Backfill from whatever history already exists, so upgrading does not reset
-- every tenant's month-to-date spend to zero.
INSERT INTO usage_hourly (
    tenant_id, hour, model_alias, requests, prompt_tokens, completion_tokens, cost_nano
)
SELECT
    tenant_id,
    date_trunc('hour', created_at),
    model_alias,
    COUNT(*),
    COALESCE(SUM(prompt_tokens), 0),
    COALESCE(SUM(completion_tokens), 0),
    COALESCE(SUM(cost_nano), 0)
FROM usage_records
GROUP BY tenant_id, date_trunc('hour', created_at), model_alias
ON CONFLICT (tenant_id, hour, model_alias) DO NOTHING;

-- `created_at` is append-only and physically correlated with insertion order, so
-- BRIN indexes it in kilobytes where a btree would take megabytes.
CREATE INDEX IF NOT EXISTS usage_records_created_brin_idx
    ON usage_records USING brin (created_at);

-- ── Row-level security ────────────────────────────────────────────────────────
-- Policies read `app.tenant_id`, set per transaction by the caller. The app role
-- owns these tables and is therefore exempt unless FORCEd; we deliberately do not
-- force it, because the auth path must look a key up *before* it knows which
-- tenant it belongs to. The role that RLS actually constrains is the one holding
-- attacker-reachable input: `switchboard_tool_ro`.

CREATE OR REPLACE FUNCTION current_tenant_id() RETURNS uuid
LANGUAGE sql STABLE AS $$
    -- `true` = missing_ok. Unset yields NULL, and `tenant_id = NULL` matches no
    -- rows, so a caller that forgets to scope itself sees nothing at all.
    SELECT NULLIF(current_setting('app.tenant_id', true), '')::uuid
$$;

ALTER TABLE usage_records   ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_hourly    ENABLE ROW LEVEL SECURITY;
ALTER TABLE tenants         ENABLE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON usage_records;
CREATE POLICY tenant_isolation ON usage_records
    FOR SELECT USING (tenant_id = current_tenant_id());

DROP POLICY IF EXISTS tenant_isolation ON usage_hourly;
CREATE POLICY tenant_isolation ON usage_hourly
    FOR SELECT USING (tenant_id = current_tenant_id());

DROP POLICY IF EXISTS tenant_isolation ON tenants;
CREATE POLICY tenant_isolation ON tenants
    FOR SELECT USING (id = current_tenant_id());

DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_tables WHERE tablename = 'tool_invocations') THEN
        EXECUTE 'ALTER TABLE tool_invocations ENABLE ROW LEVEL SECURITY';
        EXECUTE 'DROP POLICY IF EXISTS tenant_isolation ON tool_invocations';
        EXECUTE 'CREATE POLICY tenant_isolation ON tool_invocations
                 FOR SELECT USING (tenant_id = current_tenant_id())';
    END IF;
END
$$;

-- ── Make the tool_ro views honour it ──────────────────────────────────────────
-- A view runs as its owner by default, which would bypass the policies above
-- entirely. `security_invoker` makes it run as `switchboard_tool_ro`, so RLS on
-- the underlying tables applies to whatever the model asked for.

CREATE OR REPLACE VIEW tool_ro.tenants_public
WITH (security_invoker = true) AS
SELECT id, name, status, rpm_limit, tpm_limit, created_at
FROM tenants;

CREATE OR REPLACE VIEW tool_ro.usage_by_model
WITH (security_invoker = true) AS
SELECT
    tenant_id,
    model_alias,
    COUNT(*)::bigint AS requests,
    COALESCE(SUM(prompt_tokens), 0)::bigint AS prompt_tokens,
    COALESCE(SUM(completion_tokens), 0)::bigint AS completion_tokens,
    COALESCE(SUM(cost_nano), 0)::bigint AS cost_nano
FROM usage_records
GROUP BY tenant_id, model_alias;

GRANT SELECT ON ALL TABLES IN SCHEMA tool_ro TO switchboard_tool_ro;
-- The invoker needs to reach the base tables for `security_invoker` to be able to
-- filter them; RLS, not GRANT, is what limits which rows it sees.
GRANT SELECT ON tenants, usage_records TO switchboard_tool_ro;
GRANT EXECUTE ON FUNCTION current_tenant_id() TO switchboard_tool_ro;
