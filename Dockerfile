FROM rust:1.93-bookworm AS builder
WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
COPY cmd ./cmd
COPY crates ./crates
COPY migrations ./migrations

RUN cargo build --release --bin memory-api --bin memory-worker --bin memory-migrator

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /workspace/target/release/memory-api /usr/local/bin/memory-api
COPY --from=builder /workspace/target/release/memory-worker /usr/local/bin/memory-worker
COPY --from=builder /workspace/target/release/memory-migrator /usr/local/bin/memory-migrator

ENV RUST_LOG=info
ENV LOG_FORMAT=json
EXPOSE 8080

CMD ["memory-api"]
