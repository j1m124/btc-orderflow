//! Database helpers — pool init and migrations.

use anyhow::Result;
use sqlx::PgPool;
use tracing::info;

/// Run every migration in `migrations/` that hasn't been applied yet. The
/// macro path is resolved at compile time against the crate root.
pub async fn run_migrations(pool: &PgPool) -> Result<()> {
    info!("applying pending migrations");
    sqlx::migrate!("./migrations").run(pool).await?;
    info!("migrations up to date");
    Ok(())
}
