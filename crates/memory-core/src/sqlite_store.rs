use std::{cmp::Ordering, collections::HashMap, str::FromStr, time::Duration};

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{Value, json};
use sqlx::{
    FromRow, Pool, QueryBuilder, Sqlite,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use uuid::Uuid;

use crate::{
    ServiceConfig,
    model::{
        FeedbackRequest, IngestRequest, IngestResponse, OutboxEnvelope, OutboxFeedbackEnvelope,
        RecallDebug, RecallItem, RecallRequest, RecallResponse, allowed_namespaces,
        build_namespace,
    },
    store::{
        AuditLogInput, CoreError, EmbeddingRecord, ExtractedMemory, LlmUsageInput, OutboxMessage,
    },
};

#[derive(Debug, Clone, FromRow)]
struct SqliteOutboxRow {
    id: String,
    tenant_id: String,
    event_type: String,
    payload: String,
    retry_count: i32,
}

#[derive(Debug, Clone, FromRow)]
struct SqliteMemoryCandidateRow {
    id: String,
    process_id: String,
    memory_type: String,
    content: String,
    importance: i16,
    confidence: f64,
    last_seen_at: String,
    source: String,
    properties: String,
}

#[derive(Debug, Clone)]
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
struct SqliteVectorSearchRow {
    memory_id: String,
    distance: f64,
}

pub async fn connect_sqlite_pool(config: &ServiceConfig) -> Result<Pool<Sqlite>, sqlx::Error> {
    let options = SqliteConnectOptions::from_str(&config.sqlite_database_url)
        .map_err(|error| sqlx::Error::Configuration(Box::new(error)))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_millis(config.sqlite_busy_timeout_ms))
        .extension(config.sqlite_vector_extension_path.clone());

    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options)
        .await
}

pub async fn init_sqlite_vector_store(
    pool: &Pool<Sqlite>,
    embedding_dims: usize,
) -> Result<(), CoreError> {
    let options = format!("type=FLOAT32,dimension={embedding_dims},distance=COSINE");
    match sqlx::query("SELECT vector_init('memory_embedding', 'embedding_blob', ?1)")
        .bind(options)
        .execute(pool)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) => {
            let message = error.to_string().to_lowercase();
            if message.contains("already") && message.contains("init") {
                return Ok(());
            }
            Err(CoreError::Database(error))
        }
    }
}

pub async fn ping_sqlite(pool: &Pool<Sqlite>) -> Result<(), CoreError> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(CoreError::Database)
}

pub async fn ingest_event_sqlite(
    pool: &Pool<Sqlite>,
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

    if let Some(existing_id) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM memory_outbox WHERE idempotency_key = ?1 LIMIT 1",
    )
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return Ok(IngestResponse {
            accepted: true,
            event_id: existing_id.clone(),
            task_id: existing_id,
        });
    }

    let occurred_at = Utc::now();
    let now = occurred_at.to_rfc3339();
    for message in &request.messages {
        sqlx::query(
            "INSERT INTO memory_event (id, tenant_id, entity_id, process_id, session_id, turn_id, role, payload, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(&request.tenant_id)
        .bind(&request.entity_id)
        .bind(&request.process_id)
        .bind(&request.session_id)
        .bind(&request.turn_id)
        .bind(&message.role)
        .bind(
            json!({
                "content": &message.content,
                "context": request.context.clone(),
                "occurred_at": occurred_at,
            })
            .to_string(),
        )
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }

    let outbox_id = Uuid::now_v7().to_string();
    let payload = serde_json::to_value(OutboxEnvelope {
        schema_version: 1,
        event_id: outbox_id.clone(),
        occurred_at,
        request: request.clone(),
    })?;

    sqlx::query(
        "INSERT INTO memory_outbox (id, tenant_id, event_type, aggregate_id, idempotency_key, payload, state, retry_count, next_retry_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, ?8, ?8)",
    )
    .bind(&outbox_id)
    .bind(&request.tenant_id)
    .bind("memory.ingest")
    .bind(&outbox_id)
    .bind(&idempotency_key)
    .bind(payload.to_string())
    .bind(&now)
    .bind(&now)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(IngestResponse {
        accepted: true,
        event_id: outbox_id.clone(),
        task_id: outbox_id,
    })
}

