use std::{collections::HashMap, net::SocketAddr, sync::Arc};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use memory_core::{
    AuditLogInput, FeedbackRequest, HealthResponse, IngestRequest, LlmUsageInput, RecallRequest,
    ServiceConfig, allowed_namespaces, append_audit_log, connect_pool, ingest_event,
    persist_feedback, persist_llm_usage, ping, recall_memories, recall_memories_hybrid,
};
use serde_json::{Value, json};
use tracing::{error, info, warn};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    pool: memory_core::Pool<memory_core::Postgres>,
    config: Arc<ServiceConfig>,
    embedder: OpenAiEmbedder,
    qdrant: QdrantSearcher,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = ServiceConfig::from_env().context("failed to load configuration")?;
    init_tracing(&config.log_format);

    let pool = connect_pool(&config.database_url)
        .await
        .context("failed to connect database")?;

    let state = AppState {
        pool,
        config: Arc::new(config.clone()),
        embedder: OpenAiEmbedder::new(&config),
        qdrant: QdrantSearcher::new(&config),
    };

    let app = Router::new()
        .route("/v1/memory/ingest", post(ingest_handler))
        .route("/v1/memory/recall", post(recall_handler))
        .route("/v1/memory/feedback", post(feedback_handler))
        .route("/v1/memory/health", get(health_handler))
        .with_state(state);

    let bind_addr: SocketAddr = config
        .api_bind_addr
        .parse()
        .with_context(|| format!("invalid API_BIND_ADDR: {}", config.api_bind_addr))?;

    let listener = tokio::net::TcpListener::bind(bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", bind_addr))?;
    info!(bind = %bind_addr, "memory-api listening");

    axum::serve(listener, app)
        .await
        .context("memory-api server error")?;

    Ok(())
}

async fn ingest_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<IngestRequest>,
) -> Result<(StatusCode, Json<memory_core::IngestResponse>), ApiError> {
    let auth = authenticate_headers(&headers, &state.config)?;
    validate_tenant_consistency(&auth.tenant_id, &request.tenant_id)?;

    info!(
        tenant_id = %request.tenant_id,
        entity_id = %request.entity_id,
        process_id = %request.process_id,
        message_count = request.messages.len(),
        "ingest request received"
    );

    let response = ingest_event(&state.pool, &request)
        .await
        .map_err(ApiError::from)?;
    append_audit_log(
        &state.pool,
        &AuditLogInput {
            tenant_id: request.tenant_id.clone(),
            entity_id: request.entity_id.clone(),
            process_id: request.process_id.clone(),
            request_id: auth.request_id,
            actor: auth.actor,
            action: "ingest.accepted".to_owned(),
            payload: json!({
                "event_id": response.event_id.clone(),
                "task_id": response.task_id.clone(),
                "message_count": request.messages.len(),
            }),
        },
    )
    .await
    .map_err(ApiError::from)?;

    info!(
        tenant_id = %request.tenant_id,
        event_id = %response.event_id,
        "ingest request accepted"
    );

    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn recall_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RecallRequest>,
) -> Result<Json<memory_core::RecallResponse>, ApiError> {
    let auth = authenticate_headers(&headers, &state.config)?;
    validate_tenant_consistency(&auth.tenant_id, &request.tenant_id)?;

    info!(
        tenant_id = %request.tenant_id,
        entity_id = %request.entity_id,
        process_id = %request.process_id,
        top_k = request.top_k,
        query = %request.query,
        "recall request received"
    );

    let route = choose_route(&request);
    let vector_result = if route == RecallRoute::Hybrid {
        build_vector_scores(&state, &request).await
    } else {
        VectorSearchResult::default()
    };
    if let Some(usage) = vector_result.usage {
        info!(
            tenant_id = %request.tenant_id,
            model = %vector_result.model.as_deref().unwrap_or(&state.config.openai_embedding_model),
            prompt_tokens = usage.prompt_tokens,
            completion_tokens = usage.completion_tokens,
            total_tokens = usage.total_tokens,
            "recorded recall embedding token usage"
        );
        if let Err(error) = persist_llm_usage(
            &state.pool,
            &LlmUsageInput {
                tenant_id: request.tenant_id.clone(),
                entity_id: request.entity_id.clone(),
                process_id: request.process_id.clone(),
                event_type: "recall".to_owned(),
                event_id: auth.request_id.clone(),
                operation: "embed_query".to_owned(),
                provider: "openai-compatible".to_owned(),
                model: vector_result
                    .model
                    .clone()
                    .unwrap_or_else(|| state.config.openai_embedding_model.clone()),
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                payload: json!({
                    "query": request.query.clone(),
                    "intent": request.intent.clone()
                }),
            },
        )
        .await
        {
            warn!(error = %error, "failed to persist recall embedding usage");
        }
    }
    let response = if route == RecallRoute::SqlFirst {
        recall_memories(&state.pool, &request, state.config.recall_candidate_limit)
            .await
            .map_err(ApiError::from)?
    } else {
        let vector_scores: HashMap<Uuid, f64> = vector_result.scores;
        if vector_scores.is_empty() {
            recall_memories(&state.pool, &request, state.config.recall_candidate_limit)
                .await
                .map_err(ApiError::from)?
        } else {
            recall_memories_hybrid(
                &state.pool,
                &request,
                state.config.recall_candidate_limit,
                vector_scores,
            )
            .await
            .map_err(ApiError::from)?
        }
    };
    append_audit_log(
        &state.pool,
        &AuditLogInput {
            tenant_id: request.tenant_id.clone(),
            entity_id: request.entity_id.clone(),
            process_id: request.process_id.clone(),
            request_id: auth.request_id,
            actor: auth.actor,
            action: "recall.completed".to_owned(),
            payload: json!({
                "query": request.query.clone(),
                "intent": request.intent.clone(),
                "route": response.debug.route.clone(),
                "returned": response.items.len(),
            }),
        },
    )
    .await
    .map_err(ApiError::from)?;

    info!(
        tenant_id = %request.tenant_id,
        route = %response.debug.route,
        returned = response.items.len(),
        "recall completed"
    );

    Ok(Json(response))
}

