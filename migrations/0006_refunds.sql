CREATE TABLE refunds (
  id UUID PRIMARY KEY,
  raffle_id UUID NOT NULL REFERENCES raffles(id),
  invoice_id UUID REFERENCES invoices(id),
  payout_id UUID UNIQUE REFERENCES payouts(id),
  reason TEXT NOT NULL CHECK (reason IN ('late_settlement', 'cancelled', 'minimum_not_met', 'unsafe_draw')),
  status TEXT NOT NULL DEFAULT 'awaiting_invoice' CHECK (status IN ('awaiting_invoice', 'pending', 'processing', 'settled', 'failed')),
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  UNIQUE (invoice_id, reason)
);

CREATE INDEX refunds_raffle_status_idx ON refunds (raffle_id, status);
