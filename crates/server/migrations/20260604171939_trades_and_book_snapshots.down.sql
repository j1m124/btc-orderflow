-- TimescaleDB cascades the hypertable + retention policy + every chunk when
-- the parent table goes.
DROP TABLE IF EXISTS book_snapshots CASCADE;
DROP TABLE IF EXISTS trades CASCADE;
