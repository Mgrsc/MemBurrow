CREATE TABLE IF NOT EXISTS memory_item (
  id UUID PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  session_id TEXT,
  memory_type TEXT NOT NULL,
  category TEXT NOT NULL,
  content TEXT NOT NULL,
  normalized_content TEXT NOT NULL,
  importance SMALLINT NOT NULL DEFAULT 50,
  confidence NUMERIC(4,3) NOT NULL DEFAULT 0.500,
  source TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  mention_count INTEGER NOT NULL DEFAULT 1,
  fingerprint_hash TEXT NOT NULL,
  first_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  expires_at TIMESTAMPTZ,
  version INTEGER NOT NULL DEFAULT 1,
  properties JSONB NOT NULL DEFAULT '{}'::jsonb,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT chk_memory_importance CHECK (importance BETWEEN 0 AND 100),
  CONSTRAINT chk_memory_confidence CHECK (confidence BETWEEN 0 AND 1)
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_memory_fingerprint
ON memory_item(tenant_id, entity_id, process_id, memory_type, fingerprint_hash);

CREATE INDEX IF NOT EXISTS idx_memory_scope
ON memory_item(tenant_id, entity_id, process_id, memory_type, status, last_seen_at DESC);

CREATE INDEX IF NOT EXISTS idx_memory_expire
ON memory_item(tenant_id, expires_at);

CREATE INDEX IF NOT EXISTS idx_memory_properties_gin
ON memory_item USING GIN(properties);

CREATE TABLE IF NOT EXISTS memory_event (
  id UUID PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  session_id TEXT,
  turn_id TEXT,
  role TEXT NOT NULL,
  payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_event_scope
ON memory_event(tenant_id, entity_id, process_id, created_at DESC);

CREATE TABLE IF NOT EXISTS memory_outbox (
  id UUID PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  aggregate_id UUID NOT NULL,
  idempotency_key TEXT NOT NULL,
  payload JSONB NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending',
  retry_count INTEGER NOT NULL DEFAULT 0,
  next_retry_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_error TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  CONSTRAINT chk_outbox_state CHECK (state IN ('pending', 'processing', 'done', 'dead'))
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_outbox_idempotency
ON memory_outbox(idempotency_key);

CREATE INDEX IF NOT EXISTS idx_outbox_poll
ON memory_outbox(state, next_retry_at, created_at);

CREATE TABLE IF NOT EXISTS memory_feedback (
  id UUID PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  turn_id TEXT,
  used_items JSONB NOT NULL DEFAULT '[]'::jsonb,
  helpful JSONB NOT NULL DEFAULT '[]'::jsonb,
  harmful JSONB NOT NULL DEFAULT '[]'::jsonb,
  note TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_feedback_scope
ON memory_feedback(tenant_id, entity_id, process_id, created_at DESC);

CREATE TABLE IF NOT EXISTS memory_embedding (
  memory_id UUID PRIMARY KEY REFERENCES memory_item(id) ON DELETE CASCADE,
  tenant_id TEXT NOT NULL,
  model TEXT NOT NULL,
  dims INTEGER NOT NULL,
  embedding JSONB NOT NULL,
  recall_text TEXT NOT NULL,
  metadata JSONB NOT NULL DEFAULT '{}'::jsonb,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_embedding_scope
ON memory_embedding(tenant_id, model, updated_at DESC);
