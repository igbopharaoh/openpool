ALTER TABLE proofs
  ADD COLUMN storage_version_id TEXT,
  ADD COLUMN storage_etag TEXT,
  ADD COLUMN published_at TIMESTAMPTZ;

ALTER TABLE proofs
  ADD CONSTRAINT proofs_publication_metadata_complete CHECK (
    (storage_uri IS NULL AND storage_version_id IS NULL AND storage_etag IS NULL AND published_at IS NULL)
    OR
    (storage_uri IS NOT NULL AND storage_version_id IS NOT NULL AND storage_etag IS NOT NULL AND published_at IS NOT NULL)
  );
