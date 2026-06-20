CREATE TABLE organizers (
  id UUID PRIMARY KEY,
  display_name TEXT NOT NULL,
  lightning_address_ciphertext BYTEA NOT NULL,
  status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled', 'pending_review')),
  verification_status TEXT NOT NULL DEFAULT 'unverified' CHECK (verification_status IN ('unverified', 'verified', 'rejected')),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE raffles (
  id UUID PRIMARY KEY,
  organizer_id UUID NOT NULL REFERENCES organizers(id),
  name TEXT NOT NULL,
  entry_price_sats BIGINT NOT NULL CHECK (entry_price_sats > 0),
  start_time TIMESTAMPTZ NOT NULL,
  end_time TIMESTAMPTZ NOT NULL CHECK (end_time > start_time),
  status TEXT NOT NULL DEFAULT 'DRAFT' CHECK (status IN ('DRAFT', 'SCHEDULED', 'OPEN', 'CLOSING', 'RANDOMNESS_PENDING', 'DRAW_READY', 'WINNER_SELECTED', 'PAYOUT_PENDING', 'PAID_OUT', 'CANCELLED', 'REFUNDING', 'REFUNDED')),
  randomness_delay_blocks INTEGER NOT NULL DEFAULT 6 CHECK (randomness_delay_blocks > 0),
  total_pool_sats BIGINT NOT NULL DEFAULT 0 CHECK (total_pool_sats >= 0),
  total_tickets BIGINT NOT NULL DEFAULT 0 CHECK (total_tickets >= 0),
  entry_chain_head BYTEA,
  entries_root BYTEA,
  frozen_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE payout_splits (
  raffle_id UUID PRIMARY KEY REFERENCES raffles(id) ON DELETE CASCADE,
  winner_bps INTEGER NOT NULL CHECK (winner_bps BETWEEN 0 AND 10000),
  organizer_bps INTEGER NOT NULL CHECK (organizer_bps BETWEEN 0 AND 10000),
  platform_bps INTEGER NOT NULL CHECK (platform_bps BETWEEN 0 AND 10000),
  CHECK (winner_bps + organizer_bps + platform_bps = 10000)
);

CREATE TABLE invoices (
  id UUID PRIMARY KEY,
  raffle_id UUID NOT NULL REFERENCES raffles(id),
  participant_public_id BYTEA NOT NULL CHECK (octet_length(participant_public_id) = 32),
  payment_reference_hash BYTEA NOT NULL UNIQUE CHECK (octet_length(payment_reference_hash) = 32),
  amount_sats BIGINT NOT NULL CHECK (amount_sats > 0),
  ticket_count BIGINT NOT NULL CHECK (ticket_count > 0),
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'settled', 'expired', 'ineligible')),
  settled_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE provider_quotes (
  id UUID PRIMARY KEY,
  invoice_id UUID NOT NULL REFERENCES invoices(id),
  provider_name TEXT NOT NULL,
  provider_reference TEXT NOT NULL,
  raw_payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (provider_name, provider_reference)
);

CREATE TABLE provider_orders (
  id UUID PRIMARY KEY,
  invoice_id UUID NOT NULL REFERENCES invoices(id),
  provider_name TEXT NOT NULL,
  provider_reference TEXT NOT NULL,
  raw_payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (provider_name, provider_reference)
);

CREATE TABLE webhook_events (
  id UUID PRIMARY KEY,
  provider_name TEXT NOT NULL,
  provider_event_id TEXT NOT NULL,
  payload_hash BYTEA NOT NULL CHECK (octet_length(payload_hash) = 32),
  raw_payload JSONB NOT NULL,
  signature_valid BOOLEAN NOT NULL,
  received_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (provider_name, provider_event_id)
);

CREATE TABLE payment_events (
  id UUID PRIMARY KEY,
  invoice_id UUID NOT NULL REFERENCES invoices(id),
  provider_name TEXT NOT NULL,
  provider_transaction_id TEXT NOT NULL,
  status TEXT NOT NULL,
  amount_sats BIGINT NOT NULL CHECK (amount_sats > 0),
  observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (provider_name, provider_transaction_id)
);

CREATE TABLE entries (
  id UUID PRIMARY KEY,
  raffle_id UUID NOT NULL REFERENCES raffles(id),
  invoice_id UUID NOT NULL UNIQUE REFERENCES invoices(id),
  entry_index BIGINT NOT NULL CHECK (entry_index >= 0),
  participant_public_id BYTEA NOT NULL CHECK (octet_length(participant_public_id) = 32),
  payment_reference_hash BYTEA NOT NULL CHECK (octet_length(payment_reference_hash) = 32),
  amount_sats BIGINT NOT NULL CHECK (amount_sats > 0),
  ticket_start BIGINT NOT NULL CHECK (ticket_start >= 0),
  ticket_end BIGINT NOT NULL CHECK (ticket_end > ticket_start),
  settled_at TIMESTAMPTZ NOT NULL,
  previous_entry_hash BYTEA NOT NULL CHECK (octet_length(previous_entry_hash) = 32),
  entry_hash BYTEA NOT NULL CHECK (octet_length(entry_hash) = 32),
  UNIQUE (raffle_id, entry_index),
  UNIQUE (raffle_id, ticket_start),
  UNIQUE (raffle_id, ticket_end)
);

CREATE TABLE bitcoin_blocks (
  height BIGINT NOT NULL,
  block_hash BYTEA PRIMARY KEY CHECK (octet_length(block_hash) = 32),
  previous_hash BYTEA NOT NULL CHECK (octet_length(previous_hash) = 32),
  block_time TIMESTAMPTZ NOT NULL,
  is_canonical BOOLEAN NOT NULL DEFAULT true,
  observed_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (height, is_canonical)
);

CREATE TABLE draws (
  id UUID PRIMARY KEY,
  raffle_id UUID NOT NULL UNIQUE REFERENCES raffles(id),
  entries_root BYTEA NOT NULL CHECK (octet_length(entries_root) = 32),
  randomness_block_hash BYTEA NOT NULL CHECK (octet_length(randomness_block_hash) = 32),
  winning_ticket BIGINT NOT NULL CHECK (winning_ticket >= 0),
  winner_entry_id UUID NOT NULL REFERENCES entries(id),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE payouts (
  id UUID PRIMARY KEY,
  raffle_id UUID NOT NULL REFERENCES raffles(id),
  recipient_type TEXT NOT NULL CHECK (recipient_type IN ('winner', 'organizer', 'platform', 'refund')),
  idempotency_key TEXT NOT NULL UNIQUE,
  amount_sats BIGINT NOT NULL CHECK (amount_sats > 0),
  status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'processing', 'settled', 'failed')),
  provider_reference TEXT,
  failure_reason TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  settled_at TIMESTAMPTZ
);

