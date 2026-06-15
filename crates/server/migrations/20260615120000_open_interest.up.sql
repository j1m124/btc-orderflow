-- Open interest: one row per poll of Binance USD-M `/fapi/v1/openInterest`
-- (live, ~5s cadence), plus 5m-resolution cold-start rows from
-- `/futures/data/openInterestHist`. `oi` is the symbol's total open interest
-- in CONTRACTS (base asset, e.g. BTC) — Binance's live endpoint returns no
-- USD figure, so USD notional is derived client-side (oi × candle close).
--
-- Only raw samples are persisted; per-bar OHLC is computed on read via
-- `time_bucket` (first/max/min/last over ts), so any chart TF works
-- retroactively — same philosophy as footprint and sub-second candle
-- synthesis. Backfill is 5m-resolution, so historical OI on sub-5m TFs is
-- sparse; live polling fills finer buckets going forward.
--
-- PK = (symbol, ts). The 5s live poll and the 5m backfill align on different
-- ms boundaries in practice; ON CONFLICT DO NOTHING absorbs the rare tie and
-- any redelivery on reconnect.

CREATE TABLE IF NOT EXISTS open_interest (
    symbol  TEXT             NOT NULL,
    ts      TIMESTAMPTZ      NOT NULL,
    oi      DOUBLE PRECISION NOT NULL,
    PRIMARY KEY (symbol, ts)
);

-- Daily chunks mirror `candles` / `liquidations` (low row-rate: a 5s poll is
-- ~17k rows/day, comfortably under a chunk's natural size).
SELECT create_hypertable(
    'open_interest',
    'ts',
    chunk_time_interval => INTERVAL '1 day',
    if_not_exists       => TRUE
);

-- 7 days matches `candles` and the cold-start OI backfill window.
SELECT add_retention_policy('open_interest', INTERVAL '7 days', if_not_exists => TRUE);
