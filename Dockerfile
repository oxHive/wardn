FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home --uid 10001 wardn

# publish-docker stages the cross-compiled binary here per target arch
# (dist/linux-amd64, dist/linux-arm64) before invoking buildx — no
# in-container cargo build, so both platforms build natively without QEMU.
ARG TARGETARCH
COPY dist/linux-${TARGETARCH}/wardn /usr/local/bin/wardn

USER wardn
EXPOSE 8787
ENTRYPOINT ["/usr/local/bin/wardn"]
