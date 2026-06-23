-- A reorg can leave more than one non-canonical candidate at a height.  The original
-- `(height, is_canonical)` unique constraint allowed only one of them, which made historical
-- reorg evidence impossible to retain.  Only the canonical candidate is unique per height.
ALTER TABLE bitcoin_blocks DROP CONSTRAINT IF EXISTS bitcoin_blocks_height_is_canonical_key;
CREATE UNIQUE INDEX bitcoin_blocks_one_canonical_height_idx
  ON bitcoin_blocks (height) WHERE is_canonical;
CREATE INDEX bitcoin_blocks_canonical_chain_idx
  ON bitcoin_blocks (height, block_hash) WHERE is_canonical;
