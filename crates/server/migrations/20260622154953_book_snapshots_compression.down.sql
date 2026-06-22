-- Remove the policy first, then turn compression off. Disabling compression
-- errors if any chunk is still compressed, so decompress them explicitly
-- after stopping the policy and before clearing the table flag.
SELECT remove_compression_policy('book_snapshots', if_exists => TRUE);

SELECT decompress_chunk(c, if_compressed => TRUE)
FROM show_chunks('book_snapshots') c;

ALTER TABLE book_snapshots SET (timescaledb.compress = false);
