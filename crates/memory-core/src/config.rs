use std::env;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BackendProfile {
    Lite,
    Distributed,
}

impl BackendProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lite => "lite",
            Self::Distributed => "distributed",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub backend_profile: BackendProfile,
    pub database_url: String,
    pub sqlite_database_url: String,
    pub sqlite_vector_extension_path: String,
    pub sqlite_busy_timeout_ms: u64,
    pub api_bind_addr: String,
    pub api_auth_token: String,
    pub worker_poll_interval_ms: u64,
    pub worker_batch_size: i64,
    pub worker_max_retry: i32,
    pub recall_candidate_limit: i64,
    pub reconcile_enabled: bool,
    pub reconcile_interval_seconds: u64,
    pub reconcile_batch_size: i64,
    pub qdrant_url: String,
    pub openai_base_url: String,
    pub openai_api_key: String,
    pub openai_extract_model: String,
    pub openai_embedding_model: String,
    pub embedding_dims: usize,
    pub log_format: String,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("missing environment variable: {0}")]
    MissingVar(&'static str),
    #[error("invalid environment variable: {name}, value={value}")]
    InvalidVar { name: &'static str, value: String },
}

impl ServiceConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        let backend_profile = optional_profile("BACKEND_PROFILE", BackendProfile::Lite)?;
        let database_url = optional("DATABASE_URL", "");
        let sqlite_database_url = optional("SQLITE_DATABASE_URL", "sqlite://./data/memburrow.db");
        let sqlite_vector_extension_path = optional(
            "SQLITE_VECTOR_EXTENSION_PATH",
            "/usr/local/lib/sqlite-vector/vector.so",
        );
        let sqlite_busy_timeout_ms = optional_parse("SQLITE_BUSY_TIMEOUT_MS", 5000_u64)?;
        let api_bind_addr = optional("API_BIND_ADDR", "0.0.0.0:8080");
        let api_auth_token = required("API_AUTH_TOKEN")?;
        let worker_poll_interval_ms = optional_parse("WORKER_POLL_INTERVAL_MS", 1500_u64)?;
        let worker_batch_size = optional_parse("WORKER_BATCH_SIZE", 32_i64)?;
        let worker_max_retry = optional_parse("WORKER_MAX_RETRY", 8_i32)?;
        let recall_candidate_limit = optional_parse("RECALL_CANDIDATE_LIMIT", 64_i64)?;
        let reconcile_enabled = optional_parse_bool("RECONCILE_ENABLED", true)?;
        let reconcile_interval_seconds = optional_parse("RECONCILE_INTERVAL_SECONDS", 120_u64)?;
        let reconcile_batch_size = optional_parse("RECONCILE_BATCH_SIZE", 200_i64)?;
        let qdrant_url = optional("QDRANT_URL", "http://qdrant:6333");
        let openai_base_url =
            validate_openai_base_url(optional("OPENAI_BASE_URL", "https://api.openai.com/v1"))?;
        let openai_api_key = required("OPENAI_API_KEY")?;
        let openai_extract_model = optional("OPENAI_EXTRACT_MODEL", "gpt-4o-mini");
        let openai_embedding_model = optional("OPENAI_EMBEDDING_MODEL", "text-embedding-3-small");
        let embedding_dims = optional_parse("EMBEDDING_DIMS", 1536)?;
        let log_format = optional("LOG_FORMAT", "json");

        match backend_profile {
            BackendProfile::Lite => {
                if sqlite_database_url.trim().is_empty() {
                    return Err(ConfigError::MissingVar("SQLITE_DATABASE_URL"));
                }
                if sqlite_vector_extension_path.trim().is_empty() {
                    return Err(ConfigError::MissingVar("SQLITE_VECTOR_EXTENSION_PATH"));
                }
            }
            BackendProfile::Distributed => {
                if database_url.trim().is_empty() {
                    return Err(ConfigError::MissingVar("DATABASE_URL"));
                }
                if qdrant_url.trim().is_empty() {
                    return Err(ConfigError::MissingVar("QDRANT_URL"));
                }
            }
        }

        Ok(Self {
            backend_profile,
            database_url,
            sqlite_database_url,
            sqlite_vector_extension_path,
            sqlite_busy_timeout_ms,
            api_bind_addr,
            api_auth_token,
            worker_poll_interval_ms,
            worker_batch_size,
            worker_max_retry,
            recall_candidate_limit,
            reconcile_enabled,
            reconcile_interval_seconds,
            reconcile_batch_size,
            qdrant_url,
            openai_base_url,
            openai_api_key,
            openai_extract_model,
            openai_embedding_model,
            embedding_dims,
            log_format,
        })
    }

    pub fn qdrant_collection(&self) -> String {
        "memburrow_agent_memory".to_owned()
    }

    pub fn is_lite(&self) -> bool {
        self.backend_profile == BackendProfile::Lite
    }
}

fn optional_parse_bool(name: &'static str, default: bool) -> Result<bool, ConfigError> {
    let value = env::var(name).unwrap_or_else(|_| default.to_string());
    match value.to_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(ConfigError::InvalidVar {
            name,
            value: value.clone(),
        }),
    }
}

fn required(name: &'static str) -> Result<String, ConfigError> {
    env::var(name).map_err(|_| ConfigError::MissingVar(name))
}

fn optional(name: &'static str, default: &str) -> String {
    env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn optional_profile(
    name: &'static str,
    default: BackendProfile,
) -> Result<BackendProfile, ConfigError> {
    let value = env::var(name).unwrap_or_else(|_| default.as_str().to_owned());
    match value.trim().to_lowercase().as_str() {
        "lite" => Ok(BackendProfile::Lite),
        "distributed" => Ok(BackendProfile::Distributed),
        _ => Err(ConfigError::InvalidVar {
            name,
            value: value.clone(),
        }),
    }
}

fn optional_parse<T>(name: &'static str, default: T) -> Result<T, ConfigError>
where
    T: std::str::FromStr + ToString,
{
    let value = env::var(name).unwrap_or_else(|_| default.to_string());
    value.parse::<T>().map_err(|_| ConfigError::InvalidVar {
        name,
        value: value.clone(),
    })
}

fn validate_openai_base_url(value: String) -> Result<String, ConfigError> {
    let normalized = value.trim_end_matches('/').to_owned();
    if normalized.ends_with("/v1") {
        return Ok(normalized);
    }

    Err(ConfigError::InvalidVar {
        name: "OPENAI_BASE_URL",
        value,
    })
}