#[derive(Debug, Clone, Default)]
struct VectorSearchResult {
    scores: HashMap<Uuid, f64>,
    usage: Option<OpenAiUsage>,
    model: Option<String>,
}

async fn build_vector_scores(state: &AppState, request: &RecallRequest) -> VectorSearchResult {
    let embedding = match state.embedder.embed(&request.query).await {
        Ok(vector) => vector,
        Err(error) => {
            warn!(error = %error, "query embedding failed, fallback to sql-first");
            return VectorSearchResult::default();
        }
    };

    let vector_top_k = request.top_k.max(8) * 4;
    let namespaces =
        allowed_namespaces(&request.tenant_id, &request.entity_id, &request.process_id);
    match state
        .qdrant
        .search(&embedding.vector, vector_top_k, &namespaces)
        .await
    {
        Ok(rows) => VectorSearchResult {
            scores: rows.into_iter().collect::<HashMap<Uuid, f64>>(),
            usage: embedding.usage,
            model: embedding.model,
        },
        Err(error) => {
            warn!(error = %error, "qdrant search failed, fallback to sql-first");
            VectorSearchResult {
                scores: HashMap::new(),
                usage: embedding.usage,
                model: embedding.model,
            }
        }
    }
}

async fn feedback_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<FeedbackRequest>,
) -> Result<(StatusCode, Json<memory_core::IngestResponse>), ApiError> {
    let auth = authenticate_headers(&headers, &state.config)?;
    validate_tenant_consistency(&auth.tenant_id, &request.tenant_id)?;

    info!(
        tenant_id = %request.tenant_id,
        entity_id = %request.entity_id,
        process_id = %request.process_id,
        used_count = request.used_items.len(),
        helpful_count = request.helpful.len(),
        harmful_count = request.harmful.len(),
        "feedback request received"
    );

    let response = persist_feedback(&state.pool, &request)
        .await
        .map_err(ApiError::from)?;
    append_audit_log(
        &state.pool,
        &AuditLogInput {
            tenant_id: request.tenant_id.clone(),
            entity_id: request.entity_id.clone(),
            process_id: request.process_id.clone(),
            request_id: auth.request_id,
            actor: auth.actor,
            action: "feedback.accepted".to_owned(),
            payload: json!({
                "event_id": response.event_id.clone(),
                "task_id": response.task_id.clone(),
                "used_count": request.used_items.len(),
                "helpful_count": request.helpful.len(),
                "harmful_count": request.harmful.len(),
            }),
        },
    )
    .await
    .map_err(ApiError::from)?;

    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn health_handler(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    ping(&state.pool).await.map_err(ApiError::from)?;

    Ok(Json(HealthResponse {
        status: "ok".to_owned(),
        timestamp: chrono::Utc::now(),
    }))
}

#[derive(Clone)]
struct OpenAiEmbedder {
    base_url: String,
    api_key: String,
    model: String,
    dims: usize,
    client: reqwest::Client,
}

#[derive(Debug, Clone)]
struct OpenAiUsage {
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
}

#[derive(Debug, Clone)]
struct OpenAiEmbeddingResult {
    vector: Vec<f32>,
    usage: Option<OpenAiUsage>,
    model: Option<String>,
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

    async fn embed(&self, input: &str) -> anyhow::Result<OpenAiEmbeddingResult> {
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
            return Err(anyhow::anyhow!(
                "openai embedding request failed: status={}, body={}",
                status,
                body
            ));
        }

        parse_openai_embedding(body)
    }
}

