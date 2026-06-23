ALTER TABLE payouts
  ADD COLUMN lease_owner TEXT,
  ADD COLUMN lease_expires_at TIMESTAMPTZ;

CREATE INDEX payouts_lease_idx ON payouts (status, lease_expires_at)
  WHERE status = 'processing';
