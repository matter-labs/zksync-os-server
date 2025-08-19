# syntax=docker/dockerfile:1.6
#################################
# -------- Builder -------------#
#################################
FROM rust:slim AS builder

# ---- build-time system libs ----
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev ca-certificates git \
        clang-15 llvm-15-dev libclang-15-dev && \
    # ---------- ensure bindgen can find libclang ----------
    LLVM_LIBDIR="$(llvm-config-15 --libdir)" && \
    ln -sf "${LLVM_LIBDIR}/libclang.so.1"  "${LLVM_LIBDIR}/libclang.so" && \
    echo "libclang located in ${LLVM_LIBDIR}" && \
    rm -rf /var/lib/apt/lists/*

ENV LIBCLANG_PATH=/usr/lib/llvm-15/lib
ENV LD_LIBRARY_PATH=${LIBCLANG_PATH}:${LD_LIBRARY_PATH}

# ---- non-root builder user ----
ARG UID=10001
RUN useradd -m -u ${UID} app
USER app
WORKDIR /app

# ---- pin nightly ----
COPY --chown=app rust-toolchain* ./
RUN rustup set profile minimal

# ---- copy src & build ----
COPY --chown=app . .
RUN cargo build --release --bin zksync_os_sequencer

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
COPY --from=builder /app/target/release/zksync_os_sequencer /usr/local/bin/

COPY --from=builder /app/server_app.bin /app/server_app_logging_enabled.bin /app/multiblock_batch.bin /app/

RUN mkdir -p /app/genesis
COPY --from=builder /app/genesis/genesis.json /app/genesis/

# reuired to support mod.rs in batcher: `concat!(env!("CARGO_MANIFEST_DIR"), "/../../server_app.bin")`
RUN mkdir -p /app/node/sequencer
RUN chmod +x /app/server_app.bin /app/server_app_logging_enabled.bin /app/multiblock_batch.bin

USER app
WORKDIR /

EXPOSE 3050 3124 3312
VOLUME ["/db"]

ENTRYPOINT ["/usr/bin/tini","--","zksync_os_sequencer"]

LABEL org.opencontainers.image.title="zksync_os_sequencer"
