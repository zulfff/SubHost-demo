# Build stage: pinned to the toolchain in rust-toolchain.toml.
FROM rust:1.93.0-slim-bookworm@sha256:776861219cd851131c1cec3bbd7cbeb16b99a794048097eb69ad9682a8ed0d57 AS builder

WORKDIR /build

# Copy manifests first so a source-only change reuses the dependency layer.
COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates/ ./crates/
COPY explorer/ ./explorer/

# `--locked` fails the build if Cargo.lock does not match the manifests, so an
# image can never be produced from an unreviewed dependency set.
RUN cargo build --release --locked \
    -p subhost-cli -p subhost-faucet -p subhost-explorer -p subhost-bench

# Keep the resolved Rust dependency inventories alongside the runtime binaries.
# 0.5.10 defaults to <crate>.cdx.json; --override-filename bom yields bom.json.
RUN cargo install --locked --version 0.5.10 cargo-cyclonedx \
    && cargo cyclonedx --format json --all --override-filename bom

# Runtime stage: no toolchain, no build dependencies.
FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc

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
COPY --from=builder /build/crates/subhost-cli/bom.json     /usr/share/subhost/sbom/subhost.cdx.json
COPY --from=builder /build/crates/subhost-faucet/bom.json  /usr/share/subhost/sbom/subhost-faucet.cdx.json
COPY --from=builder /build/explorer/bom.json               /usr/share/subhost/sbom/subhost-explorer.cdx.json
COPY --from=builder /build/crates/subhost-bench/bom.json   /usr/share/subhost/sbom/subhost-bench.cdx.json

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
# Select the intended validator/observer/demo policy explicitly at deployment.
CMD ["subhost", "--help"]
