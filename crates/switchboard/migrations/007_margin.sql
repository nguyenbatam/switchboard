-- Revenue and cost of goods are different numbers, and a table that stores only one of
-- them cannot answer whether the business is profitable.
--
-- `cost_nano` was, and remains, what the tenant is billed. It is the only money column a
-- tenant is ever shown. `upstream_cost_nano` is what we paid the provider for the same
-- tokens. The difference is the margin, and it belongs to us.
--
-- Defaulting to 0 is safe and deliberately unflattering: rows written before this
-- migration report infinite margin, which is obviously wrong, rather than negative margin,
-- which is plausibly wrong and would poison a real decision.

ALTER TABLE usage_records
    ADD COLUMN IF NOT EXISTS upstream_cost_nano bigint NOT NULL DEFAULT 0;

ALTER TABLE usage_hourly
    ADD COLUMN IF NOT EXISTS upstream_cost_nano bigint NOT NULL DEFAULT 0;

COMMENT ON COLUMN usage_records.cost_nano IS
    'Revenue: nanodollars billed to the tenant, at the rate of the model that answered.';
COMMENT ON COLUMN usage_records.upstream_cost_nano IS
    'Cost of goods: nanodollars paid to the provider. Never exposed to a tenant.';

-- 005 granted table-wide SELECT on `usage_records` to the read-only tool role — the role
-- reachable from `sql_query`, which is to say from anything an attacker can write into a
-- prompt. That role must not be able to read our margin.
--
-- A column-level REVOKE cannot carve a hole in a table-level GRANT: PostgreSQL accepts it,
-- warns that no privileges could be revoked, and changes nothing. The privilege has to be
-- withdrawn at the table and re-granted per column.
REVOKE SELECT ON usage_records FROM switchboard_tool_ro;

GRANT SELECT (
    id, request_id, tenant_id, api_key_id, endpoint, model_alias, provider,
    served_by_alias, prompt_tokens, completion_tokens, cached_tokens, reasoning_tokens,
    cost_nano, ttft_ms, latency_ms, agent_iterations, finish_reason, status,
    error_kind, cache_hit, created_at
) ON usage_records TO switchboard_tool_ro;