pub async fn persist_feedback_sqlite(
    pool: &Pool<Sqlite>,
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

    if let Some(existing_id) = sqlx::query_scalar::<_, String>(
        "SELECT id FROM memory_outbox WHERE idempotency_key = ?1 LIMIT 1",
    )
    .bind(&idempotency_key)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return Ok(IngestResponse {
            accepted: true,
            event_id: existing_id.clone(),
            task_id: existing_id,
        });
    }

    let now = Utc::now();
    let now_str = now.to_rfc3339();
    let feedback_id = Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO memory_feedback (id, tenant_id, entity_id, process_id, turn_id, used_items, helpful, harmful, note, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(&feedback_id)
    .bind(&request.tenant_id)
    .bind(&request.entity_id)
    .bind(&request.process_id)
    .bind(&request.turn_id)
    .bind(json!(request.used_items).to_string())
    .bind(json!(request.helpful).to_string())
    .bind(json!(request.harmful).to_string())
    .bind(&request.note)
    .bind(&now_str)
    .execute(&mut *tx)
    .await?;

    let outbox_id = Uuid::now_v7().to_string();
    let payload = serde_json::to_value(OutboxFeedbackEnvelope {
        schema_version: 1,
        event_id: outbox_id.clone(),
        occurred_at: now,
        request: request.clone(),
    })?;

    sqlx::query(
        "INSERT INTO memory_outbox (id, tenant_id, event_type, aggregate_id, idempotency_key, payload, state, retry_count, next_retry_at, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'pending', 0, ?7, ?8, ?8)",
    )
    .bind(&outbox_id)
    .bind(&request.tenant_id)
    .bind("memory.feedback")
    .bind(&feedback_id)
    .bind(&idempotency_key)
    .bind(payload.to_string())
    .bind(&now_str)
    .bind(&now_str)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    Ok(IngestResponse {
        accepted: true,
        event_id: outbox_id.clone(),
        task_id: outbox_id,
    })
}

pub async fn recall_memories_sqlite(
    pool: &Pool<Sqlite>,
    request: &RecallRequest,
    recall_candidate_limit: i64,
) -> Result<RecallResponse, CoreError> {
    let candidates = fetch_candidates_sqlite(
        pool,
        request,
        recall_candidate_limit,
        should_prefer_structured(&request.intent),
        None,
    )
    .await?;
    build_recall_response(request, candidates, HashMap::new(), "sql-first")
}

