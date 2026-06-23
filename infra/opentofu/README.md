# OpenTofu deployment boundary

Technical staging consumes cloud-specific implementations of these interfaces:

- OCI image registry and a container runtime for independent API and worker services.
- Private managed PostgreSQL with automated backups, point-in-time recovery, and a migration job.
- Versioned S3-compatible proof storage with retention and least-privilege access.
- Managed OIDC, secret storage, TLS ingress, DNS, logs, traces, metrics, and alert delivery.

No provider resource is committed until a cloud account and region are selected. Every provider
implementation must expose the same inputs: `image_digest`, `database_url_secret`,
`address_encryption_key_secret`, `mavapay_secret`, `oidc_issuer`, `proof_bucket`, and
`public_base_url`. It must deploy with public-money collection disabled by default.
