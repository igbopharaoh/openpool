CREATE TABLE oidc_login_attempts (
  state TEXT PRIMARY KEY,
  nonce TEXT NOT NULL,
  pkce_verifier TEXT NOT NULL,
  expires_at TIMESTAMPTZ NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX oidc_login_attempts_expiry_idx ON oidc_login_attempts (expires_at);
