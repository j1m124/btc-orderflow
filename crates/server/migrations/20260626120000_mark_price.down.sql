-- Drop the retention policies first (would error on a missing hypertable
-- otherwise), then drop the tables; hypertable bookkeeping cascades.
SELECT remove_retention_policy('mark_price',   if_exists => TRUE);
SELECT remove_retention_policy('funding_rate', if_exists => TRUE);
DROP TABLE IF EXISTS mark_price;
DROP TABLE IF EXISTS funding_rate;
