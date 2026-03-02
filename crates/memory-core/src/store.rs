use std::{cmp::Ordering, collections::HashMap};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::{FromRow, Pool, Postgres, postgres::PgPoolOptions};
use thiserror::Error;
use uuid::Uuid;

use crate::model::{
    FeedbackRequest, IngestRequest, IngestResponse, OutboxEnvelope, OutboxFeedbackEnvelope,
    RecallDebug, RecallItem, RecallRequest, RecallResponse, allowed_namespaces, build_namespace,
};

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
}

#[derive(Debug, Clone, FromRow)]
struct MemoryCandidate {
    id: Uuid,
    process_id: String,
    memory_type: String,
    content: String,
    importance: i16,
    confidence: f64,
    last_seen_at: DateTime<Utc>,
    source: String,
    properties: Value,
}

#[derive(Debug, Clone, FromRow)]
pub struct OutboxMessage {
    pub id: Uuid,
    pub tenant_id: String,
    pub event_type: String,
    pub payload: Value,
    pub retry_count: i32,
}

#[derive(Debug, Clone)]
pub struct AuditLogInput {
    pub tenant_id: String,
    pub entity_id: String,
    pub process_id: String,
    pub request_id: Option<String>,
    pub actor: String,
    pub action: String,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct LlmUsageInput {
    pub tenant_id: String,
    pub entity_id: String,
    pub process_id: String,
    pub event_type: String,
    pub event_id: Option<String>,
    pub operation: String,
    pub provider: String,
    pub model: String,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct ExtractedMemory {
    pub tenant_id: String,
    pub entity_id: String,
    pub process_id: String,
    pub session_id: Option<String>,
    pub memory_type: String,
    pub category: String,
    pub content: String,
    pub normalized_content: String,
    pub importance: i16,
    pub confidence: f64,
    pub source: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub properties: Value,
}

#[derive(Debug, Clone)]
pub struct EmbeddingRecord {
    pub memory_id: Uuid,
    pub tenant_id: String,
    pub namespace: String,
    pub model: String,
    pub dims: i32,
    pub embedding: Value,
    pub recall_text: String,
    pub metadata: Value,
}

#[derive(Debug, Clone)]
pub struct ReconcileCandidate {
    pub memory_id: Uuid,
    pub tenant_id: String,
    pub namespace: String,
    pub entity_id: String,
    pub process_id: String,
    pub memory_type: String,
    pub source: String,
    pub metadata: Value,
    pub vector: Vec<f32>,
}

#[derive(Debug, Clone, FromRow)]
struct ReconcileRow {
    memory_id: Uuid,
    tenant_id: String,
    namespace: String,
    entity_id: String,
    process_id: String,
    memory_type: String,
    source: String,
    metadata: Value,
    embedding: Value,
}

pub async fn connect_pool(database_url: &str) -> Result<Pool<Postgres>, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(20)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect(database_url)
        .await
}

pub async fn ping(pool: &Pool<Postgres>) -> Result<(), CoreError> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(CoreError::Database)
}

