-- Initial schema: one OHLCV hypertable for every (symbol, tf, bar). The
-- forward-looking columns (quote_volume, trades, taker_buy_vol) are populated
-- from Binance kline data but not on the wire today; they unlock per-bar
-- delta / VWAP / aggression-ratio indicators without needing a trade-tape.

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
