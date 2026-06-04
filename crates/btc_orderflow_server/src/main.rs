//! btc_orderflow_server — entry point.
//!
//! Boot sequence today: parse env, init tracing, connect to TimescaleDB,
//! run migrations, exit. Real boot ordering (Q10: subscribe Binance WS →
//! gap-heal REST per-tf → spawn DB writer → open WS gateway) lands in
//! follow-up commits.

use anyhow::{Context, Result};
use sqlx::postgres::PgPoolOptions;
use tracing::info;

mod db;

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://btc:btc@127.0.0.1:5432/btc_orderflow".into());

    info!(db_url = %redact_url(&db_url), "connecting to database");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&db_url)
        .await
        .context("connect to TimescaleDB")?;

    db::run_migrations(&pool)
        .await
        .context("run sqlx migrations")?;

    info!("skeleton boot complete; exiting");
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("btc_orderflow_server=info,sqlx=warn"));
    fmt().with_env_filter(filter).init();
}

/// Strip the password from a postgres URL before logging it.
fn redact_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut u) => {
            if u.password().is_some() {
                let _ = u.set_password(Some("***"));
            }
            u.to_string()
        }
        Err(_) => "<unparseable url>".to_string(),
    }
}
