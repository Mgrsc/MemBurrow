<p align="center">
  <h1 align="center">MemBurrow</h1>
  <p align="center">Long-term memory service for AI agents. SQL as the source of truth, vectors as the acceleration layer.</p>
  <p align="center"><em>Not another vector-only RAG.</em></p>
</p>

<p align="center">
  <a href="https://www.rust-lang.org/"><img src="https://img.shields.io/badge/Rust-000000?logo=rust&logoColor=white" alt="Rust"></a>
  <a href="https://www.postgresql.org/"><img src="https://img.shields.io/badge/PostgreSQL-4169E1?logo=postgresql&logoColor=white" alt="PostgreSQL"></a>
  <a href="https://qdrant.tech/"><img src="https://img.shields.io/badge/Qdrant-DC382D?logo=qdrant&logoColor=white" alt="Qdrant"></a>
  <a href="https://www.docker.com/"><img src="https://img.shields.io/badge/Docker-2496ED?logo=docker&logoColor=white" alt="Docker"></a>
  <a href="https://github.com/Mgrsc/MemBurrow/blob/main/LICENSE"><img src="https://img.shields.io/badge/License-Apache--2.0-blue" alt="License"></a>
</p>

<p align="center">
  <a href="README-zh.md">中文文档</a>
</p>

---

## Why Not Vector-Only?

Without long-term memory, agents repeatedly ask for context, forget user preferences, and violate previously stated rules — burning tokens replaying chat history every turn.

Vector-only memory makes this worse: it retrieves by surface similarity, not by operational correctness.

| | Vector-Only | MemBurrow |
|---|---|---|
| Source of truth | Embeddings | ✅ SQL |
| Retrieval strategy | Cosine similarity only | ✅ Intent-based routing |
| Ranking | Single similarity score | ✅ Multi-factor scoring |
| Rule / preference | Embedded in text chunks | ✅ First-class structured types |
| Vector DB down | ❌ No recall | ✅ SQL-only fallback |
| Audit trail | ❌ Opaque | ✅ Full explainability |
| Write semantics | Fire-and-forget | ✅ Outbox, exactly-once |

## Architecture

**Write Flow**

```
Client → API (event + outbox tx, <60 ms, 202) → Worker → LLM extract → Embed → Qdrant upsert
```

**Recall Flow**

```
Client → API → Intent Router → SQL-first / Hybrid → Rerank → Response
```

```mermaid
flowchart LR
    C([Client]) --> |ingest| API
    API --> |event + outbox tx| PG[(PostgreSQL)]
    API --> |202 Accepted| C
    PG --> |poll| W[Worker]
    W --> |extract| LLM[LLM API]
    W --> |embed| EMB[Embedding API]
    W --> |upsert| QD[(Qdrant)]
    W --> |update status| PG

    C --> |recall| API2[API]
    API2 --> |intent route| IR{Intent Router}
    IR --> |policy / rule / preference| SQL[SQL-first]
    IR --> |other| HY[Hybrid]
    SQL --> RR[Rerank]
    HY --> RR
    RR --> |response| C
```

**Scoring formula:**

`score = semantic(0.45) + importance(0.18) + confidence(0.14) + freshness(0.13) + scope(0.10)`

## Key Features

- **5 Memory Types** — fact, preference, rule, context, event
- **Intent-Based Routing** — policy/rule/preference queries go SQL-first; others go hybrid
- **Outbox Pattern** — event + outbox written in a single transaction, exactly-once delivery
- **Graceful Degradation** — vector DB down? recall falls back to SQL-only
- **Scope Isolation** — tenant / entity / process level isolation
- **Audit Trail** — full traceability for every memory operation
- **Async Write Path** — ingest returns in <60 ms; extraction happens in background

> **Trade-off:** async write path means recall is eventually consistent — a freshly ingested memory may not appear immediately.

## Quick Start

```bash
git clone https://github.com/Mgrsc/MemBurrow.git && cd MemBurrow
cp .env.example .env  # set OPENAI_API_KEY
docker compose up -d --build
```

