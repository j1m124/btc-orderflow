-- Mark price + funding rate.
--
-- `mark_price`: one row per sample of the Binance USD-M `<symbol>@markPrice@1s`
-- WS stream (live, ~1s cadence), plus mark-price OHLC cold-start rows from
-- `/fapi/v1/markPriceKlines` (close only — backfilled rows leave the index /
-- settle / funding columns NULL). `mark_price` is the symbol's fair-price mark,
-- the canonical reference for USD open-interest notional (the OI indicators
-- multiply OI × mark close instead of × candle close). `funding_rate` here is
-- the live *predicted* rate carried by the same WS payload; it is NULL on
-- backfilled rows (markPriceKlines has no funding).
--
-- `funding_rate`: the *settled* 8h funding history from `/fapi/v1/fundingRate`.
-- One row per settlement. The mark-price channel COALESCEs the live predicted
-- funding (from `mark_price`) with these settled points so the funding pane has
-- history before the live predicted curve has accumulated.
--
-- Both: per-bar OHLC / funding is computed on read via `time_bucket` (same
-- philosophy as open interest / footprint), so any chart TF works
-- retroactively. PK = (symbol, ts); ON CONFLICT DO NOTHING absorbs the rare
-- live/backfill tie and any reconnect redelivery (we tag each live sample with
-- Binance's own event time, not wall-clock).

CREATE TABLE IF NOT EXISTS mark_price (
    symbol           TEXT             NOT NULL,
    ts               TIMESTAMPTZ      NOT NULL,
    mark_price       DOUBLE PRECISION NOT NULL,
    index_price      DOUBLE PRECISION,
    est_settle_price DOUBLE PRECISION,
    funding_rate     DOUBLE PRECISION,
    PRIMARY KEY (symbol, ts)
);

-- Daily chunks mirror open_interest / candles. A 1s sample is ~86k rows/day —
-- larger than OI but trivial for TimescaleDB at 14-day retention.
SELECT create_hypertable(
    'mark_price',
    'ts',
    chunk_time_interval => INTERVAL '1 day',
    if_not_exists       => TRUE
);

CREATE TABLE IF NOT EXISTS funding_rate (
    symbol TEXT             NOT NULL,
    ts     TIMESTAMPTZ      NOT NULL,
    rate   DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (symbol, ts)
);

-- Settled funding is one row per 8h settlement — ~3 rows/day. Daily chunks are
-- oversized for the row-rate but keep the chunk-management uniform across
-- hypertables.
SELECT create_hypertable(
    'funding_rate',
    'ts',
    chunk_time_interval => INTERVAL '1 day',
    if_not_exists       => TRUE
);

-- 14 days matches every other hypertable (see 20260623032808_extend_retention_14d).
SELECT add_retention_policy('mark_price',   INTERVAL '14 days', if_not_exists => TRUE);
SELECT add_retention_policy('funding_rate', INTERVAL '14 days', if_not_exists => TRUE);