CREATE TABLE proofs (
  id UUID PRIMARY KEY,
  raffle_id UUID NOT NULL REFERENCES raffles(id),
  protocol_version TEXT NOT NULL,
  proof_json JSONB NOT NULL,
  proof_hash BYTEA NOT NULL CHECK (octet_length(proof_hash) = 32),
  storage_uri TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (raffle_id, protocol_version)
);

CREATE TABLE jobs (
  id UUID PRIMARY KEY,
  kind TEXT NOT NULL,
  deduplication_key TEXT NOT NULL,
  payload JSONB NOT NULL DEFAULT '{}'::jsonb,
  status TEXT NOT NULL DEFAULT 'queued' CHECK (status IN ('queued', 'running', 'succeeded', 'dead')),
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
  max_attempts INTEGER NOT NULL DEFAULT 8 CHECK (max_attempts > 0),
  available_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  lease_owner TEXT,
  lease_expires_at TIMESTAMPTZ,
  last_error TEXT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (kind, deduplication_key)
);

CREATE TABLE outbox_events (
  id UUID PRIMARY KEY,
  aggregate_type TEXT NOT NULL,
  aggregate_id UUID NOT NULL,
  event_type TEXT NOT NULL,
  payload JSONB NOT NULL,
  occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  delivered_at TIMESTAMPTZ,
  attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0)
);

CREATE TABLE audit_events (
  id UUID PRIMARY KEY,
  aggregate_type TEXT NOT NULL,
  aggregate_id UUID NOT NULL,
  event_type TEXT NOT NULL,
  actor_type TEXT NOT NULL,
  actor_id UUID,
  payload JSONB NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX raffles_status_end_time_idx ON raffles (status, end_time);
CREATE INDEX entries_raffle_index_idx ON entries (raffle_id, entry_index);
CREATE INDEX invoices_raffle_status_idx ON invoices (raffle_id, status);
CREATE INDEX jobs_claim_idx ON jobs (status, available_at, lease_expires_at);
CREATE INDEX outbox_undelivered_idx ON outbox_events (occurred_at) WHERE delivered_at IS NULL;
CREATE INDEX payouts_retry_idx ON payouts (status, created_at) WHERE status IN ('pending', 'failed');
