//! btc_orderflow_server — entry point.
//!
//! Current boot path: parse env → init tracing → connect TimescaleDB → run
//! migrations → REST-backfill every timeframe for BTCUSDT up to "now" →
//! exit. WS ingest and the gateway listener are added in follow-up commits.

use anyhow::{Context, Result};
use btc_orderflow_protocol::Timeframe;
use chrono::Duration as ChronoDuration;
use sqlx::postgres::PgPoolOptions;
use tracing::info;

mod binance;
mod db;
mod ingest;

/// Hardcoded for v1. The combined-stream WS plus the REST gap-heal both key
/// off this single value. A real multi-symbol story lives behind a config
/// flag the day a second venue lands.
const SYMBOL: &str = "BTCUSDT";

/// Cold-start backfill window. Matches the DB retention policy (Q9/Q10) —
/// no reason to fetch what Timescale would drop on the next chunk eviction.
const COLD_START_DAYS: i64 = 7;

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

    info!(symbol = SYMBOL, days = COLD_START_DAYS, "starting REST backfill");
    let rest = binance::rest::RestClient::default();
    let counts = ingest::backfill_symbol(
        &pool,
        &rest,
        SYMBOL,
        ChronoDuration::days(COLD_START_DAYS),
    )
    .await
    .context("rest backfill")?;

    for (tf, n) in Timeframe::ALL.iter().zip(counts.iter()) {
        info!(tf = tf.as_str(), rows = n, "tf backfill total");
    }

    info!("skeleton boot complete; WS ingest + gateway land next");
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
