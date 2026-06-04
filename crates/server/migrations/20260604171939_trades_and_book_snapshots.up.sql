-- Raw orderflow tables: aggTrades + periodic book snapshots.
--
-- Both unlock the next batch of indicators (volume profile, volume delta on
-- trades, footprint, orderbook panel, orderbook heatmap) and the sub-second
-- timeframes (1s/5s), which are computed on read from `trades` via
-- time_bucket rather than persisted.
--
-- Retention is 48h on both (vs 7d on `candles`) — orderflow analysis is
-- recent-state-focused, the data volume is ~100x higher per row count, and
-- the heatmap is meaningful only alongside the trade flow that explains it
-- (paired lifetime: dropping trades but keeping book snapshots would create
-- a confusing half-truth state in the UI).

-- --- trades ----------------------------------------------------------------
-- One row per Binance aggTrade (combines matches from the same taker at the
-- same price into one event). On USD-M futures this is the finest-grained
-- trade unit available — there is no individual `@trade` stream.
--
-- PK includes `ts` because TimescaleDB requires the partitioning column to
-- be part of any unique constraint. `agg_id` is globally unique per symbol
-- on Binance, so (symbol, ts, agg_id) is effectively a dedupe key for
-- ON CONFLICT DO NOTHING semantics on re-ingest.

CREATE TABLE IF NOT EXISTS trades (
    symbol          TEXT             NOT NULL,
    ts              TIMESTAMPTZ      NOT NULL,
    agg_id          BIGINT           NOT NULL,
    price           DOUBLE PRECISION NOT NULL,
    qty             DOUBLE PRECISION NOT NULL,
    is_buyer_maker  BOOLEAN          NOT NULL,
    PRIMARY KEY (symbol, ts, agg_id)
);

-- Hourly chunks: ~5-15M rows/day at BTCUSDT-perp volume means each chunk is
-- ~200-600k rows. 48h retention = 48 chunks; retention runs hourly and drops
-- one chunk at a time rather than dropping 24h of rows in one go.
SELECT create_hypertable(
    'trades',
    'ts',
    chunk_time_interval => INTERVAL '1 hour',
    if_not_exists       => TRUE
);

SELECT add_retention_policy('trades', INTERVAL '48 hours', if_not_exists => TRUE);

-- --- book_snapshots --------------------------------------------------------
-- One row per second per symbol: a snapshot of the live (diff-maintained)
-- in-memory book, top 50 levels each side, as four parallel arrays.
--
-- The live orderbook panel reads in-memory state directly via the gateway;
-- this table exists purely so the orderbook heatmap can replay history. Top
-- 50 captures the meaningful resting-liquidity structure for BTCUSDT-perp
-- without persisting deeper levels nobody renders.
--
-- Parallel arrays (not JSONB, not long-form) because the shape is fixed and
-- arrays compress well in Timescale; long-form would be ~17 GB/day raw.

CREATE TABLE IF NOT EXISTS book_snapshots (
    symbol       TEXT                NOT NULL,
    ts           TIMESTAMPTZ         NOT NULL,
    bid_prices   DOUBLE PRECISION[]  NOT NULL,
    bid_sizes    DOUBLE PRECISION[]  NOT NULL,
    ask_prices   DOUBLE PRECISION[]  NOT NULL,
    ask_sizes    DOUBLE PRECISION[]  NOT NULL,
    PRIMARY KEY (symbol, ts)
);

-- Hourly chunks: 86400 rows/day per symbol, ~3600 rows per chunk. Same
-- retention cadence rationale as `trades`.
SELECT create_hypertable(
    'book_snapshots',
    'ts',
    chunk_time_interval => INTERVAL '1 hour',
    if_not_exists       => TRUE
);

SELECT add_retention_policy('book_snapshots', INTERVAL '48 hours', if_not_exists => TRUE);
