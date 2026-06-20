# OpenPool

OpenPool is the Rust foundation for the Verifiable Lightning Raffle Platform (VLRP).
Milestone 1 contains the versioned raffle protocol, independent verifier, and health-only
API/worker binaries. PostgreSQL persistence, Mavapay, and the web application are deferred.

## Local development

```bash
cp .env.example .env
docker compose up -d postgres minio
cargo test --workspace
APP_ENV=development API_BIND_ADDR=127.0.0.1:8080 WORKER_BIND_ADDR=127.0.0.1:8081 \
  cargo run -p vlrp-api-app
```

The API exposes `GET /healthz` and `GET /readyz`. Start the worker with
`cargo run -p vlrp-worker-app` and the same required environment variables.

## Quality checks

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Golden valid proof fixtures live under `crates/vlrp-test-support/fixtures`. The verifier
tests load those files directly and reject corrupted entries, roots, draws, payouts, and hashes.
