ALTER TABLE invoices
  ADD COLUMN payout_address_ciphertext BYTEA,
  ADD COLUMN bolt11 TEXT,
  ADD COLUMN provider_quote_id TEXT,
  ADD COLUMN provider_payment_hash TEXT,
  ADD COLUMN expires_at TIMESTAMPTZ;

CREATE UNIQUE INDEX invoices_provider_quote_id_idx ON invoices (provider_quote_id) WHERE provider_quote_id IS NOT NULL;
CREATE UNIQUE INDEX invoices_provider_payment_hash_idx ON invoices (provider_payment_hash) WHERE provider_payment_hash IS NOT NULL;
