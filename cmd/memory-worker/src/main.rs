use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, anyhow};
use memory_core::model::OutboxEnvelope;
use memory_core::{
    EmbeddingRecord, ExtractedMemory, IngestRequest, LlmUsageInput, OutboxMessage, ServiceConfig,
    claim_outbox_batch, connect_pool, list_reconcile_candidates, mark_outbox_done,
    mark_outbox_retry, persist_embedding, persist_llm_usage, upsert_memory_item,
};
use reqwest::StatusCode;
use serde_json::{Map, Value, json};
use tracing::{error, info, warn};

#[derive(Clone)]
struct WorkerState {
    pool: memory_core::Pool<memory_core::Postgres>,
    config: Arc<ServiceConfig>,
    extractor: OpenAiExtractor,
    embedder: OpenAiEmbedder,
    qdrant: Qdrant,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = ServiceConfig::from_env().context("failed to load configuration")?;
    init_tracing(&config.log_format);

    let pool = connect_pool(&config.database_url)
        .await
        .context("failed to connect database")?;

    let extractor = OpenAiExtractor::new(&config);
    let embedder = OpenAiEmbedder::new(&config);
    let qdrant = Qdrant::new(&config);
    qdrant.ensure_collection(config.embedding_dims).await?;

    let state = WorkerState {
        pool,
        config: Arc::new(config.clone()),
        extractor,
        embedder,
        qdrant,
    };

    let mut ticker = tokio::time::interval(Duration::from_millis(config.worker_poll_interval_ms));
    let mut last_reconcile =
        tokio::time::Instant::now() - Duration::from_secs(config.reconcile_interval_seconds);
    info!(
        poll_interval_ms = config.worker_poll_interval_ms,
        batch_size = config.worker_batch_size,
        max_retry = config.worker_max_retry,
        reconcile_enabled = config.reconcile_enabled,
        reconcile_interval_seconds = config.reconcile_interval_seconds,
        reconcile_batch_size = config.reconcile_batch_size,
        openai_base_url = %config.openai_base_url,
        extract_model = %config.openai_extract_model,
        embedding_model = %config.openai_embedding_model,
        embedding_dims = config.embedding_dims,
        qdrant_collection = %config.qdrant_collection(),
        "memory-worker started"
    );

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("memory-worker shutting down");
                break;
            }
            _ = ticker.tick() => {
                if let Err(error) = run_once(&state).await {
                    error!(error = %error, "worker tick failed");
                }
                if state.config.reconcile_enabled
                    && last_reconcile.elapsed().as_secs() >= state.config.reconcile_interval_seconds
                {
                    if let Err(error) = reconcile_once(&state).await {
                        warn!(error = %error, "reconciliation tick failed");
                    }
                    last_reconcile = tokio::time::Instant::now();
                }
            }
        }
    }

    Ok(())
}

async fn run_once(state: &WorkerState) -> anyhow::Result<()> {
    let batch = claim_outbox_batch(&state.pool, state.config.worker_batch_size)
        .await
        .map_err(|error| anyhow!(error))?;

    if batch.is_empty() {
        return Ok(());
    }

    info!(batch_size = batch.len(), "claimed outbox batch");

    for message in batch {
        if let Err(error) = process_message(state, &message).await {
            warn!(
                outbox_id = %message.id,
                retry_count = message.retry_count,
                error = %error,
                "processing outbox message failed"
            );
            let reason = truncate_error(&error.to_string());
            mark_outbox_retry(
                &state.pool,
                message.id,
                message.retry_count,
                state.config.worker_max_retry,
                &reason,
            )
            .await
            .map_err(|mark_error| anyhow!(mark_error))?;
            continue;
        }

        mark_outbox_done(&state.pool, message.id)
            .await
            .map_err(|error| anyhow!(error))?;
    }

    Ok(())
}

async fn process_message(state: &WorkerState, message: &OutboxMessage) -> anyhow::Result<()> {
    match message.event_type.as_str() {
        "memory.ingest" => process_ingest(state, message).await,
        "memory.feedback" => {
            info!(outbox_id = %message.id, "feedback event acknowledged");
            Ok(())
        }
        _ => {
            warn!(
                outbox_id = %message.id,
                event_type = %message.event_type,
                "unknown outbox event ignored"
            );
            Ok(())
        }
    }
}