pub async fn ingest_event(
    pool: &Pool<Postgres>,
    request: &IngestRequest,
) -> Result<IngestResponse, CoreError> {
    if request.messages.is_empty() {
        return Err(CoreError::InvalidRequest(
            "messages must not be empty".to_owned(),
        ));
    }

    let idempotency_key = format!(
        "ingest:{}:{}:{}:{}",
        request.tenant_id,
        request.entity_id,
        request.process_id,
        request
            .turn_id
            .clone()
            .unwrap_or_else(|| "no-turn".to_owned())
    );

    let mut tx = pool.begin().await?;

    if let Some(existing_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM memory_outbox WHERE idempotency_key = $1 LIMIT 1",
    )
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return Ok(IngestResponse {
            accepted: true,
            event_id: existing_id.to_string(),
            task_id: existing_id.to_string(),
        });
    }

    let occurred_at = Utc::now();
    for message in &request.messages {
        sqlx::query(
            "INSERT INTO memory_event (id, tenant_id, entity_id, process_id, session_id, turn_id, role, payload) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::now_v7())
        .bind(&request.tenant_id)
        .bind(&request.entity_id)
        .bind(&request.process_id)
        .bind(&request.session_id)
        .bind(&request.turn_id)
        .bind(&message.role)
        .bind(json!({
            "content": &message.content,
            "context": request.context.clone(),
            "occurred_at": occurred_at,
        }))
        .execute(&mut *tx)
        .await?;
    }

    let outbox_id = Uuid::now_v7();
    let payload = serde_json::to_value(OutboxEnvelope {
        schema_version: 1,
        event_id: outbox_id.to_string(),
        occurred_at,
        request: request.clone(),
    })?;

    sqlx::query(
        "INSERT INTO memory_outbox (id, tenant_id, event_type, aggregate_id, idempotency_key, payload) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(outbox_id)
    .bind(&request.tenant_id)
    .bind("memory.ingest")
    .bind(outbox_id)
    .bind(&idempotency_key)
    .bind(payload)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(IngestResponse {
        accepted: true,
        event_id: outbox_id.to_string(),
        task_id: outbox_id.to_string(),
    })
}

pub async fn persist_feedback(
    pool: &Pool<Postgres>,
    request: &FeedbackRequest,
) -> Result<IngestResponse, CoreError> {
    let idempotency_key = format!(
        "feedback:{}:{}:{}:{}",
        request.tenant_id,
        request.entity_id,
        request.process_id,
        request
            .turn_id
            .clone()
            .unwrap_or_else(|| "no-turn".to_owned())
    );

    let mut tx = pool.begin().await?;

    if let Some(existing_id) = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM memory_outbox WHERE idempotency_key = $1 LIMIT 1",
    )
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return Ok(IngestResponse {
            accepted: true,
            event_id: existing_id.to_string(),
            task_id: existing_id.to_string(),
        });
    }

    let feedback_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO memory_feedback (id, tenant_id, entity_id, process_id, turn_id, used_items, helpful, harmful, note) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(feedback_id)
    .bind(&request.tenant_id)
    .bind(&request.entity_id)
    .bind(&request.process_id)
    .bind(&request.turn_id)
    .bind(json!(request.used_items))
    .bind(json!(request.helpful))
    .bind(json!(request.harmful))
    .bind(&request.note)
    .execute(&mut *tx)
    .await?;

    let outbox_id = Uuid::now_v7();
    let payload = serde_json::to_value(OutboxFeedbackEnvelope {
        schema_version: 1,
        event_id: outbox_id.to_string(),
        occurred_at: Utc::now(),
        request: request.clone(),
    })?;

    sqlx::query(
        "INSERT INTO memory_outbox (id, tenant_id, event_type, aggregate_id, idempotency_key, payload) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(outbox_id)
    .bind(&request.tenant_id)
    .bind("memory.feedback")
    .bind(feedback_id)
    .bind(&idempotency_key)
    .bind(payload)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(IngestResponse {
        accepted: true,
        event_id: outbox_id.to_string(),
        task_id: outbox_id.to_string(),
    })
}

pub async fn recall_memories(
    pool: &Pool<Postgres>,
    request: &RecallRequest,
    recall_candidate_limit: i64,
) -> Result<RecallResponse, CoreError> {
    let candidates = fetch_candidates(
        pool,
        request,
        recall_candidate_limit,
        should_prefer_structured(&request.intent),
    )
    .await?;
    build_recall_response(request, candidates, HashMap::new(), "sql-first")
}

