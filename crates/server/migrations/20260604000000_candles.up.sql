-- Initial schema: one OHLCV hypertable for every (symbol, tf, bar). The
-- forward-looking columns (quote_volume, trades, taker_buy_vol) unlock per-bar
-- VWAP / trade-count / aggression-ratio indicators without a trade-tape.
--
-- They're left NULL-able even though Binance always populates them — that's
-- for exchange-portability, not a backfill TODO. Bybit/OKX/Deribit kline APIs
-- don't ship `trades` or `taker_buy_vol`; Coinbase + Kraken don't ship
-- `quote_volume` either. Keep NULL allowed so future non-Binance ingest paths
-- can write what they have without an ALTER COLUMN dance.

CREATE TABLE IF NOT EXISTS candles (
    symbol         TEXT             NOT NULL,
    tf             TEXT             NOT NULL,
    open_time      TIMESTAMPTZ      NOT NULL,
    close_time     TIMESTAMPTZ      NOT NULL,
    open           DOUBLE PRECISION NOT NULL,
    high           DOUBLE PRECISION NOT NULL,
    low            DOUBLE PRECISION NOT NULL,
    close          DOUBLE PRECISION NOT NULL,
    volume         DOUBLE PRECISION NOT NULL,
    quote_volume   DOUBLE PRECISION,
    trades         INTEGER,
    taker_buy_vol  DOUBLE PRECISION,
    PRIMARY KEY (symbol, tf, open_time)
);

-- Hypertable: chunk by open_time at 1-day granularity (matches retention).
-- `if_not_exists => TRUE` makes this idempotent if the migration replays.
SELECT create_hypertable(
    'candles',
    'open_time',
    chunk_time_interval => INTERVAL '1 day',
    if_not_exists       => TRUE
);

-- Drop chunks older than 7 days (Q9: hard retention wall).
SELECT add_retention_policy('candles', INTERVAL '7 days', if_not_exists => TRUE);