async fn process_ingest(state: &WorkerState, message: &OutboxMessage) -> anyhow::Result<()> {
    let envelope: OutboxEnvelope = serde_json::from_value(message.payload.clone())
        .context("failed to decode outbox payload")?;

    let extraction = state.extractor.extract(&envelope.request).await?;
    if let Some(usage) = extraction.usage {
        info!(
            outbox_id = %message.id,
            model = %extraction.model.as_deref().unwrap_or(&state.config.openai_extract_model),
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            total_tokens = usage.total_tokens,
            "recorded extraction token usage"
        );
        persist_llm_usage(
            &state.pool,
            &LlmUsageInput {
                tenant_id: envelope.request.tenant_id.clone(),
                entity_id: envelope.request.entity_id.clone(),
                process_id: envelope.request.process_id.clone(),
                event_type: "ingest".to_owned(),
                event_id: Some(message.id.to_string()),
                operation: "extract".to_owned(),
                provider: "openai-compatible".to_owned(),
                model: extraction
                    .model
                    .clone()
                    .unwrap_or_else(|| state.config.openai_extract_model.clone()),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                payload: json!({
                    "outbox_id": message.id.to_string()
                }),
            },
        )
        .await
        .map_err(|error| anyhow!(error))?;
    }

    let extracted = extraction.items;
    if extracted.is_empty() {
        info!(outbox_id = %message.id, "no memory extracted");
        return Ok(());
    }

    info!(
        outbox_id = %message.id,
        extracted_count = extracted.len(),
        tenant_id = %envelope.request.tenant_id,
        entity_id = %envelope.request.entity_id,
        "processing extracted memories"
    );

    let texts = extracted
        .iter()
        .map(|memory| memory.content.clone())
        .collect::<Vec<_>>();
    let embedding_batch = state.embedder.embed_many(&texts).await?;
    if let Some(usage) = embedding_batch.usage {
        info!(
            outbox_id = %message.id,
            model = %embedding_batch.model.as_deref().unwrap_or(&state.config.openai_embedding_model),
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            total_tokens = usage.total_tokens,
            "recorded embedding token usage"
        );
        persist_llm_usage(
            &state.pool,
            &LlmUsageInput {
                tenant_id: envelope.request.tenant_id.clone(),
                entity_id: envelope.request.entity_id.clone(),
                process_id: envelope.request.process_id.clone(),
                event_type: "ingest".to_owned(),
                event_id: Some(message.id.to_string()),
                operation: "embed_ingest".to_owned(),
                provider: "openai-compatible".to_owned(),
                model: embedding_batch
                    .model
                    .clone()
                    .unwrap_or_else(|| state.config.openai_embedding_model.clone()),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                payload: json!({
                    "outbox_id": message.id.to_string(),
                    "input_count": texts.len()
                }),
            },
        )
        .await
        .map_err(|error| anyhow!(error))?;
    }
    let vectors = embedding_batch.vectors;

    if vectors.len() != extracted.len() {
        return Err(anyhow!(
            "embedding count mismatch: extracted={}, vectors={}",
            extracted.len(),
            vectors.len()
        ));
    }

    for (memory, vector) in extracted.into_iter().zip(vectors.into_iter()) {
        if vector.len() != state.config.embedding_dims {
            return Err(anyhow!(
                "openai embedding dims mismatch: expected={}, got={}",
                state.config.embedding_dims,
                vector.len()
            ));
        }

        let memory_id = upsert_memory_item(&state.pool, &memory)
            .await
            .map_err(|error| anyhow!(error))?;

        let metadata = json!({
            "tenant_id": memory.tenant_id,
            "entity_id": memory.entity_id,
            "process_id": memory.process_id,
            "memory_type": memory.memory_type,
            "source": memory.source,
            "collection": state.config.qdrant_collection(),
        });

        persist_embedding(
            &state.pool,
            &EmbeddingRecord {
                memory_id,
                tenant_id: memory.tenant_id.clone(),
                model: state.config.openai_embedding_model.clone(),
                dims: state.config.embedding_dims as i32,
                embedding: json!(vector),
                recall_text: memory.content.clone(),
                metadata: metadata.clone(),
            },
        )
        .await
        .map_err(|error| anyhow!(error))?;

        state
            .qdrant
            .upsert_point(memory_id, &vector, metadata)
            .await
            .with_context(|| format!("qdrant upsert failed for memory_id={memory_id}"))?;
    }

    Ok(())
}

