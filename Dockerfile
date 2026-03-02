FROM rust:1.93-bookworm AS builder
WORKDIR /workspace

COPY Cargo.toml Cargo.lock ./
COPY cmd ./cmd
COPY crates ./crates
COPY migrations ./migrations

RUN cargo build --release --locked --bin memory-api --bin memory-worker --bin memory-migrator

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates tini \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /workspace/target/release/memory-api /usr/local/bin/memory-api
COPY --from=builder /workspace/target/release/memory-worker /usr/local/bin/memory-worker
COPY --from=builder /workspace/target/release/memory-migrator /usr/local/bin/memory-migrator
COPY docker/memory-service-entrypoint.sh /usr/local/bin/memory-service-entrypoint
RUN chmod +x /usr/local/bin/memory-service-entrypoint
RUN useradd --system --create-home --uid 10001 appuser
USER appuser

ENV RUST_LOG=info
ENV LOG_FORMAT=json
EXPOSE 8080

ENTRYPOINT ["tini", "--", "/usr/local/bin/memory-service-entrypoint"]
