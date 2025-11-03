# syntax=docker/dockerfile:1.6

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef

COPY rust-toolchain.toml rust-toolchain.toml
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --bin zksync-os-server --recipe-path recipe.json

FROM chef AS builder

# ---- build-time system libs ----
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libclang-19-dev && \
    rm -rf /var/lib/apt/lists/*

ENV LIBCLANG_PATH=/usr/lib/llvm-19/lib
ENV LD_LIBRARY_PATH=${LIBCLANG_PATH}:${LD_LIBRARY_PATH}

COPY --from=planner /app/recipe.json recipe.json
# Build dependencies (this is the caching Docker layer)
RUN cargo chef cook --bin zksync-os-server --release --recipe-path recipe.json

# Build application
COPY . .
RUN cargo build --release --bin zksync-os-server

#################################
# -------- Runtime -------------#
#################################
FROM debian:stable-slim

# ---- minimal runtime deps + tini ----
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        libssl3 ca-certificates tini && \
    rm -rf /var/lib/apt/lists/*

ARG UID=10001
RUN useradd -m -u ${UID} app && \
    mkdir -p /db && chown -R app:app /db

# ---- copy binary + genesis.json ----
COPY --from=builder /app/target/release/zksync-os-server /usr/local/bin/

COPY --from=builder /app/genesis/genesis.json /app/genesis/

USER app
WORKDIR /app

EXPOSE 3050 3124 3312 3053
VOLUME ["/db"]

ENTRYPOINT ["/usr/bin/tini", "--", "zksync-os-server"]

LABEL org.opencontainers.image.title="zksync-os-server"