async fn reconcile_once(state: &WorkerState) -> anyhow::Result<()> {
    let candidates = list_reconcile_candidates(&state.pool, state.config.reconcile_batch_size)
        .await
        .map_err(|error| anyhow!(error))?;
    if candidates.is_empty() {
        return Ok(());
    }

    let candidate_count = candidates.len();
    info!(count = candidate_count, "starting reconciliation batch");
    for candidate in candidates {
        if candidate.vector.len() != state.config.embedding_dims {
            warn!(
                memory_id = %candidate.memory_id,
                expected_dims = state.config.embedding_dims,
                got_dims = candidate.vector.len(),
                "skip reconciliation candidate due to embedding dims mismatch"
            );
            continue;
        }

        let mut metadata = candidate
            .metadata
            .as_object()
            .cloned()
            .unwrap_or_else(Map::new);
        metadata.insert(
            "tenant_id".to_owned(),
            Value::String(candidate.tenant_id.clone()),
        );
        metadata.insert(
            "entity_id".to_owned(),
            Value::String(candidate.entity_id.clone()),
        );
        metadata.insert(
            "process_id".to_owned(),
            Value::String(candidate.process_id.clone()),
        );
        metadata.insert(
            "memory_type".to_owned(),
            Value::String(candidate.memory_type.clone()),
        );
        metadata.insert("source".to_owned(), Value::String(candidate.source.clone()));
        metadata.insert(
            "collection".to_owned(),
            Value::String(state.config.qdrant_collection()),
        );

        state
            .qdrant
            .upsert_point(
                candidate.memory_id,
                &candidate.vector,
                Value::Object(metadata.clone()),
            )
            .await
            .with_context(|| {
                format!(
                    "qdrant reconciliation upsert failed for {}",
                    candidate.memory_id
                )
            })?;
    }

    info!(count = candidate_count, "reconciliation batch completed");
    Ok(())
}

#[derive(Clone)]
struct OpenAiExtractor {
    base_url: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
struct OpenAiUsage {
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
}

#[derive(Debug, Clone)]
struct ExtractionResult {
    items: Vec<ExtractedMemory>,
    usage: Option<OpenAiUsage>,
    model: Option<String>,
}

#[derive(Debug, Clone)]
struct EmbeddingBatch {
    vectors: Vec<Vec<f32>>,
    usage: Option<OpenAiUsage>,
    model: Option<String>,
}

impl OpenAiExtractor {
    fn new(config: &ServiceConfig) -> Self {
        Self {
            base_url: config.openai_base_url.clone(),
            api_key: config.openai_api_key.clone(),
            model: config.openai_extract_model.clone(),
            client: reqwest::Client::new(),
        }
    }

    async fn extract(&self, request: &IngestRequest) -> anyhow::Result<ExtractionResult> {
        let url = format!("{}/chat/completions", self.base_url);
        let system_prompt = "You extract durable user memory from conversation. Return strict JSON object with key items. Each item fields: memory_type (fact|preference|rule|skill|event), content, confidence (0-1), importance (0-100), scope (shared|process), properties (object).";
        let user_prompt = format!(
            "tenant_id={}\nentity_id={}\nprocess_id={}\nmessages=\n{}",
            request.tenant_id,
            request.entity_id,
            request.process_id,
            format_messages(request)
        );

        let body = json!({
            "model": self.model,
            "temperature": 0,
            "response_format": {"type": "json_object"},
            "messages": [
                {"role": "system", "content": system_prompt},
                {"role": "user", "content": user_prompt}
            ]
        });

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        let status = response.status();
        let raw: Value = response.json().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "openai extraction failed: status={}, body={}",
                status,
                raw
            ));
        }

        let content = raw
            .get("choices")
            .and_then(|value| value.as_array())
            .and_then(|choices| choices.first())
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(|content| content.as_str())
            .ok_or_else(|| anyhow!("openai extraction response missing content"))?;
        let usage = parse_openai_usage(&raw);
        let model = raw
            .get("model")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);

        let items = parse_extracted_items(content, request)?;
        Ok(ExtractionResult {
            items,
            usage,
            model,
        })
    }
}

