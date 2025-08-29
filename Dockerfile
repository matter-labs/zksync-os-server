# syntax=docker/dockerfile:1.6

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef

COPY rust-toolchain.toml rust-toolchain.toml
WORKDIR /app

FROM chef AS planner
COPY . .
RUN cargo chef prepare --bin zksync_os_bin --recipe-path recipe.json

FROM chef AS builder

# ---- build-time system libs ----
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libclang-19-dev && \
    rm -rf /var/lib/apt/lists/*

ENV LIBCLANG_PATH=/usr/lib/llvm-19/lib
ENV LD_LIBRARY_PATH=${LIBCLANG_PATH}:${LD_LIBRARY_PATH}

COPY --from=planner /app/recipe.json recipe.json
# Build dependencies (this is the caching Docker layer)
RUN cargo chef cook --bin zksync_os_bin --release --recipe-path recipe.json

# Build application
COPY . .
RUN cargo build --release --bin zksync_os_bin

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

# ---- copy binary + prover blobs ----
COPY --from=builder /app/target/release/zksync_os_bin /usr/local/bin/

COPY --from=builder /app/server_app.bin /app/server_app_logging_enabled.bin /app/multiblock_batch.bin /app/

COPY --from=builder /app/genesis/genesis.json /app/genesis/

# reuired to support mod.rs in batcher: `concat!(env!("CARGO_MANIFEST_DIR"), "/../../server_app.bin")`
RUN mkdir -p /app/node/bin
RUN chmod +x /app/server_app.bin /app/server_app_logging_enabled.bin /app/multiblock_batch.bin

USER app
WORKDIR /

EXPOSE 3050 3124 3312 3053
VOLUME ["/db"]

ENTRYPOINT ["/usr/bin/tini","--","zksync_os_bin"]

LABEL org.opencontainers.image.title="zksync_os_bin"
