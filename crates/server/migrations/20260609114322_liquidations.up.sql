-- Liquidation events: one row per Binance `<symbol>@forceOrder` event,
-- decoded at ingest into the *liquidated position* side (NOT the raw
-- Binance forced-order side — see crates/server/src/binance/parse.rs).
--
-- Binance throttles per-symbol forceOrder to ≤1 message/sec; only the
-- latest liquidation in each 1-second window survives upstream. We cannot
-- recover the lost detail — accept it. There is also no REST endpoint for
-- liquidation history, so cold-start = empty until the first live event.
--
-- PK = (symbol, ts, price, qty). Ties at the same ms with identical
-- price+qty across genuinely distinct events are essentially impossible
-- under the 1/sec throttle. Inserts use ON CONFLICT DO NOTHING so any
-- redelivery during reconnect is silently dropped.
--
-- `quote_qty = price * qty` precomputed at ingest so SUM(quote_qty) bar-stat
-- queries don't redo the multiply per row across days × chunks.

CREATE TABLE IF NOT EXISTS liquidations (
    symbol     TEXT             NOT NULL,
    ts         TIMESTAMPTZ      NOT NULL,
    price      DOUBLE PRECISION NOT NULL,
    qty        DOUBLE PRECISION NOT NULL,
    quote_qty  DOUBLE PRECISION NOT NULL,
    side       TEXT             NOT NULL CHECK (side IN ('long', 'short')),
    PRIMARY KEY (symbol, ts, price, qty)
);

-- Daily chunks mirror `candles` (low row-rate; hourly chunks would create
-- 24 near-empty chunks/day for a sparse feed).
SELECT create_hypertable(
    'liquidations',
    'ts',
    chunk_time_interval => INTERVAL '1 day',
    if_not_exists       => TRUE
);

-- 7 days matches `candles`. Liquidation history is a longer-tail analysis
-- tool but candle parity keeps the retention story simple; revisit if
-- traders want deeper backtest depth.
SELECT add_retention_policy('liquidations', INTERVAL '7 days', if_not_exists => TRUE);