pub async fn recall_memories_hybrid(
    pool: &Pool<Postgres>,
    request: &RecallRequest,
    recall_candidate_limit: i64,
    vector_scores: HashMap<Uuid, f64>,
) -> Result<RecallResponse, CoreError> {
    let mut merged = HashMap::<Uuid, MemoryCandidate>::new();
    let structured = should_prefer_structured(&request.intent);

    let sql_candidates =
        fetch_candidates(pool, request, recall_candidate_limit, structured).await?;
    for candidate in sql_candidates {
        merged.insert(candidate.id, candidate);
    }

    if !vector_scores.is_empty() {
        let vector_ids = vector_scores.keys().copied().collect::<Vec<_>>();
        let vector_candidates =
            fetch_candidates_by_ids(pool, request, &vector_ids, structured).await?;
        for candidate in vector_candidates {
            merged.insert(candidate.id, candidate);
        }
    }

    build_recall_response(
        request,
        merged.into_values().collect::<Vec<_>>(),
        vector_scores,
        "hybrid",
    )
}

async fn fetch_candidates(
    pool: &Pool<Postgres>,
    request: &RecallRequest,
    recall_candidate_limit: i64,
    structured_only: bool,
) -> Result<Vec<MemoryCandidate>, CoreError> {
    let namespaces =
        allowed_namespaces(&request.tenant_id, &request.entity_id, &request.process_id);
    let rows = if structured_only {
        sqlx::query_as::<_, MemoryCandidate>(
            "SELECT id, process_id, memory_type, content, importance, confidence::float8 AS confidence, \
                    last_seen_at, source, properties \
             FROM memory_item \
             WHERE namespace = ANY($1) \
               AND status = 'active' \
               AND (expires_at IS NULL OR expires_at > now()) \
               AND memory_type IN ('preference', 'rule') \
             ORDER BY last_seen_at DESC, importance DESC \
             LIMIT $2",
        )
        .bind(&namespaces)
        .bind(recall_candidate_limit)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, MemoryCandidate>(
            "SELECT id, process_id, memory_type, content, importance, confidence::float8 AS confidence, \
                    last_seen_at, source, properties \
             FROM memory_item \
             WHERE namespace = ANY($1) \
               AND status = 'active' \
               AND (expires_at IS NULL OR expires_at > now()) \
             ORDER BY last_seen_at DESC, importance DESC \
             LIMIT $2",
        )
        .bind(&namespaces)
        .bind(recall_candidate_limit)
        .fetch_all(pool)
        .await?
    };

    Ok(rows)
}

async fn fetch_candidates_by_ids(
    pool: &Pool<Postgres>,
    request: &RecallRequest,
    ids: &[Uuid],
    structured_only: bool,
) -> Result<Vec<MemoryCandidate>, CoreError> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }

    let namespaces =
        allowed_namespaces(&request.tenant_id, &request.entity_id, &request.process_id);
    let rows = if structured_only {
        sqlx::query_as::<_, MemoryCandidate>(
            "SELECT id, process_id, memory_type, content, importance, confidence::float8 AS confidence, \
                    last_seen_at, source, properties \
             FROM memory_item \
             WHERE namespace = ANY($1) \
               AND id = ANY($2) \
               AND status = 'active' \
               AND (expires_at IS NULL OR expires_at > now()) \
               AND memory_type IN ('preference', 'rule')",
        )
        .bind(&namespaces)
        .bind(ids)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, MemoryCandidate>(
            "SELECT id, process_id, memory_type, content, importance, confidence::float8 AS confidence, \
                    last_seen_at, source, properties \
             FROM memory_item \
             WHERE namespace = ANY($1) \
               AND id = ANY($2) \
               AND status = 'active' \
               AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(&namespaces)
        .bind(ids)
        .fetch_all(pool)
        .await?
    };

    Ok(rows)
}

