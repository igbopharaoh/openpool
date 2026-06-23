CREATE TABLE oidc_sessions (
  id UUID PRIMARY KEY,
  subject UUID NOT NULL,
  roles JSONB NOT NULL DEFAULT '[]'::jsonb,
  csrf_token_hash BYTEA NOT NULL CHECK (octet_length(csrf_token_hash) = 32),
  expires_at TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX oidc_sessions_expiry_idx ON oidc_sessions (expires_at);
