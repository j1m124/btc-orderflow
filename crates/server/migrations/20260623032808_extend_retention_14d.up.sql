-- Extend retention on every hypertable to a uniform 14 days.
--
-- Was: trades + book_snapshots at 48h (recent-state orderflow), candles +
-- liquidations + open_interest at 7d. The 48h orderflow window was too short
-- for replaying more than two days of heatmap / footprint history; 14d gives a
-- fortnight of context across the board while staying tiny on disk (~4.6 GB
-- uncompressed at current ingest rates on the 38 GB VPS; trades dominates at
-- ~150 MB/day, book_snapshots drops sharply once the 1m-cadence branch ships).
--
-- add_retention_policy with if_not_exists => TRUE will NOT update an existing
-- policy's interval — it no-ops on a table that already has a policy. So each
-- table is remove_retention_policy(if_exists) THEN add_retention_policy. That
-- pair is idempotent (safe to re-run), which matters because this migration is
-- applied to the live DB ahead of the next deploy and sqlx re-runs it on boot.

SELECT remove_retention_policy('candles', if_exists => TRUE);
SELECT add_retention_policy('candles', INTERVAL '14 days', if_not_exists => TRUE);

SELECT remove_retention_policy('trades', if_exists => TRUE);
SELECT add_retention_policy('trades', INTERVAL '14 days', if_not_exists => TRUE);

SELECT remove_retention_policy('book_snapshots', if_exists => TRUE);
SELECT add_retention_policy('book_snapshots', INTERVAL '14 days', if_not_exists => TRUE);

SELECT remove_retention_policy('liquidations', if_exists => TRUE);
SELECT add_retention_policy('liquidations', INTERVAL '14 days', if_not_exists => TRUE);

SELECT remove_retention_policy('open_interest', if_exists => TRUE);
SELECT add_retention_policy('open_interest', INTERVAL '14 days', if_not_exists => TRUE);