fn build_recall_response(
    request: &RecallRequest,
    candidates: Vec<MemoryCandidate>,
    vector_scores: HashMap<Uuid, f64>,
    route: &str,
) -> Result<RecallResponse, CoreError> {
    let candidate_count = candidates.len();
    let query = request.query.to_lowercase();
    let terms = query_terms(&query);
    let now = Utc::now();

    let mut dropped_low_confidence = 0usize;
    let mut ranked = candidates
        .into_iter()
        .filter_map(|candidate| {
            if candidate.confidence < 0.15 {
                dropped_low_confidence += 1;
                return None;
            }

            let vector = vector_scores.get(&candidate.id).copied().unwrap_or(0.0);
            let lexical = semantic_score(&candidate.content.to_lowercase(), &query, &terms);
            let semantic = (0.70 * vector + 0.30 * lexical).clamp(0.0, 1.0);
            let importance = (candidate.importance as f64 / 100.0).clamp(0.0, 1.0);
            let confidence = candidate.confidence.clamp(0.0, 1.0);
            let hours = (now - candidate.last_seen_at).num_minutes().max(0) as f64 / 60.0;
            let freshness = 1.0 / (1.0 + hours / 168.0);
            let scope_boost = if candidate.process_id == request.process_id {
                1.0
            } else {
                0.6
            };
            let score = 0.40 * semantic
                + 0.16 * importance
                + 0.12 * confidence
                + 0.22 * freshness
                + 0.10 * scope_boost;

            Some((
                RecallItem {
                    id: candidate.id,
                    r#type: candidate.memory_type,
                    content: candidate.content,
                    score,
                    source: candidate.source,
                    properties: candidate.properties,
                },
                candidate.last_seen_at,
            ))
        })
        .collect::<Vec<_>>();

    ranked.sort_by(|left, right| {
        right
            .0
            .score
            .partial_cmp(&left.0.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| right.1.cmp(&left.1))
            .then_with(|| right.0.id.cmp(&left.0.id))
    });
    let mut ranked = ranked.into_iter().map(|(item, _)| item).collect::<Vec<_>>();
    ranked.truncate(request.top_k.max(1));

    Ok(RecallResponse {
        items: ranked,
        debug: RecallDebug {
            route: route.to_owned(),
            candidate_count,
            dropped: json!({
                "expired": 0,
                "low_confidence": dropped_low_confidence,
                "vector_missing": vector_scores.is_empty(),
            }),
        },
    })
}

pub async fn claim_outbox_batch(
    pool: &Pool<Postgres>,
    batch_size: i64,
) -> Result<Vec<OutboxMessage>, CoreError> {
    let rows = sqlx::query_as::<_, OutboxMessage>(
        "WITH picked AS ( \
            SELECT id \
            FROM memory_outbox \
            WHERE state = 'pending' AND next_retry_at <= now() \
            ORDER BY created_at \
            LIMIT $1 \
            FOR UPDATE SKIP LOCKED \
         ) \
         UPDATE memory_outbox AS outbox \
         SET state = 'processing', updated_at = now() \
         FROM picked \
         WHERE outbox.id = picked.id \
         RETURNING outbox.id, outbox.tenant_id, outbox.event_type, outbox.payload, outbox.retry_count",
    )
    .bind(batch_size)
    .fetch_all(pool)
    .await?;

    Ok(rows)
}

