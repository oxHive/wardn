FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libssl3 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --create-home --uid 10001 wardn

# publish-docker stages the cross-compiled binary here per target arch
# (dist/linux-amd64, dist/linux-arm64) before invoking buildx — no
# in-container cargo build, so both platforms build natively without QEMU.
ARG TARGETARCH
COPY dist/linux-${TARGETARCH}/wardn /usr/local/bin/wardn

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
