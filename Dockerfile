# syntax=docker/dockerfile:1.6

FROM lukemathwalker/cargo-chef:latest-rust-1 AS chef

COPY rust-toolchain.toml rust-toolchain.toml
WORKDIR /app

FROM chef AS planner
COPY . .
# This cargo-chef `prepare` runs `cargo metadata`, which resolves the private git
# dependencies; same secret handling as the build stages below.
RUN --mount=type=secret,id=github_token \
    if [ -s /run/secrets/github_token ]; then \
      git config --global url."https://x-access-token:$(cat /run/secrets/github_token)@github.com/".insteadOf "https://github.com/"; \
    fi && \
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo chef prepare --bin zksync-os-server --recipe-path recipe.json && \
    rm -f /root/.gitconfig

FROM chef AS builder

# ---- build-time system libs ----
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends libclang-19-dev && \
    rm -rf /var/lib/apt/lists/*

ENV LIBCLANG_PATH=/usr/lib/llvm-19/lib
ENV LD_LIBRARY_PATH=${LIBCLANG_PATH}:${LD_LIBRARY_PATH}

COPY --from=planner /app/recipe.json recipe.json
# Build dependencies (this is the caching Docker layer). Cargo git dependencies may
# live in private repos; the token is secret-mounted and the git config is removed
# within the same layer so it never persists in the image.
RUN --mount=type=secret,id=github_token \
    if [ -s /run/secrets/github_token ]; then \
      git config --global url."https://x-access-token:$(cat /run/secrets/github_token)@github.com/".insteadOf "https://github.com/"; \
    fi && \
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo chef cook --bin zksync-os-server --release --features gcp --recipe-path recipe.json && \
    rm -f /root/.gitconfig

# Build application
COPY . .
RUN --mount=type=secret,id=github_token \
    if [ -s /run/secrets/github_token ]; then \
      git config --global url."https://x-access-token:$(cat /run/secrets/github_token)@github.com/".insteadOf "https://github.com/"; \
    fi && \
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo build --release --bin zksync-os-server --features gcp && \
    rm -f /root/.gitconfig

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

COPY --from=builder /app/local-chains/v31.0/default/genesis.json /app/local-chains/v31.0/default/genesis.json
# Chains that were genesis'd on v30.2 still need the original genesis input (e.g. a fresh
# external-node sync).
COPY --from=builder /app/local-chains/v30.2/default/genesis.json /app/local-chains/v30.2/default/genesis.json

USER app
WORKDIR /app

EXPOSE 3050 3124 3312 3060
VOLUME ["/db"]

ENTRYPOINT ["/usr/bin/tini", "--", "zksync-os-server"]

LABEL org.opencontainers.image.title="zksync-os-server"
