# Technical staging runbook

1. Verify the SBOM, signed immutable image digest, and vulnerability scan; only then approve the
   protected `technical-staging` environment in the release workflow.
2. Apply migrations exactly once using the release image and its migration-only ECS task. Confirm
   task success before API/worker rollout.
3. Deploy API and worker from immutable, scanned image digests; verify `/healthz` and `/readyz`.
4. Confirm OIDC issuer/session configuration, secret redaction, proof bucket retention/versioning,
   database
   backup success, and feature flags that keep public-money collection disabled.
5. Execute the fake-provider end-to-end drill: invoice, duplicate webhook, one entry, close,
   deterministic draw, payout simulation, proof publication, and local browser verification.
6. Execute failure drills: provider timeout, worker kill during payout, expired lease recovery,
   Bitcoin reorg, malformed proof, and failed payout retry.
7. Record failures, metrics, traces, backup/restore evidence, and rollback outcome before promoting any release.
