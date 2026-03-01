CREATE TABLE IF NOT EXISTS memory_llm_usage (
  id UUID PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  event_id TEXT,
  operation TEXT NOT NULL,
  provider TEXT NOT NULL,
  model TEXT NOT NULL,
  prompt_tokens INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0,
  total_tokens INTEGER NOT NULL DEFAULT 0,
  payload JSONB NOT NULL DEFAULT '{}'::jsonb,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_llm_usage_scope
ON memory_llm_usage(tenant_id, entity_id, process_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_llm_usage_event
ON memory_llm_usage(event_type, event_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_llm_usage_operation
ON memory_llm_usage(operation, model, created_at DESC);