pub async fn mark_outbox_done(pool: &Pool<Postgres>, outbox_id: Uuid) -> Result<(), CoreError> {
    sqlx::query(
        "UPDATE memory_outbox \
         SET state = 'done', updated_at = now(), last_error = NULL \
         WHERE id = $1",
    )
    .bind(outbox_id)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn mark_outbox_retry(
    pool: &Pool<Postgres>,
    outbox_id: Uuid,
    current_retry_count: i32,
    max_retry: i32,
    error_message: &str,
) -> Result<(), CoreError> {
    let next_retry_count = current_retry_count + 1;
    if next_retry_count >= max_retry {
        sqlx::query(
            "UPDATE memory_outbox \
             SET state = 'dead', retry_count = $2, last_error = $3, updated_at = now() \
             WHERE id = $1",
        )
        .bind(outbox_id)
        .bind(next_retry_count)
        .bind(error_message)
        .execute(pool)
        .await?;
        return Ok(());
    }

    let wait_seconds = 2_i64.pow(next_retry_count.min(10) as u32);
    sqlx::query(
        "UPDATE memory_outbox \
         SET state = 'pending', retry_count = $2, last_error = $3, \
             next_retry_at = now() + ($4::text || ' seconds')::interval, \
             updated_at = now() \
         WHERE id = $1",
    )
    .bind(outbox_id)
    .bind(next_retry_count)
    .bind(error_message)
    .bind(wait_seconds)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn upsert_memory_item(
    pool: &Pool<Postgres>,
    memory: &ExtractedMemory,
) -> Result<Uuid, CoreError> {
    let namespace = build_namespace(&memory.tenant_id, &memory.entity_id, &memory.process_id);
    let memory_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO memory_item ( \
            id, tenant_id, entity_id, process_id, namespace, session_id, memory_type, category, content, normalized_content, \
            importance, confidence, source, status, fingerprint_hash, expires_at, properties \
         ) VALUES ( \
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, \
            $11, $12, $13, 'active', $14, $15, $16 \
         ) \
         ON CONFLICT (namespace, memory_type, fingerprint_hash) \
         DO UPDATE SET \
            content = EXCLUDED.content, \
            normalized_content = EXCLUDED.normalized_content, \
            importance = LEAST(100, GREATEST(memory_item.importance, EXCLUDED.importance) + 1), \
            confidence = GREATEST(memory_item.confidence, EXCLUDED.confidence), \
            mention_count = memory_item.mention_count + 1, \
            last_seen_at = now(), \
            version = memory_item.version + 1, \
            properties = memory_item.properties || EXCLUDED.properties, \
            expires_at = COALESCE(EXCLUDED.expires_at, memory_item.expires_at), \
            updated_at = now() \
         RETURNING id",
    )
    .bind(Uuid::now_v7())
    .bind(&memory.tenant_id)
    .bind(&memory.entity_id)
    .bind(&memory.process_id)
    .bind(&namespace)
    .bind(&memory.session_id)
    .bind(&memory.memory_type)
    .bind(&memory.category)
    .bind(&memory.content)
    .bind(&memory.normalized_content)
    .bind(memory.importance)
    .bind(memory.confidence)
    .bind(&memory.source)
    .bind(fingerprint_of(memory))
    .bind(memory.expires_at)
    .bind(&memory.properties)
    .fetch_one(pool)
    .await?;

    Ok(memory_id)
}

pub async fn persist_embedding(
    pool: &Pool<Postgres>,
    record: &EmbeddingRecord,
) -> Result<(), CoreError> {
    sqlx::query(
        "INSERT INTO memory_embedding (memory_id, tenant_id, namespace, model, dims, embedding, recall_text, metadata) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (memory_id) \
         DO UPDATE SET \
            namespace = EXCLUDED.namespace, \
            model = EXCLUDED.model, \
            dims = EXCLUDED.dims, \
            embedding = EXCLUDED.embedding, \
            recall_text = EXCLUDED.recall_text, \
            metadata = EXCLUDED.metadata, \
            updated_at = now()",
    )
    .bind(record.memory_id)
    .bind(&record.tenant_id)
    .bind(&record.namespace)
    .bind(&record.model)
    .bind(record.dims)
    .bind(&record.embedding)
    .bind(&record.recall_text)
    .bind(&record.metadata)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn append_audit_log(
    pool: &Pool<Postgres>,
    input: &AuditLogInput,
) -> Result<(), CoreError> {
    sqlx::query(
        "INSERT INTO memory_audit_log ( \
            id, tenant_id, entity_id, process_id, request_id, actor, action, payload \
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(&input.tenant_id)
    .bind(&input.entity_id)
    .bind(&input.process_id)
    .bind(&input.request_id)
    .bind(&input.actor)
    .bind(&input.action)
    .bind(&input.payload)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn persist_llm_usage(
    pool: &Pool<Postgres>,
    input: &LlmUsageInput,
) -> Result<(), CoreError> {
    sqlx::query(
        "INSERT INTO memory_llm_usage ( \
            id, tenant_id, entity_id, process_id, event_type, event_id, operation, provider, model, \
            prompt_tokens, completion_tokens, total_tokens, payload \
         ) VALUES ( \
            $1, $2, $3, $4, $5, $6, $7, $8, $9, \
            $10, $11, $12, $13 \
         )",
    )
    .bind(Uuid::now_v7())
    .bind(&input.tenant_id)
    .bind(&input.entity_id)
    .bind(&input.process_id)
    .bind(&input.event_type)
    .bind(&input.event_id)
    .bind(&input.operation)
    .bind(&input.provider)
    .bind(&input.model)
    .bind(input.prompt_tokens)
    .bind(input.completion_tokens)
    .bind(input.total_tokens)
    .bind(&input.payload)
    .execute(pool)
    .await?;

    Ok(())
}

pub async fn list_reconcile_candidates(
    pool: &Pool<Postgres>,
    batch_size: i64,
) -> Result<Vec<ReconcileCandidate>, CoreError> {
    let rows = sqlx::query_as::<_, ReconcileRow>(
        "SELECT e.memory_id, e.tenant_id, e.namespace, i.entity_id, i.process_id, i.memory_type, i.source, \
                e.metadata, e.embedding \
         FROM memory_embedding e \
         JOIN memory_item i ON i.id = e.memory_id \
         WHERE i.status = 'active' \
         ORDER BY e.updated_at DESC \
         LIMIT $1",
    )
    .bind(batch_size)
    .fetch_all(pool)
    .await?;

    let mut parsed = Vec::with_capacity(rows.len());
    for row in rows {
        let vector = parse_embedding_vector(&row.embedding)?;
        parsed.push(ReconcileCandidate {
            memory_id: row.memory_id,
            tenant_id: row.tenant_id,
            namespace: row.namespace,
            entity_id: row.entity_id,
            process_id: row.process_id,
            memory_type: row.memory_type,
            source: row.source,
            metadata: row.metadata,
            vector,
        });
    }

    Ok(parsed)
}

fn fingerprint_of(memory: &ExtractedMemory) -> String {
    format!(
        "{}:{}",
        memory.memory_type,
        memory.normalized_content.trim().to_lowercase()
    )
}

fn should_prefer_structured(intent: &Option<String>) -> bool {
    let normalized = intent
        .as_ref()
        .map(|value| value.trim().to_lowercase())
        .unwrap_or_default();

    matches!(
        normalized.as_str(),
        "policy" | "rule" | "preference" | "constraint" | "safety" | "decision"
    )
}

fn query_terms(query: &str) -> Vec<String> {
    query
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|term| !term.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn semantic_score(content: &str, query: &str, terms: &[String]) -> f64 {
    if content.contains(query) {
        return 1.0;
    }

    if terms.is_empty() {
        return 0.0;
    }

    let overlap = terms
        .iter()
        .filter(|term| content.contains(term.as_str()))
        .count();

    overlap as f64 / terms.len() as f64
}

fn parse_embedding_vector(raw: &Value) -> Result<Vec<f32>, CoreError> {
    let Some(values) = raw.as_array() else {
        return Err(CoreError::InvalidRequest(
            "embedding payload is not an array".to_owned(),
        ));
    };

    let mut vector = Vec::with_capacity(values.len());
    for value in values {
        let Some(number) = value.as_f64() else {
            return Err(CoreError::InvalidRequest(
                "embedding payload contains non-numeric value".to_owned(),
            ));
        };
        vector.push(number as f32);
    }

    Ok(vector)
}
