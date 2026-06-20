ALTER TABLE outbox_events
  ADD COLUMN lease_owner TEXT,
  ADD COLUMN lease_expires_at TIMESTAMPTZ;

CREATE INDEX outbox_claim_idx ON outbox_events (occurred_at, lease_expires_at) WHERE delivered_at IS NULL;
