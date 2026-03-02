use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const SHARED_PROCESS_ID: &str = "__shared__";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestRequest {
    pub tenant_id: String,
    pub entity_id: String,
    pub process_id: String,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    pub messages: Vec<MemoryMessage>,
    #[serde(default)]
    pub context: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResponse {
    pub accepted: bool,
    pub event_id: String,
    pub task_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallRequest {
    pub tenant_id: String,
    pub entity_id: String,
    pub process_id: String,
    pub query: String,
    pub intent: Option<String>,
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallItem {
    pub id: Uuid,
    pub r#type: String,
    pub content: String,
    pub score: f64,
    pub source: String,
    pub properties: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallDebug {
    pub route: String,
    pub candidate_count: usize,
    pub dropped: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecallResponse {
    pub items: Vec<RecallItem>,
    pub debug: RecallDebug,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedbackRequest {
    pub tenant_id: String,
    pub entity_id: String,
    pub process_id: String,
    pub turn_id: Option<String>,
    #[serde(default)]
    pub used_items: Vec<String>,
    #[serde(default)]
    pub helpful: Vec<String>,
    #[serde(default)]
    pub harmful: Vec<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxEnvelope {
    pub schema_version: u32,
    pub event_id: String,
    pub occurred_at: DateTime<Utc>,
    pub request: IngestRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboxFeedbackEnvelope {
    pub schema_version: u32,
    pub event_id: String,
    pub occurred_at: DateTime<Utc>,
    pub request: FeedbackRequest,
}

fn default_top_k() -> usize {
    8
}

pub fn build_namespace(tenant_id: &str, entity_id: &str, process_id: &str) -> String {
    format!("{tenant_id}:{entity_id}:{process_id}")
}

pub fn allowed_namespaces(tenant_id: &str, entity_id: &str, process_id: &str) -> Vec<String> {
    if process_id == SHARED_PROCESS_ID {
        return vec![build_namespace(tenant_id, entity_id, process_id)];
    }
    vec![
        build_namespace(tenant_id, entity_id, process_id),
        build_namespace(tenant_id, entity_id, SHARED_PROCESS_ID),
    ]
}
