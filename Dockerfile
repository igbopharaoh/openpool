FROM rust:1.94-bookworm AS builder
WORKDIR /workspace
COPY . .
RUN cargo build --release -p openpool-api -p openpool-worker

FROM gcr.io/distroless/cc-debian12:nonroot AS api
WORKDIR /app
COPY --from=builder /workspace/target/release/openpool-api /app/openpool-api
USER nonroot:nonroot
ENTRYPOINT ["/app/openpool-api"]

FROM gcr.io/distroless/cc-debian12:nonroot AS worker
WORKDIR /app
COPY --from=builder /workspace/target/release/openpool-worker /app/openpool-worker
USER nonroot:nonroot
ENTRYPOINT ["/app/openpool-worker"]
