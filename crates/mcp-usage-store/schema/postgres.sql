CREATE TABLE IF NOT EXISTS mcp_usage_task_attribution (
    tenant_hash BYTEA NOT NULL,
    task_hash BYTEA NOT NULL,
    method TEXT NOT NULL,
    name TEXT,
    expires_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (tenant_hash, task_hash)
);

CREATE INDEX IF NOT EXISTS mcp_usage_task_attribution_expiry_idx
    ON mcp_usage_task_attribution (expires_at);
