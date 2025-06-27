# syntax=docker/dockerfile:1.6
#################################
# ---------- Builder -----------#
#################################
FROM rust:slim AS builder

# ---- build-time system libs (bindgen, OpenSSL, etc.) ----
# * llvm-15 is the default on Debian bookworm (base of rust:slim).
#   If your host repo ships llvm-17 or llvm-18, just change every “15” below
#   to the matching number.
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        build-essential pkg-config libssl-dev ca-certificates git \
        clang-15 llvm-15-dev libclang-15-dev && \
    # ---------- ensure bindgen can find libclang ----------
    LLVM_LIBDIR="$(llvm-config-15 --libdir)" && \
    ln -sf "${LLVM_LIBDIR}/libclang.so.1"  "${LLVM_LIBDIR}/libclang.so" && \
    echo "libclang located in ${LLVM_LIBDIR}" && \
    rm -rf /var/lib/apt/lists/*

# Let cargo-bindgen pick it up automatically
ENV LIBCLANG_PATH=/usr/lib/llvm-15/lib
ENV LD_LIBRARY_PATH=${LIBCLANG_PATH}:${LD_LIBRARY_PATH}

# ---- non-root build user ----
ARG UID=10001
RUN useradd -m -u ${UID} app
USER app
WORKDIR /app

# ---- pin nightly via rust-toolchain(.toml) ----
COPY --chown=app rust-toolchain* ./
RUN rustup set profile minimal   # cargo installs the pinned toolchain on demand

# ---- copy entire source & build ----
COPY --chown=app . .
RUN cargo build --release --bin zksync_os_sequencer

#################################
# ---------- Runtime -----------#
#################################
FROM debian:stable-slim

# ---- minimal runtime deps + tini ----
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        libssl3 ca-certificates tini && \
    rm -rf /var/lib/apt/lists/*

# ---- runtime user & writable data dir ----
ARG UID=10001
RUN useradd -m -u ${UID} app && \
    mkdir -p /data && chown -R app:app /data

# ---- copy the compiled binary ----
COPY --from=builder /app/target/release/zksync_os_sequencer /usr/local/bin/

USER app
WORKDIR /data

# ---- document ports & persistent volume ----
EXPOSE 3000 3124 3312
VOLUME ["/data"]

ENTRYPOINT ["/usr/bin/tini","--","zksync_os_sequencer"]

LABEL org.opencontainers.image.title="zksync_os_sequencer"
