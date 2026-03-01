pub mod config;
pub mod model;
pub mod store;

pub use config::ServiceConfig;
pub use model::{
    FeedbackRequest, HealthResponse, IngestRequest, IngestResponse, MemoryMessage, RecallDebug,
    RecallItem, RecallRequest, RecallResponse,
};
pub use sqlx::{Pool, Postgres};
pub use store::{
    AuditLogInput, CoreError, EmbeddingRecord, ExtractedMemory, LlmUsageInput, OutboxMessage,
    ReconcileCandidate, append_audit_log, claim_outbox_batch, connect_pool, ingest_event,
    list_reconcile_candidates, mark_outbox_done, mark_outbox_retry, persist_embedding,
    persist_feedback, persist_llm_usage, ping, recall_memories, recall_memories_hybrid,
    upsert_memory_item,
};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
