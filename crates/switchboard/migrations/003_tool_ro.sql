CREATE SCHEMA IF NOT EXISTS tool_ro;

CREATE OR REPLACE VIEW tool_ro.tenants_public AS
SELECT id, name, status, rpm_limit, tpm_limit, created_at
FROM tenants;

CREATE OR REPLACE VIEW tool_ro.usage_by_model AS
SELECT
    tenant_id,
    model_alias,
    COUNT(*)::bigint AS requests,
    COALESCE(SUM(prompt_tokens), 0)::bigint AS prompt_tokens,
    COALESCE(SUM(completion_tokens), 0)::bigint AS completion_tokens,
    COALESCE(SUM(cost_nano), 0)::bigint AS cost_nano
FROM usage_records
GROUP BY tenant_id, model_alias;

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'switchboard_tool_ro') THEN
        CREATE ROLE switchboard_tool_ro LOGIN PASSWORD 'switchboard_ro';
    END IF;
END
$$;

GRANT USAGE ON SCHEMA tool_ro TO switchboard_tool_ro;
GRANT SELECT ON ALL TABLES IN SCHEMA tool_ro TO switchboard_tool_ro;
ALTER DEFAULT PRIVILEGES IN SCHEMA tool_ro GRANT SELECT ON TABLES TO switchboard_tool_ro;

ALTER ROLE switchboard_tool_ro SET default_transaction_read_only = on;
ALTER ROLE switchboard_tool_ro SET statement_timeout = '2s';
ALTER ROLE switchboard_tool_ro SET idle_in_transaction_session_timeout = '5s';
