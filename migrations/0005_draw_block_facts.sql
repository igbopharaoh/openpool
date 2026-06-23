ALTER TABLE raffles
  ADD COLUMN close_block_height BIGINT,
  ADD COLUMN close_block_hash BYTEA CHECK (close_block_hash IS NULL OR octet_length(close_block_hash) = 32),
  ADD COLUMN randomness_block_height BIGINT,
  ADD COLUMN randomness_block_hash BYTEA CHECK (randomness_block_hash IS NULL OR octet_length(randomness_block_hash) = 32);
