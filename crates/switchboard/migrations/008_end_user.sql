-- Attribution below the tenant.
--
-- A tenant is who pays us. It is not who *caused* the spend. A bot fleet serving ten
-- thousand end users is one tenant, one API key, and one undifferentiated invoice — so the
-- partner who wants to re-bill their own users, or find the one user whose runaway loop
-- burned the month's budget on a Tuesday, cannot. They have exactly the problem
-- `/v1/usage/records` exists to solve, one level down, and no column to solve it with.
--
-- OpenAI's request body already carries `user` for this. We record it.
--
-- `text`, not `uuid`: it is the *partner's* identifier for their own user, and we have no
-- business telling them what shape it is. Length is capped in the handler rather than here,
-- so that an over-long value is a `400` a client can fix and not a `500` from the database.
ALTER TABLE usage_records
    ADD COLUMN IF NOT EXISTS end_user_id text;

COMMENT ON COLUMN usage_records.end_user_id IS
    'The caller''s own identifier for the end user, from the request body''s `user` field. '
    'Opaque to us. Never a metric label: it is unbounded cardinality by construction.';

-- The query this exists to serve: "one tenant's rows, for one of their users, newest
-- first". Partial, because rows without a `user` are the common case for a tenant that
-- does not use the feature, and they do not belong in this index.
CREATE INDEX IF NOT EXISTS usage_records_tenant_end_user_idx
    ON usage_records (tenant_id, end_user_id, id DESC)
    WHERE end_user_id IS NOT NULL;

-- Deliberately NOT granted to `switchboard_tool_ro`.
--
-- Migration 007 replaced that role's table-wide SELECT with an explicit column list,
-- precisely so that a new column is invisible to it by default rather than exposed by
-- default. `end_user_id` is a partner's user identifier — reachable from `sql_query`
-- means reachable from anything an attacker can write into a prompt.
--
-- This comment is the whole reason 007 chose an allowlist over a denylist. Nothing to do
-- here is the correct amount of work.
