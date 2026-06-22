-- Restore the original per-table retention windows: trades + book_snapshots
-- back to 48h, candles + liquidations + open_interest back to 7d.

SELECT remove_retention_policy('candles', if_exists => TRUE);
SELECT add_retention_policy('candles', INTERVAL '7 days', if_not_exists => TRUE);

SELECT remove_retention_policy('trades', if_exists => TRUE);
SELECT add_retention_policy('trades', INTERVAL '48 hours', if_not_exists => TRUE);

SELECT remove_retention_policy('book_snapshots', if_exists => TRUE);
SELECT add_retention_policy('book_snapshots', INTERVAL '48 hours', if_not_exists => TRUE);

SELECT remove_retention_policy('liquidations', if_exists => TRUE);
SELECT add_retention_policy('liquidations', INTERVAL '7 days', if_not_exists => TRUE);

SELECT remove_retention_policy('open_interest', if_exists => TRUE);
SELECT add_retention_policy('open_interest', INTERVAL '7 days', if_not_exists => TRUE);
