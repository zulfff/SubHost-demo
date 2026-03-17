FROM rust:1.75-slim-bookworm as builder

WORKDIR /app

RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN cargo build --release

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    ca-certificates \
    libssl3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app

COPY --from=builder /app/target/release/subhost /usr/local/bin/
COPY --from=builder /app/target/release/subhost-faucet /usr/local/bin/
COPY --from=builder /app/target/release/subhost-bench /usr/local/bin/

RUN mkdir -p /data

EXPOSE 30333 8545 8080

CMD ["subhost", "node"]
