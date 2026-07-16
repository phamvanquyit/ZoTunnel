# syntax=docker/dockerfile:1

FROM rust:bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates
COPY web ./web
COPY configs ./configs
RUN cargo build --release -p zo-tunnel-server -p zo-tunnel-client \
    && mkdir -p /out/clients \
    && cp target/release/zo-tunnel-server /out/ \
    && ARCH="$(uname -m)" \
    && case "$ARCH" in \
         x86_64) LABEL=amd64 ;; \
         aarch64) LABEL=arm64 ;; \
         *) LABEL=amd64 ;; \
       esac \
    && cp target/release/zotunnel /out/clients/zotunnel-linux-${LABEL} \
    && tar -czf /out/clients/zotunnel-src.tar.gz \
         Cargo.toml Cargo.lock rust-toolchain.toml crates web configs

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /out/zo-tunnel-server /usr/local/bin/zo-tunnel-server
COPY --from=builder /out/clients /var/lib/zo-tunnel/clients
RUN mkdir -p /etc/zo-tunnel /etc/traefik/dynamic
ENV ZO_CONFIG=/etc/zo-tunnel/server.yaml \
    ZO_CLIENTS_DIR=/var/lib/zo-tunnel/clients \
    ZO_TRAEFIK_ENABLED=true \
    ZO_TRAEFIK_CONFIG_DIR=/etc/traefik/dynamic \
    ZO_TRAEFIK_SERVICE_URL=http://127.0.0.1:6210 \
    RUST_LOG=info
EXPOSE 6200 6210
CMD ["zo-tunnel-server", "start"]
