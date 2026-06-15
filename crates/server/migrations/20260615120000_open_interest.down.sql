-- Drop the retention policy first (would error on a missing hypertable
-- otherwise), then drop the table; hypertable bookkeeping cascades.
SELECT remove_retention_policy('open_interest', if_exists => TRUE);
DROP TABLE IF EXISTS open_interest;
