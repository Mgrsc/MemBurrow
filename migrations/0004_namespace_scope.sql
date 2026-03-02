ALTER TABLE memory_item
ADD COLUMN IF NOT EXISTS namespace TEXT;

UPDATE memory_item
SET namespace = tenant_id || ':' || entity_id || ':' || process_id
WHERE namespace IS NULL OR namespace = '';

ALTER TABLE memory_item
ALTER COLUMN namespace SET NOT NULL;

DROP INDEX IF EXISTS ux_memory_fingerprint;
DROP INDEX IF EXISTS idx_memory_scope;
DROP INDEX IF EXISTS idx_memory_expire;

CREATE UNIQUE INDEX IF NOT EXISTS ux_memory_namespace_fingerprint
ON memory_item(namespace, memory_type, fingerprint_hash);

CREATE INDEX IF NOT EXISTS idx_memory_namespace_scope
ON memory_item(namespace, memory_type, status, last_seen_at DESC);

CREATE INDEX IF NOT EXISTS idx_memory_namespace_expire
ON memory_item(namespace, expires_at);

ALTER TABLE memory_embedding
ADD COLUMN IF NOT EXISTS namespace TEXT;

UPDATE memory_embedding AS embedding
SET namespace = item.namespace
FROM memory_item AS item
WHERE item.id = embedding.memory_id
  AND (embedding.namespace IS NULL OR embedding.namespace = '');

ALTER TABLE memory_embedding
ALTER COLUMN namespace SET NOT NULL;

DROP INDEX IF EXISTS idx_embedding_scope;

CREATE INDEX IF NOT EXISTS idx_embedding_namespace_model
ON memory_embedding(namespace, model, updated_at DESC);
