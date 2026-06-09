-- Drop retention policy first (would block on missing hypertable otherwise),
-- then drop the table; the hypertable bookkeeping cascades with the table.
SELECT remove_retention_policy('liquidations', if_exists => TRUE);
DROP TABLE IF EXISTS liquidations;
