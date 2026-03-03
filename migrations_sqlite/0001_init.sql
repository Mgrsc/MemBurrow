CREATE TABLE IF NOT EXISTS memory_item (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  namespace TEXT NOT NULL,
  session_id TEXT,
  memory_type TEXT NOT NULL,
  category TEXT NOT NULL,
  content TEXT NOT NULL,
  normalized_content TEXT NOT NULL,
  importance INTEGER NOT NULL DEFAULT 50 CHECK (importance BETWEEN 0 AND 100),
  confidence REAL NOT NULL DEFAULT 0.5 CHECK (confidence BETWEEN 0 AND 1),
  source TEXT NOT NULL,
  status TEXT NOT NULL DEFAULT 'active',
  mention_count INTEGER NOT NULL DEFAULT 1,
  fingerprint_hash TEXT NOT NULL,
  first_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  last_seen_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  expires_at TEXT,
  version INTEGER NOT NULL DEFAULT 1,
  properties TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_memory_namespace_fingerprint
ON memory_item(namespace, memory_type, fingerprint_hash);

CREATE INDEX IF NOT EXISTS idx_memory_namespace_scope
ON memory_item(namespace, memory_type, status, last_seen_at DESC);

CREATE INDEX IF NOT EXISTS idx_memory_namespace_expire
ON memory_item(namespace, expires_at);

CREATE TABLE IF NOT EXISTS memory_event (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  session_id TEXT,
  turn_id TEXT,
  role TEXT NOT NULL,
  payload TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_event_scope
ON memory_event(tenant_id, entity_id, process_id, created_at DESC);

CREATE TABLE IF NOT EXISTS memory_outbox (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  event_type TEXT NOT NULL,
  aggregate_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  payload TEXT NOT NULL,
  state TEXT NOT NULL DEFAULT 'pending' CHECK (state IN ('pending','processing','done','dead')),
  retry_count INTEGER NOT NULL DEFAULT 0,
  next_retry_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  last_error TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE UNIQUE INDEX IF NOT EXISTS ux_outbox_idempotency
ON memory_outbox(idempotency_key);

CREATE INDEX IF NOT EXISTS idx_outbox_poll
ON memory_outbox(state, next_retry_at, created_at);

CREATE TABLE IF NOT EXISTS memory_feedback (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  turn_id TEXT,
  used_items TEXT NOT NULL DEFAULT '[]',
  helpful TEXT NOT NULL DEFAULT '[]',
  harmful TEXT NOT NULL DEFAULT '[]',
  note TEXT,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_feedback_scope
ON memory_feedback(tenant_id, entity_id, process_id, created_at DESC);

CREATE TABLE IF NOT EXISTS memory_embedding (
  memory_id TEXT PRIMARY KEY REFERENCES memory_item(id) ON DELETE CASCADE,
  tenant_id TEXT NOT NULL,
  namespace TEXT NOT NULL,
  model TEXT NOT NULL,
  dims INTEGER NOT NULL,
  embedding_json TEXT NOT NULL,
  embedding_blob BLOB NOT NULL,
  recall_text TEXT NOT NULL,
  metadata TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  updated_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_embedding_namespace_model
ON memory_embedding(namespace, model, updated_at DESC);

CREATE TABLE IF NOT EXISTS memory_audit_log (
  id TEXT PRIMARY KEY,
  tenant_id TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  process_id TEXT NOT NULL,
  request_id TEXT,
  actor TEXT NOT NULL,
  action TEXT NOT NULL,
  payload TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_audit_scope
ON memory_audit_log(tenant_id, entity_id, process_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_audit_request
ON memory_audit_log(request_id);

CREATE TABLE IF NOT EXISTS memory_llm_usage (
  id TEXT PRIMARY KEY,
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
  payload TEXT NOT NULL DEFAULT '{}',
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);

CREATE INDEX IF NOT EXISTS idx_llm_usage_scope
ON memory_llm_usage(tenant_id, entity_id, process_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_llm_usage_event
ON memory_llm_usage(event_type, event_id, created_at DESC);

CREATE INDEX IF NOT EXISTS idx_llm_usage_operation
ON memory_llm_usage(operation, model, created_at DESC);
