-- Compress `book_snapshots`. With the depth bump (top-50 → top-1000 each
-- side) the row arrays grow ~20x; at 1 row/s/symbol that is ~5.6 GB raw over
-- the 48h retention window. Timescale columnar compression on the fixed-shape
-- parallel arrays takes that to ~1 GB — comfortable on the 80 GB CPX22.
--
-- segmentby = symbol: every query filters by symbol, so segmenting on it lets
--   the planner skip other symbols' compressed batches wholesale.
-- orderby = ts DESC: the heatmap history query is
--   `WHERE symbol = $1 AND ts < $2 ORDER BY ts DESC LIMIT $3` — ordering the
--   compressed batches by ts DESC matches that access pattern so Timescale
--   reads the fewest batches.
ALTER TABLE book_snapshots SET (
    timescaledb.compress,
    timescaledb.compress_segmentby = 'symbol',
    timescaledb.compress_orderby    = 'ts DESC'
);

-- Compress chunks older than 2h. That sits well inside the 48h retention, so
-- the recent ~2h (the hot write + live-tail read path) stays uncompressed for
-- fast inserts and the bulk of history is compressed.
SELECT add_compression_policy('book_snapshots', INTERVAL '2 hours', if_not_exists => TRUE);