fn parse_openai_embedding(body: Value) -> anyhow::Result<OpenAiEmbeddingResult> {
    let model = body
        .get("model")
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned);
    let usage = parse_openai_usage(&body);
    let data = body
        .get("data")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow::anyhow!("openai embedding response missing data array"))?;

    let first = data
        .first()
        .ok_or_else(|| anyhow::anyhow!("openai embedding response data is empty"))?;

    let values = first
        .get("embedding")
        .and_then(|value| value.as_array())
        .ok_or_else(|| anyhow::anyhow!("openai embedding response missing embedding array"))?;

    let mut vector = Vec::with_capacity(values.len());
    for value in values {
        let number = value
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("openai embedding contains non-numeric value"))?;
        vector.push(number as f32);
    }

    Ok(OpenAiEmbeddingResult {
        vector,
        usage,
        model,
    })
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

#[derive(Clone)]
struct QdrantSearcher {
    base_url: String,
    collection: String,
    client: reqwest::Client,
}

impl QdrantSearcher {
    fn new(config: &ServiceConfig) -> Self {
        Self {
            base_url: config.qdrant_url.clone(),
            collection: config.qdrant_collection(),
            client: reqwest::Client::new(),
        }
    }

    async fn search(
        &self,
        vector: &[f32],
        limit: usize,
        namespaces: &[String],
    ) -> anyhow::Result<Vec<(Uuid, f64)>> {
        let url = format!(
            "{}/collections/{}/points/search",
            self.base_url, self.collection
        );
        let payload = json!({
            "vector": vector,
            "limit": limit,
            "filter": {
                "must": [
                    {
                        "key": "namespace",
                        "match": {
                            "any": namespaces
                        }
                    }
                ]
            },
            "with_payload": false,
            "with_vector": false
        });

        let response = self.client.post(&url).json(&payload).send().await?;
        let status = response.status();
        let body: Value = response.json().await?;
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "qdrant search failed: status={}, body={}",
                status,
                body
            ));
        }

        let mut result = Vec::new();
        if let Some(rows) = body.get("result").and_then(|value| value.as_array()) {
            for row in rows {
                let Some(id_value) = row.get("id") else {
                    continue;
                };
                let Some(score_value) = row.get("score").and_then(|value| value.as_f64()) else {
                    continue;
                };
                if let Some(id) = parse_qdrant_uuid(id_value) {
                    result.push((id, score_value));
                }
            }
        }

        Ok(result)
    }
}

fn parse_qdrant_uuid(value: &Value) -> Option<Uuid> {
    if let Some(raw) = value.as_str() {
        return Uuid::parse_str(raw).ok();
    }

    if let Some(raw) = value.get("uuid").and_then(|inner| inner.as_str()) {
        return Uuid::parse_str(raw).ok();
    }

    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecallRoute {
    SqlFirst,
    Hybrid,
}

#[derive(Debug, Clone)]
struct AuthContext {
    tenant_id: String,
    request_id: Option<String>,
    actor: String,
}

fn choose_route(request: &RecallRequest) -> RecallRoute {
    let normalized = request
        .intent
        .as_ref()
        .map(|value| value.trim().to_lowercase())
        .unwrap_or_default();

    if matches!(
        normalized.as_str(),
        "policy" | "rule" | "preference" | "constraint" | "safety" | "decision"
    ) {
        return RecallRoute::SqlFirst;
    }

    RecallRoute::Hybrid
}

fn authenticate_headers(
    headers: &HeaderMap,
    config: &ServiceConfig,
) -> Result<AuthContext, ApiError> {
    let Some(authorization) = header_value(headers, AUTHORIZATION.as_str()) else {
        return Err(ApiError::unauthorized("missing Authorization header"));
    };
    let Some(token) = authorization.strip_prefix("Bearer ") else {
        return Err(ApiError::unauthorized("invalid Authorization scheme"));
    };
    if token != config.api_auth_token {
        return Err(ApiError::unauthorized("invalid api token"));
    }

    let Some(tenant_id) = header_value(headers, "X-Tenant-ID") else {
        return Err(ApiError::bad_request("missing X-Tenant-ID header"));
    };
    let request_id = header_value(headers, "X-Request-ID");

    Ok(AuthContext {
        tenant_id,
        request_id,
        actor: "bearer".to_owned(),
    })
}

fn validate_tenant_consistency(
    header_tenant_id: &str,
    body_tenant_id: &str,
) -> Result<(), ApiError> {
    if header_tenant_id == body_tenant_id {
        return Ok(());
    }
    Err(ApiError::bad_request(
        "tenant_id mismatch between X-Tenant-ID header and request body",
    ))
}

fn header_value(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl From<memory_core::CoreError> for ApiError {
    fn from(value: memory_core::CoreError) -> Self {
        match value {
            memory_core::CoreError::InvalidRequest(message) => Self {
                status: StatusCode::BAD_REQUEST,
                message,
            },
            memory_core::CoreError::Database(error) => {
                error!(error = %error, "database error");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "database error".to_owned(),
                }
            }
            memory_core::CoreError::Serialization(error) => {
                error!(error = %error, "serialization error");
                Self {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "serialization error".to_owned(),
                }
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(serde_json::json!({
            "error": self.message,
        }));
        (self.status, body).into_response()
    }
}

impl ApiError {
    fn unauthorized(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.to_owned(),
        }
    }

    fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.to_owned(),
        }
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
