# OpenPool

OpenPool is a custodial, verifiable Lightning raffle platform. `OPENPOOL-1` fixes the entry
ledger, Bitcoin-block draw, payout arithmetic, and public proof protocol; it does not make
Lightning custody trustless or eliminate regulatory obligations.

## Local development

```bash
cp .env.example .env
docker compose up -d postgres minio
cargo test --workspace
APP_ENV=development API_BIND_ADDR=127.0.0.1:8080 WORKER_BIND_ADDR=127.0.0.1:8081 \
  ADDRESS_ENCRYPTION_KEY=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA= \
  cargo run -p openpool-api-app
```

The Axum process serves `/v1` plus the Dioxus SSR shell. Start the worker with
`cargo run -p openpool-worker-app` and the same required environment variables. The browser
hydration target and verifier WASM are completed before technical staging.

## Quality checks

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Golden valid proof fixtures live under `crates/openpool-test-support/fixtures`. The verifier
tests load those files directly and reject corrupted entries, roots, draws, payouts, and hashes.

## Mavapay staging caveat

The Mavapay adapter is implemented against the staging quote/transaction contract, but the
configured account currently receives `401` for both documented authentication forms. Local and
CI invoice-flow work must use `FakePaymentProvider`; no staging collection is considered verified
until Mavapay enables a working staging key. This is an explicit release gate, not a fallback to
production credentials.