pub async fn recall_memories_hybrid_sqlite(
    pool: &Pool<Sqlite>,
    request: &RecallRequest,
    recall_candidate_limit: i64,
    vector_scores: HashMap<Uuid, f64>,
) -> Result<RecallResponse, CoreError> {
    let mut merged = HashMap::<Uuid, MemoryCandidate>::new();
    let structured = should_prefer_structured(&request.intent);
    let sql_candidates =
        fetch_candidates_sqlite(pool, request, recall_candidate_limit, structured, None).await?;
    for candidate in sql_candidates {
        merged.insert(candidate.id, candidate);
    }

    if !vector_scores.is_empty() {
        let ids = vector_scores
            .keys()
            .map(Uuid::to_string)
            .collect::<Vec<_>>();
        let vector_candidates = fetch_candidates_sqlite(
            pool,
            request,
            recall_candidate_limit,
            structured,
            Some(&ids),
        )
        .await?;
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

pub async fn search_vector_scores_sqlite(
    pool: &Pool<Sqlite>,
    vector: &[f32],
    limit: usize,
    namespaces: &[String],
) -> Result<Vec<(Uuid, f64)>, CoreError> {
    if vector.is_empty() || namespaces.is_empty() {
        return Ok(Vec::new());
    }

    let mut query = QueryBuilder::<Sqlite>::new(
        "SELECT e.memory_id AS memory_id, v.distance AS distance \
         FROM vector_full_scan('memory_embedding', 'embedding_blob', ",
    );
    query.push_bind(vec_to_blob(vector));
    query.push(", ");
    query.push_bind(limit as i64);
    query.push(") AS v JOIN memory_embedding e ON e.rowid = v.rowid WHERE e.namespace IN (");
    {
        let mut separated = query.separated(", ");
        for namespace in namespaces {
            separated.push_bind(namespace);
        }
    }
    query.push(")");
    let rows = query
        .build_query_as::<SqliteVectorSearchRow>()
        .fetch_all(pool)
        .await?;

    let mut result = Vec::with_capacity(rows.len());
    for row in rows {
        let Ok(id) = Uuid::parse_str(row.memory_id.trim()) else {
            continue;
        };
        let distance = row.distance.max(0.0);
        let score = 1.0 / (1.0 + distance);
        result.push((id, score.clamp(0.0, 1.0)));
    }
    Ok(result)
}

pub async fn claim_outbox_batch_sqlite(
    pool: &Pool<Sqlite>,
    batch_size: i64,
) -> Result<Vec<OutboxMessage>, CoreError> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query_as::<_, SqliteOutboxRow>(
        "UPDATE memory_outbox \
         SET state = 'processing', updated_at = ?1 \
         WHERE id IN ( \
            SELECT id FROM memory_outbox \
            WHERE state = 'pending' AND next_retry_at <= ?1 \
            ORDER BY created_at \
            LIMIT ?2 \
         ) \
         RETURNING id, tenant_id, event_type, payload, retry_count",
    )
    .bind(&now)
    .bind(batch_size)
    .fetch_all(pool)
    .await?;

    let mut parsed = Vec::with_capacity(rows.len());
    for row in rows {
        parsed.push(parse_outbox_row(row)?);
    }
    Ok(parsed)
}

pub async fn mark_outbox_done_sqlite(
    pool: &Pool<Sqlite>,
    outbox_id: Uuid,
) -> Result<(), CoreError> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE memory_outbox \
         SET state = 'done', updated_at = ?2, last_error = NULL \
         WHERE id = ?1",
    )
    .bind(outbox_id.to_string())
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_outbox_retry_sqlite(
    pool: &Pool<Sqlite>,
    outbox_id: Uuid,
    current_retry_count: i32,
    max_retry: i32,
    error_message: &str,
) -> Result<(), CoreError> {
    let next_retry_count = current_retry_count + 1;
    let now = Utc::now();
    let now_str = now.to_rfc3339();
    if next_retry_count >= max_retry {
        sqlx::query(
            "UPDATE memory_outbox \
             SET state = 'dead', retry_count = ?2, last_error = ?3, updated_at = ?4 \
             WHERE id = ?1",
        )
        .bind(outbox_id.to_string())
        .bind(next_retry_count)
        .bind(error_message)
        .bind(now_str)
        .execute(pool)
        .await?;
        return Ok(());
    }

    let wait_seconds = 2_i64.pow(next_retry_count.min(10) as u32);
    let next_retry_at = (now + ChronoDuration::seconds(wait_seconds)).to_rfc3339();
    sqlx::query(
        "UPDATE memory_outbox \
         SET state = 'pending', retry_count = ?2, last_error = ?3, next_retry_at = ?4, updated_at = ?5 \
         WHERE id = ?1",
    )
    .bind(outbox_id.to_string())
    .bind(next_retry_count)
    .bind(error_message)
    .bind(next_retry_at)
    .bind(now.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn upsert_memory_item_sqlite(
    pool: &Pool<Sqlite>,
    memory: &ExtractedMemory,
) -> Result<Uuid, CoreError> {
    let namespace = build_namespace(&memory.tenant_id, &memory.entity_id, &memory.process_id);
    let now = Utc::now().to_rfc3339();
    let new_id = Uuid::now_v7().to_string();
    let properties = memory.properties.to_string();
    let expires_at = memory.expires_at.map(|value| value.to_rfc3339());
    let memory_id = sqlx::query_scalar::<_, String>(
        "INSERT INTO memory_item ( \
            id, tenant_id, entity_id, process_id, namespace, session_id, memory_type, category, content, normalized_content, \
            importance, confidence, source, status, fingerprint_hash, expires_at, properties, first_seen_at, last_seen_at, created_at, updated_at \
         ) VALUES ( \
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
            ?11, ?12, ?13, 'active', ?14, ?15, ?16, ?17, ?17, ?17, ?17 \
         ) \
         ON CONFLICT(namespace, memory_type, fingerprint_hash) DO UPDATE SET \
            content = excluded.content, \
            normalized_content = excluded.normalized_content, \
            importance = MIN(100, MAX(memory_item.importance, excluded.importance) + 1), \
            confidence = MAX(memory_item.confidence, excluded.confidence), \
            mention_count = memory_item.mention_count + 1, \
            last_seen_at = ?17, \
            version = memory_item.version + 1, \
            properties = excluded.properties, \
            expires_at = COALESCE(excluded.expires_at, memory_item.expires_at), \
            updated_at = ?17 \
         RETURNING id",
    )
    .bind(new_id)
    .bind(&memory.tenant_id)
    .bind(&memory.entity_id)
    .bind(&memory.process_id)
    .bind(namespace)
    .bind(&memory.session_id)
    .bind(&memory.memory_type)
    .bind(&memory.category)
    .bind(&memory.content)
    .bind(&memory.normalized_content)
    .bind(memory.importance)
    .bind(memory.confidence)
    .bind(&memory.source)
    .bind(fingerprint_of(memory))
    .bind(expires_at)
    .bind(properties)
    .bind(now)
    .fetch_one(pool)
    .await?;

    Uuid::parse_str(memory_id.trim())
        .map_err(|error| CoreError::InvalidRequest(format!("invalid uuid from sqlite: {error}")))
}

pub async fn persist_embedding_sqlite(
    pool: &Pool<Sqlite>,
    record: &EmbeddingRecord,
    vector: &[f32],
) -> Result<(), CoreError> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO memory_embedding ( \
            memory_id, tenant_id, namespace, model, dims, embedding_json, embedding_blob, recall_text, metadata, created_at, updated_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10) \
         ON CONFLICT(memory_id) DO UPDATE SET \
            namespace = excluded.namespace, \
            model = excluded.model, \
            dims = excluded.dims, \
            embedding_json = excluded.embedding_json, \
            embedding_blob = excluded.embedding_blob, \
            recall_text = excluded.recall_text, \
            metadata = excluded.metadata, \
            updated_at = ?10",
    )
    .bind(record.memory_id.to_string())
    .bind(&record.tenant_id)
    .bind(&record.namespace)
    .bind(&record.model)
    .bind(record.dims)
    .bind(record.embedding.to_string())
    .bind(vec_to_blob(vector))
    .bind(&record.recall_text)
    .bind(record.metadata.to_string())
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn append_audit_log_sqlite(
    pool: &Pool<Sqlite>,
    input: &AuditLogInput,
) -> Result<(), CoreError> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO memory_audit_log ( \
            id, tenant_id, entity_id, process_id, request_id, actor, action, payload, created_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(&input.tenant_id)
    .bind(&input.entity_id)
    .bind(&input.process_id)
    .bind(&input.request_id)
    .bind(&input.actor)
    .bind(&input.action)
    .bind(input.payload.to_string())
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn persist_llm_usage_sqlite(
    pool: &Pool<Sqlite>,
    input: &LlmUsageInput,
) -> Result<(), CoreError> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO memory_llm_usage ( \
            id, tenant_id, entity_id, process_id, event_type, event_id, operation, provider, model, \
            prompt_tokens, completion_tokens, total_tokens, payload, created_at \
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
    )
    .bind(Uuid::now_v7().to_string())
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
    .bind(input.payload.to_string())
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

fn parse_outbox_row(row: SqliteOutboxRow) -> Result<OutboxMessage, CoreError> {
    let id = Uuid::parse_str(row.id.trim()).map_err(|error| {
        CoreError::InvalidRequest(format!("invalid outbox id from sqlite: {error}"))
    })?;
    let payload = serde_json::from_str::<Value>(&row.payload).map_err(|error| {
        CoreError::InvalidRequest(format!("invalid outbox payload json: {error}"))
    })?;
    Ok(OutboxMessage {
        id,
        tenant_id: row.tenant_id,
        event_type: row.event_type,
        payload,
        retry_count: row.retry_count,
    })
}

async fn fetch_candidates_sqlite(
    pool: &Pool<Sqlite>,
    request: &RecallRequest,
    recall_candidate_limit: i64,
    structured_only: bool,
    ids: Option<&[String]>,
) -> Result<Vec<MemoryCandidate>, CoreError> {
    let namespaces =
        allowed_namespaces(&request.tenant_id, &request.entity_id, &request.process_id);
    let now = Utc::now().to_rfc3339();

    let mut query = QueryBuilder::<Sqlite>::new(
        "SELECT id, process_id, memory_type, content, importance, confidence, last_seen_at, source, properties \
         FROM memory_item WHERE namespace IN (",
    );
    {
        let mut separated = query.separated(", ");
        for namespace in &namespaces {
            separated.push_bind(namespace);
        }
    }
    query.push(") AND status = 'active' AND (expires_at IS NULL OR expires_at > ");
    query.push_bind(now);
    query.push(")");
    if structured_only {
        query.push(" AND memory_type IN ('preference', 'rule')");
    }
    if let Some(list) = ids
        && !list.is_empty()
    {
        query.push(" AND id IN (");
        let mut separated = query.separated(", ");
        for id in list {
            separated.push_bind(id);
        }
        query.push(")");
    }
    query.push(" ORDER BY last_seen_at DESC, importance DESC LIMIT ");
    query.push_bind(recall_candidate_limit);

    let rows = query
        .build_query_as::<SqliteMemoryCandidateRow>()
        .fetch_all(pool)
        .await?;
    let mut parsed = Vec::with_capacity(rows.len());
    for row in rows {
        parsed.push(parse_candidate_row(row)?);
    }
    Ok(parsed)
}

fn parse_candidate_row(row: SqliteMemoryCandidateRow) -> Result<MemoryCandidate, CoreError> {
    let id = Uuid::parse_str(row.id.trim())
        .map_err(|error| CoreError::InvalidRequest(format!("invalid memory id: {error}")))?;
    let last_seen_at = DateTime::parse_from_rfc3339(row.last_seen_at.trim())
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| CoreError::InvalidRequest(format!("invalid last_seen_at: {error}")))?;
    let properties = serde_json::from_str::<Value>(&row.properties)
        .map_err(|error| CoreError::InvalidRequest(format!("invalid properties json: {error}")))?;
    Ok(MemoryCandidate {
        id,
        process_id: row.process_id,
        memory_type: row.memory_type,
        content: row.content,
        importance: row.importance,
        confidence: row.confidence,
        last_seen_at,
        source: row.source,
        properties,
    })
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

fn vec_to_blob(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}
