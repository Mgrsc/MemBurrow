FROM rust:1.93-bookworm AS builder
WORKDIR /workspace
ARG TARGETARCH
ARG SQLITE_VECTOR_VERSION=0.9.92

RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY cmd ./cmd
COPY crates ./crates
COPY migrations ./migrations
COPY migrations_sqlite ./migrations_sqlite

RUN cargo build --release --locked --bin memory-api --bin memory-worker --bin memory-migrator

RUN set -eux; \
  arch="${TARGETARCH:-}"; \
  if [ -z "$arch" ]; then \
    case "$(uname -m)" in \
      x86_64) arch="amd64" ;; \
      aarch64|arm64) arch="arm64" ;; \
      *) echo "unsupported architecture: $(uname -m)"; exit 1 ;; \
    esac; \
  fi; \
  case "$arch" in \
    amd64) \
      asset="vector-linux-x86_64-${SQLITE_VECTOR_VERSION}.tar.gz"; \
      sha256="be665e24fbcf70be72906d82b4d1a5d842311bd4df813fdd4d0de7735300a1de" ;; \
    arm64) \
      asset="vector-linux-arm64-${SQLITE_VECTOR_VERSION}.tar.gz"; \
      sha256="5d70bc2afcb156e688862df2df0536f875ba3528d0b334eb413eb978534d8730" ;; \
    *) \
      echo "unsupported TARGETARCH: ${arch}"; \
      exit 1 ;; \
  esac; \
  curl -fsSL -o /tmp/sqlite-vector.tar.gz "https://github.com/sqliteai/sqlite-vector/releases/download/${SQLITE_VECTOR_VERSION}/${asset}"; \
  echo "${sha256}  /tmp/sqlite-vector.tar.gz" | sha256sum -c -; \
  mkdir -p /workspace/sqlite-vector; \
  tar -xzf /tmp/sqlite-vector.tar.gz -C /workspace/sqlite-vector; \
  test -f /workspace/sqlite-vector/vector.so

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates tini \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY --from=builder /workspace/target/release/memory-api /usr/local/bin/memory-api
COPY --from=builder /workspace/target/release/memory-worker /usr/local/bin/memory-worker
COPY --from=builder /workspace/target/release/memory-migrator /usr/local/bin/memory-migrator
COPY --from=builder /workspace/sqlite-vector/vector.so /usr/local/lib/sqlite-vector/vector.so
COPY docker/memory-service-entrypoint.sh /usr/local/bin/memory-service-entrypoint
RUN chmod +x /usr/local/bin/memory-service-entrypoint
RUN useradd --system --create-home --uid 10001 appuser \
  && mkdir -p /app/data \
  && chown -R appuser:appuser /app
USER appuser

ENV RUST_LOG=info
ENV LOG_FORMAT=json
EXPOSE 8080

ENTRYPOINT ["tini", "--", "/usr/local/bin/memory-service-entrypoint"]
