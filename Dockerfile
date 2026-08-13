# Builder
FROM rust:1-slim-bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY migrations ./migrations
COPY src ./src

# sqlx::migrate!("./migrations") embeds the migration SQL into the binary
# at compile time — migrations/ must exist here, but not in the runtime image.
RUN cargo build --release --bin hivewarden

# Runtime
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/hivewarden /usr/local/bin/hivewarden

EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/hivewarden"]