fn format_messages(request: &IngestRequest) -> String {
    request
        .messages
        .iter()
        .map(|message| format!("{}: {}", message.role, message.content))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_extracted_items(
    content: &str,
    request: &IngestRequest,
) -> anyhow::Result<Vec<ExtractedMemory>> {
    let parsed = serde_json::from_str::<Value>(content)
        .or_else(|_| serde_json::from_str::<Value>(&extract_json_fragment(content)))
        .map_err(|error| anyhow!("failed to parse extraction JSON: {}", error))?;

    let items = parsed
        .get("items")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow!("extraction result missing items array"))?;

    let mut dedup = HashSet::new();
    let mut memories = Vec::new();

    for item in items {
        let Some(content) = item.get("content").and_then(|value| value.as_str()) else {
            continue;
        };
        let normalized = normalize_content(content);
        if normalized.len() < 2 {
            continue;
        }

        let memory_type =
            normalize_memory_type(item.get("memory_type").and_then(|value| value.as_str()));
        let scope = item
            .get("scope")
            .and_then(|value| value.as_str())
            .unwrap_or("shared");
        let process_id = if scope == "process" {
            request.process_id.clone()
        } else {
            "__shared__".to_owned()
        };

        let fingerprint = format!("{}:{}", memory_type, normalized);
        if dedup.contains(&fingerprint) {
            continue;
        }
        dedup.insert(fingerprint);

        let importance = item
            .get("importance")
            .and_then(|value| value.as_i64())
            .unwrap_or(default_importance(memory_type))
            .clamp(0, 100) as i16;
        let confidence = item
            .get("confidence")
            .and_then(|value| value.as_f64())
            .unwrap_or(0.7)
            .clamp(0.0, 1.0);

        let properties = item
            .get("properties")
            .and_then(|value| value.as_object())
            .cloned()
            .unwrap_or_else(Map::new);

        memories.push(ExtractedMemory {
            tenant_id: request.tenant_id.clone(),
            entity_id: request.entity_id.clone(),
            process_id,
            session_id: request.session_id.clone(),
            memory_type: memory_type.to_owned(),
            category: memory_type.to_owned(),
            content: content.trim().to_owned(),
            normalized_content: normalized,
            importance,
            confidence,
            source: "conversation".to_owned(),
            expires_at: None,
            properties: Value::Object(properties),
        });
    }

    Ok(memories)
}

fn extract_json_fragment(raw: &str) -> String {
    let start = raw.find('{').unwrap_or(0);
    let end = raw.rfind('}').map(|index| index + 1).unwrap_or(raw.len());
    raw[start..end].to_owned()
}

fn normalize_memory_type(raw: Option<&str>) -> &'static str {
    match raw.unwrap_or("fact") {
        "preference" => "preference",
        "rule" => "rule",
        "skill" => "skill",
        "event" => "event",
        _ => "fact",
    }
}

fn default_importance(memory_type: &str) -> i64 {
    match memory_type {
        "rule" => 85,
        "preference" => 75,
        "skill" => 70,
        "event" => 66,
        _ => 62,
    }
}

fn normalize_content(content: &str) -> String {
    content
        .trim()
        .to_lowercase()
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Clone)]
struct OpenAiEmbedder {
    base_url: String,
    api_key: String,
    model: String,
    dims: usize,
    client: reqwest::Client,
}

impl OpenAiEmbedder {
    fn new(config: &ServiceConfig) -> Self {
        Self {
            base_url: config.openai_base_url.clone(),
            api_key: config.openai_api_key.clone(),
            model: config.openai_embedding_model.clone(),
            dims: config.embedding_dims,
            client: reqwest::Client::new(),
        }
    }

    async fn embed_many(&self, input: &[String]) -> anyhow::Result<EmbeddingBatch> {
        if input.is_empty() {
            return Ok(EmbeddingBatch {
                vectors: Vec::new(),
                usage: None,
                model: Some(self.model.clone()),
            });
        }

        let url = format!("{}/embeddings", self.base_url);
        let request_body = json!({
            "model": self.model,
            "input": input,
            "dimensions": self.dims,
        });

        let response = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&request_body)
            .send()
            .await?;

