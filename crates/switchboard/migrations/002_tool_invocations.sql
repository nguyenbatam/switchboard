CREATE TABLE IF NOT EXISTS tool_invocations (
    id          uuid PRIMARY KEY,
    request_id  text NOT NULL,
    tenant_id   uuid NOT NULL REFERENCES tenants(id),
    iteration   smallint NOT NULL,
    tool_name   text NOT NULL,
    arguments   jsonb NOT NULL,
    result      jsonb,
    trust       text NOT NULL CHECK (trust IN ('trusted', 'untrusted')),
    latency_ms  integer NOT NULL,
    status      text NOT NULL,
    error       text,
    created_at  timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS tool_invocations_request_id_idx
    ON tool_invocations (request_id);