Verify:

```bash
curl -s http://localhost:8080/v1/memory/health
# {"status":"ok","timestamp":"..."}
```

## API Examples

**Ingest**

```bash
curl -s -X POST http://localhost:8080/v1/memory/ingest \
  -H 'Authorization: Bearer dev-token' \
  -H 'X-Tenant-ID: acme' \
  -H 'Content-Type: application/json' \
  -d '{
    "tenant_id": "acme",
    "entity_id": "user_123",
    "process_id": "planner",
    "session_id": "sess_001",
    "turn_id": "turn_001",
    "messages": [
      {"role": "user", "content": "I do not drink espresso."},
      {"role": "assistant", "content": "Noted."}
    ]
  }'
```

**Recall**

```bash
curl -s -X POST http://localhost:8080/v1/memory/recall \
  -H 'Authorization: Bearer dev-token' \
  -H 'X-Tenant-ID: acme' \
  -H 'Content-Type: application/json' \
  -d '{
    "tenant_id": "acme",
    "entity_id": "user_123",
    "process_id": "planner",
    "query": "Recommend a low-caffeine drink for this afternoon.",
    "intent": "recommendation",
    "top_k": 8
  }'
```

> Full API documentation: [LLM_README.md](LLM_README.md)

## Tech Stack

| Layer | Technology |
|---|---|
| Language | Rust |
| HTTP framework | Axum |
| Database | PostgreSQL + SQLx |
| Vector index | Qdrant |
| LLM / Embedding | OpenAI-compatible API |
| Deployment | Docker Compose |

## Project Structure

```
MemBurrow/
├── api/
│   └── openapi.yaml
├── cmd/
│   ├── memory-api/          # HTTP API server
│   ├── memory-migrator/     # Database migration runner
│   └── memory-worker/       # Async extraction & embedding worker
├── crates/
│   └── memory-core/         # Shared domain logic, models, store
├── migrations/              # SQL migration files
├── docker-compose.yml
├── Dockerfile
└── .env.example
```

## Configuration

<details>
<summary>Environment Variables</summary>

**Required**

| Variable | Description |
|---|---|
| `DATABASE_URL` | PostgreSQL connection string |
| `API_AUTH_TOKEN` | Bearer token for API authentication |
| `QDRANT_URL` | Qdrant gRPC/HTTP endpoint |
| `OPENAI_BASE_URL` | OpenAI-compatible API base URL (must end with `/v1`) |
| `OPENAI_API_KEY` | API key for LLM and embedding calls |
| `OPENAI_EXTRACT_MODEL` | Model for memory extraction (e.g. `gpt-4o-mini`) |
| `OPENAI_EMBEDDING_MODEL` | Model for embeddings (e.g. `text-embedding-3-small`) |
| `EMBEDDING_DIMS` | Embedding dimensions (must match model output) |

**Optional**

| Variable | Default | Description |
|---|---|---|
| `API_BIND_ADDR` | `0.0.0.0:8080` | API listen address |
| `WORKER_POLL_INTERVAL_MS` | `1500` | Worker poll interval |
| `WORKER_BATCH_SIZE` | `32` | Worker batch size |
| `WORKER_MAX_RETRY` | `8` | Max retry attempts |
| `RECALL_CANDIDATE_LIMIT` | `64` | Max recall candidates |
| `RECONCILE_ENABLED` | `true` | Enable reconciliation |
| `RECONCILE_INTERVAL_SECONDS` | `120` | Reconciliation interval |
| `RECONCILE_BATCH_SIZE` | `200` | Reconciliation batch size |
| `LOG_FORMAT` | `json` | Log format (`json` or `pretty`) |
| `RUST_LOG` | `info` | Log level |

</details>

## References

- [Memorilabs — AI Agent Memory on Postgres: Back to SQL](https://memorilabs.ai/blog/ai-agent-memory-on-postgres-back-to-sql)
- [Qdrant Documentation](https://qdrant.tech/documentation/)

## License

[Apache-2.0](LICENSE)
