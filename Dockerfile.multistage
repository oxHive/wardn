# Builder
FROM rust:1-slim-bookworm AS builder

# libsql-ffi compiles its bundled sqlite3 with cc; reqwest's default TLS
# backend links against OpenSSL.
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --release --bin wardn

# Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 wardn

COPY --from=builder /build/target/release/wardn /usr/local/bin/wardn

USER wardn
# The local libSQL database lives under the container user's home
# (~/.local/share/wardn/org.db by default) unless WARDN_DB_PATH overrides
# it — a home directory is required for that default to resolve, unlike the
# old proxy which had no local state. WARDN_LISTEN_ADDR must be set to
# something other than the 127.0.0.1 default for `serve` to be reachable
# from outside the container (podman-compose.yml sets both).
ENV HOME=/home/wardn
EXPOSE 7787
ENTRYPOINT ["/usr/local/bin/wardn"]
CMD ["serve"]
