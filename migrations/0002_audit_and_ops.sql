CREATE TABLE IF NOT EXISTS memory_audit_log (
  id UUID PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  request_id TEXT,
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_audit_scope
ON memory_audit_log(tenant_id, entity_id, process_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_audit_request
ON memory_audit_log(request_id);
