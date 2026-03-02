# MemBurrow LLM Integration Guide

## 1. Overview

MemBurrow is an external memory service for agent runtimes.
It provides durable long-term memory with structured storage and semantic recall.

Core responsibilities:

- Accept conversation/events and enqueue asynchronous memory processing.
- Extract and store durable memory units.
- Serve recall results for LLM context injection.
- Track usage and audit metadata.

Current implementation:

- Structured memory source of truth: PostgreSQL
- Vector index: Qdrant
- Embeddings API: OpenAI-compatible `/v1/embeddings`
- Extraction model: OpenAI-compatible chat completion endpoint

## 2. Minimum Configuration

Required:

```env
DATABASE_URL=postgres://postgres:postgres@postgres:5432/memburrow
API_AUTH_TOKEN=dev-token
QDRANT_URL=http://qdrant:6333
OPENAI_BASE_URL=https://api.openai.com/v1
OPENAI_API_KEY=sk-xxxx
OPENAI_EXTRACT_MODEL=gpt-4o-mini
OPENAI_EMBEDDING_MODEL=text-embedding-3-small
EMBEDDING_DIMS=1536
```

Optional:

```env
API_BIND_ADDR=0.0.0.0:8080
WORKER_POLL_INTERVAL_MS=1500
WORKER_BATCH_SIZE=32
WORKER_MAX_RETRY=8
RECALL_CANDIDATE_LIMIT=64
RECONCILE_ENABLED=true
RECONCILE_INTERVAL_SECONDS=120
RECONCILE_BATCH_SIZE=200
LOG_FORMAT=json
RUST_LOG=info
```

Constraints:

- `OPENAI_BASE_URL` must end with `/v1`.
- `EMBEDDING_DIMS` must match real model output dimensions.
- Qdrant collection name is fixed to `memburrow_agent_memory`.

## 3. Startup

```bash
docker compose up -d --build
```

Services:

- `postgres`
- `qdrant`
- `memory-service` (migrator -> worker + api)

## 4. API Summary

Base URL: `http://<host>:8080`

Headers:

- `Authorization: Bearer <API_AUTH_TOKEN>`
- `X-Tenant-ID: <tenant_id>`
- Optional: `X-Request-ID: <request_id>`

The `tenant_id` in headers must match request body `tenant_id`.

### 4.1 Health

- `GET /v1/memory/health`

Response:

```json
{
  "status": "ok",
  "timestamp": "2026-03-01T12:00:00Z"
}
```

### 4.2 Ingest

- `POST /v1/memory/ingest`

Request:

```json
{
  "tenant_id": "acme",
  "entity_id": "user_123",
  "process_id": "planner",
  "session_id": "sess_001",
  "turn_id": "turn_001",
  "messages": [
    {"role": "user", "content": "I do not drink espresso."},
    {"role": "assistant", "content": "Noted."}
  ],
  "context": {
    "channel": "web",
    "language": "en-US"
  }
}
```

Response:

```json
{
  "accepted": true,
  "event_id": "019...",
  "task_id": "019..."
}
```

Behavior:

- Synchronous path writes event + outbox and returns fast.
- Extraction, embedding, and Qdrant upsert happen asynchronously in worker.
- `turn_id` should be unique per conversation turn.
- If `turn_id` is missing, the server uses `no-turn` in idempotency key generation.
- Reused `event_id` means idempotency hit, so no new outbox task is enqueued.

### 4.3 Recall

- `POST /v1/memory/recall`

Request:

```json
{
  "tenant_id": "acme",
  "entity_id": "user_123",
  "process_id": "planner",
  "query": "Recommend a low-caffeine drink for this afternoon.",
  "intent": "recommendation",
  "top_k": 8
}
```

Response:

```json
{
  "items": [
    {
      "id": "019...",
      "type": "preference",
      "content": "User does not drink espresso.",
      "score": 0.93,
      "source": "conversation",
      "properties": {
        "role": "user"
      }
    }
  ],
  "debug": {
    "route": "hybrid",
    "candidate_count": 12,
    "dropped": {
      "expired": 0,
      "low_confidence": 1
    }
  }
}
```