        let status = response.status();
        let body: Value = response.json().await?;
        if !status.is_success() {
            return Err(anyhow!(
                "openai embedding request failed: status={}, body={}",
                status,
                body
            ));
        }

        let model = body
            .get("model")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
        let usage = parse_openai_usage(&body);
        let data = body
            .get("data")
            .and_then(|value| value.as_array())
            .ok_or_else(|| anyhow!("openai embedding response missing data array"))?;

        let mut vectors = Vec::with_capacity(data.len());
        for row in data {
            let values = row
                .get("embedding")
                .and_then(|value| value.as_array())
                .ok_or_else(|| anyhow!("openai embedding row missing embedding"))?;

            let mut vector = Vec::with_capacity(values.len());
            for value in values {
                let number = value
                    .as_f64()
                    .ok_or_else(|| anyhow!("openai embedding contains non-numeric value"))?;
                vector.push(number as f32);
            }
            vectors.push(vector);
        }

        Ok(EmbeddingBatch {
            vectors,
            usage,
            model,
        })
    }
}

fn parse_openai_usage(body: &Value) -> Option<OpenAiUsage> {
    let usage = body.get("usage")?;
    let prompt_tokens = parse_token(usage.get("prompt_tokens"))?;
    let completion_tokens = parse_token(usage.get("completion_tokens")).unwrap_or(0);
    let total_tokens = parse_token(usage.get("total_tokens"))
        .unwrap_or(prompt_tokens.saturating_add(completion_tokens));
    Some(OpenAiUsage {
        prompt_tokens,
        completion_tokens,
        total_tokens,
    })
}

fn parse_token(value: Option<&Value>) -> Option<i32> {
    let number = value.and_then(|item| item.as_i64())?;
    let safe = number.clamp(0, i64::from(i32::MAX));
    i32::try_from(safe).ok()
}

fn truncate_error(message: &str) -> String {
    let max = 700;
    if message.len() <= max {
        return message.to_owned();
    }
    format!("{}...", &message[..max])
}

#[derive(Clone)]
struct Qdrant {
    base_url: String,
    collection: String,
    client: reqwest::Client,
}

impl Qdrant {
    fn new(config: &ServiceConfig) -> Self {
        Self {
            base_url: config.qdrant_url.clone(),
            collection: config.qdrant_collection(),
            client: reqwest::Client::new(),
        }
    }

    async fn ensure_collection(&self, dims: usize) -> anyhow::Result<()> {
        let get_url = format!("{}/collections/{}", self.base_url, self.collection);
        let get_response = self.client.get(&get_url).send().await?;
        let get_status = get_response.status();
        if get_status == StatusCode::OK {
            info!(collection = %self.collection, "qdrant collection exists");
            return Ok(());
        }

        if get_status != StatusCode::NOT_FOUND {
            let body = get_response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "qdrant collection check failed: status={}, body={}",
                get_status,
                body
            ));
        }

        let create_payload = json!({
            "vectors": {
                "size": dims,
                "distance": "Cosine"
            }
        });
        let create_response = self
            .client
            .put(&get_url)
            .json(&create_payload)
            .send()
            .await?;
        let create_status = create_response.status();

        if create_status.is_success() {
            info!(collection = %self.collection, dims, "qdrant collection created");
            return Ok(());
        }

        let body = create_response.text().await.unwrap_or_default();
        Err(anyhow!(
            "qdrant create collection failed: status={}, body={}",
            create_status,
            body
        ))
    }

    async fn upsert_point(
        &self,
        memory_id: uuid::Uuid,
        vector: &[f32],
        payload: Value,
    ) -> anyhow::Result<()> {
        let url = format!(
            "{}/collections/{}/points?wait=true",
            self.base_url, self.collection
        );
        let body = json!({
            "points": [
                {
                    "id": memory_id.to_string(),
                    "vector": vector,
                    "payload": payload,
                }
            ]
        });

        let response = self.client.put(&url).json(&body).send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }

        let response_body = response.text().await.unwrap_or_default();
        Err(anyhow!(
            "qdrant upsert failed: status={}, body={}",
            status,
            response_body
        ))
    }
}

fn init_tracing(log_format: &str) {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    if log_format == "json" {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .json()
            .with_current_span(true)
            .with_span_list(true)
            .init();
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .compact()
        .init();
}
