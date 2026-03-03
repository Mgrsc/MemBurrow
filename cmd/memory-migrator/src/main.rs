use anyhow::Context;
use memory_core::{
    BackendProfile, PG_MIGRATOR, SQLITE_MIGRATOR, ServiceConfig, connect_pool, connect_sqlite_pool,
};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = ServiceConfig::from_env().context("failed to load configuration")?;
    init_tracing(&config.log_format);

    match config.backend_profile {
        BackendProfile::Distributed => {
            let pool = connect_pool(&config.database_url)
                .await
                .context("failed to connect postgres database")?;
            PG_MIGRATOR
                .run(&pool)
                .await
                .context("postgres migration execution failed")?;
        }
        BackendProfile::Lite => {
            let pool = connect_sqlite_pool(&config)
                .await
                .context("failed to connect sqlite database")?;
            SQLITE_MIGRATOR
                .run(&pool)
                .await
                .context("sqlite migration execution failed")?;
        }
    }

    info!("migrations completed");
    Ok(())
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