Routing behavior:

- `intent` in `policy/rule/preference/constraint/safety/decision` -> SQL-first.
- Other intents -> Hybrid (vector + SQL) with SQL fallback if vector search fails.
- Hybrid vector search is pre-filtered by namespace scope.

Scoring behavior (rerank):

- semantic + lexical + importance + confidence + freshness + scope.

### 4.4 Feedback

- `POST /v1/memory/feedback`

Request:

```json
{
  "tenant_id": "acme",
  "entity_id": "user_123",
  "process_id": "planner",
  "turn_id": "turn_001",
  "used_items": ["019..."],
  "helpful": ["019..."],
  "harmful": [],
  "note": "Matched preference and improved recommendation quality."
}
```

Response:

```json
{
  "accepted": true,
  "event_id": "019...",
  "task_id": "019..."
}
```

## 5. Recommended LLM Call Sequence

1. Call `recall` before planning.
2. Inject `recall.items` into system/tool context.
3. Generate model response.
4. Call `ingest` with user and assistant messages.
5. Optionally call `feedback` when quality signals are available.

Pseudo-flow:

```text
memories = recall(query, tenant_id, entity_id, process_id)
prompt = prompt + format(memories)
answer = llm(prompt, user_input)
ingest(messages=[user_input, answer])
feedback(used_items, helpful, harmful)
```

## 6. Isolation, Idempotency, and Consistency

Isolation model:

- External request scope keys:
  - `tenant_id`
  - `entity_id`
  - `process_id`
- Internal canonical scope key:
  - `namespace = <tenant_id>:<entity_id>:<process_id>`
- Recall allowed namespaces:
  - current process: `<tenant>:<entity>:<process>`
  - shared process: `<tenant>:<entity>:__shared__`
- SQL candidate fetch and Qdrant vector search are both filtered by allowed namespaces.
- If `process_id` is already `__shared__`, recall only uses the shared namespace.

Idempotency:

- `ingest`/`feedback` deduplicated via outbox idempotency keys.
- `ingest` idempotency key format:
  - `ingest:<tenant_id>:<entity_id>:<process_id>:<turn_id_or_no-turn>`
- `feedback` idempotency key format:
  - `feedback:<tenant_id>:<entity_id>:<process_id>:<turn_id_or_no-turn>`
- Missing `turn_id` causes all requests for same tenant/entity/process to share `no-turn`, often deduplicating later turns unexpectedly.
- Worker retries with exponential backoff.

Consistency model:

- Event and outbox are transactional.
- Recall is eventually consistent with asynchronous extraction/indexing.

## 7. Error Handling Guidance

For 5xx errors:

- `recall`: continue main flow without memory fallback.
- `ingest`/`feedback`: retry with same identifiers (`turn_id` recommended).

For embedding dimension mismatch:

- Ensure `EMBEDDING_DIMS` matches `OPENAI_EMBEDDING_MODEL`.

For temporary low recall after ingest:

- This is usually asynchronous processing lag.
- Add a short wait or polling strategy before strict recall assertions.

## 8. Known Limitations

- No local embedding model support at this time.
- Extraction quality depends on upstream LLM response quality.
- Immediate read-after-write completeness is not guaranteed due to async processing.

## 9. Quick API Examples

```bash
curl -s http://localhost:8080/v1/memory/health

curl -s -X POST http://localhost:8080/v1/memory/ingest \
  -H 'Authorization: Bearer dev-token' \
  -H 'X-Tenant-ID: acme' \
  -H 'Content-Type: application/json' \
  -d @tests/data/ingest_preference.json

curl -s -X POST http://localhost:8080/v1/memory/recall \
  -H 'Authorization: Bearer dev-token' \
  -H 'X-Tenant-ID: acme' \
  -H 'Content-Type: application/json' \
  -d @tests/data/recall_preference.json

curl -s -X POST http://localhost:8080/v1/memory/feedback \
  -H 'Authorization: Bearer dev-token' \
  -H 'X-Tenant-ID: acme' \
  -H 'Content-Type: application/json' \
  -d @tests/data/feedback_preference.json
```
