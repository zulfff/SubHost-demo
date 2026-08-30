# Build stage: pinned to the toolchain in rust-toolchain.toml.
FROM rust:1.93-slim-bookworm AS builder

WORKDIR /build

RUN apt-get update \
    && apt-get install --no-install-recommends -y pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Copy manifests first so a source-only change reuses the dependency layer.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ ./crates/
COPY explorer/ ./explorer/

# `--locked` fails the build if Cargo.lock does not match the manifests, so an
# image can never be produced from an unreviewed dependency set.
RUN cargo build --release --locked \
    -p subhost-cli -p subhost-faucet -p subhost-explorer -p subhost-bench

# Runtime stage: no toolchain, no build dependencies.
FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates tini \
    && rm -rf /var/lib/apt/lists/* \
    # Run as an unprivileged user; the data directory is the only writable path.
    && useradd --system --create-home --uid 10001 subhost \
    && mkdir -p /data \
    && chown subhost:subhost /data

COPY --from=builder /build/target/release/subhost           /usr/local/bin/subhost
COPY --from=builder /build/target/release/subhost-faucet    /usr/local/bin/subhost-faucet
COPY --from=builder /build/target/release/subhost-explorer  /usr/local/bin/subhost-explorer
COPY --from=builder /build/target/release/subhost-bench     /usr/local/bin/subhost-bench

USER subhost
WORKDIR /data
VOLUME ["/data"]

# JSON-RPC, metrics, faucet, explorer.
EXPOSE 8545 9090 8080 3000

ENV RUST_LOG=info \
    SUBHOST_LOG_FORMAT=json \
    SUBHOST_HOME=/data

# tini reaps zombies and forwards SIGTERM so the node shuts down cleanly.
ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["subhost", "node", "--listen", "0.0.0.0:8545", "--data-dir", "/data"]
