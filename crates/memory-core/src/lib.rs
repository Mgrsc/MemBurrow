pub mod config;
pub mod model;
pub mod sqlite_store;
pub mod store;

pub use config::{BackendProfile, ServiceConfig};
pub use model::{
    FeedbackRequest, HealthResponse, IngestRequest, IngestResponse, MemoryMessage, RecallDebug,
    RecallItem, RecallRequest, RecallResponse, SHARED_PROCESS_ID, allowed_namespaces,
    build_namespace,
};
pub use sqlite_store::{
    append_audit_log_sqlite, claim_outbox_batch_sqlite, connect_sqlite_pool, ingest_event_sqlite,
    init_sqlite_vector_store, mark_outbox_done_sqlite, mark_outbox_retry_sqlite,
    persist_embedding_sqlite, persist_feedback_sqlite, persist_llm_usage_sqlite, ping_sqlite,
    recall_memories_hybrid_sqlite, recall_memories_sqlite, search_vector_scores_sqlite,
    upsert_memory_item_sqlite,
};
pub use sqlx::{Pool, Postgres, Sqlite};
pub use store::{
    AuditLogInput, CoreError, EmbeddingRecord, ExtractedMemory, LlmUsageInput, OutboxMessage,
    ReconcileCandidate, append_audit_log, claim_outbox_batch, connect_pool, ingest_event,
    list_reconcile_candidates, mark_outbox_done, mark_outbox_retry, persist_embedding,
    persist_feedback, persist_llm_usage, ping, recall_memories, recall_memories_hybrid,
    upsert_memory_item,
};

pub static PG_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");
pub static SQLITE_MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations_sqlite");
