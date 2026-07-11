-- Row-level security on the retrieval tables.
--
-- Nothing reaches these through `tool_ro` today, so this closes no live hole. It
-- closes the *next* one: the moment somebody adds a `tool_ro.documents` view for
-- the model to search, the isolation has to already be in the table, not in the
-- reviewer's memory of which views are safe.
--
-- As in 005, RLS is enabled but not FORCEd: the app role owns these tables and
-- bypasses the policy, which is what lets ingestion write rows for a tenant whose
-- `app.tenant_id` was never set on that connection.

ALTER TABLE documents ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunks    ENABLE ROW LEVEL SECURITY;

DROP POLICY IF EXISTS tenant_isolation ON documents;
CREATE POLICY tenant_isolation ON documents
    FOR SELECT USING (tenant_id = current_tenant_id());

DROP POLICY IF EXISTS tenant_isolation ON chunks;
CREATE POLICY tenant_isolation ON chunks
    FOR SELECT USING (tenant_id = current_tenant_id());

-- `last_used_at` is written at most once a minute per key, and read when deciding
-- whether a key is dead enough to revoke. Index the question that gets asked.
CREATE INDEX IF NOT EXISTS api_keys_last_used_idx
    ON api_keys (last_used_at NULLS FIRST)
    WHERE revoked_at IS NULL;
