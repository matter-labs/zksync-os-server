# syntax=docker/dockerfile:1.6
#
# One-shot ZKsync ecosystem deployer for the Besu QBFT dev stack.
#
# Bakes at IMAGE BUILD time: the zk-deployer binary, the era-contracts tree at
# the exact rev zk-deployer's Cargo.lock pins (so deploy scripts match the
# compiled protocol-ops logic), and all forge/yarn artifacts — the container
# only broadcasts transactions at runtime.

#################################
# Builder
#################################
FROM rust:1-slim-bookworm AS builder

ARG ZK_ITESTS_REPO=https://github.com/matter-labs/zksync-os-integration-tests
ARG ZK_ITESTS_REF=main
ARG FOUNDRY_VERSION=v1.5.1

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        git curl ca-certificates pkg-config libssl-dev clang libclang-dev jq && \
    rm -rf /var/lib/apt/lists/*

# Node.js 20 + yarn: `build-contracts` runs `yarn install`, and forge deploy
# scripts invoke node via FFI (e.g. blake2s256.js).
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    npm install -g yarn && \
    rm -rf /var/lib/apt/lists/*

# Plain upstream Foundry — same pin as era-contracts CI (deterministic bytecode).
RUN ARCH=$(dpkg --print-architecture) && \
    curl -fsSL "https://github.com/foundry-rs/foundry/releases/download/${FOUNDRY_VERSION}/foundry_${FOUNDRY_VERSION}_linux_${ARCH}.tar.gz" \
        | tar -xz -C /usr/local/bin forge cast && \
    forge --version

RUN git clone --depth 1 --branch "${ZK_ITESTS_REF}" "${ZK_ITESTS_REPO}" /src

# era-contracts at the exact rev pinned by /src/Cargo.lock (the tree
# protocol_ops/zksync_os_genesis_gen are compiled from). Submodules provide
# the forge libraries (forge-std, OZ, ...).
RUN REV=$(grep -m1 -oE 'git\+https://github\.com/matter-labs/era-contracts[^#"]*#[0-9a-f]{40}' /src/Cargo.lock | sed 's/.*#//') && \
    echo "era-contracts rev: ${REV}" && \
    git init -q /opt/era-contracts && \
    cd /opt/era-contracts && \
    git remote add origin https://github.com/matter-labs/era-contracts && \
    git fetch --depth 1 origin "${REV}" && \
    git checkout -q FETCH_HEAD && \
    git submodule update --init --recursive --depth 1

ENV PROTOCOL_CONTRACTS_ROOT=/opt/era-contracts

WORKDIR /src
RUN cargo build --release -p zk-deployer

# Bake forge artifacts (yarn install + `forge build` in l1-contracts/ and
# da-contracts/). Also populates ~/.svm with the solc binaries that
# `forge script` re-resolves at deploy time.
RUN /src/target/release/zk-deployer build-contracts && mkdir -p /root/.svm

#################################
# Runtime
#################################
FROM debian:bookworm-slim

ARG FOUNDRY_VERSION=v1.5.1

RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
        curl ca-certificates jq git && \
    rm -rf /var/lib/apt/lists/*

# node for forge FFI scripts at deploy time
RUN curl -fsSL https://deb.nodesource.com/setup_20.x | bash - && \
    apt-get install -y --no-install-recommends nodejs && \
    npm install -g yarn && \
    rm -rf /var/lib/apt/lists/*

# anvil: ecosystem-init SIMULATES the deploy scripts against an anvil fork of
# the L1 before broadcasting, so anvil is a hard runtime dependency.
RUN ARCH=$(dpkg --print-architecture) && \
    curl -fsSL "https://github.com/foundry-rs/foundry/releases/download/${FOUNDRY_VERSION}/foundry_${FOUNDRY_VERSION}_linux_${ARCH}.tar.gz" \
        | tar -xz -C /usr/local/bin anvil && \
    anvil --version

COPY --from=builder /usr/local/bin/forge /usr/local/bin/cast /usr/local/bin/
COPY --from=builder /src/target/release/zk-deployer /usr/local/bin/zk-deployer
COPY --from=builder /opt/era-contracts /opt/era-contracts
COPY --from=builder /root/.svm /root/.svm

ENV PROTOCOL_CONTRACTS_ROOT=/opt/era-contracts

COPY intent.yaml /opt/intent.yaml
COPY entrypoint.sh /usr/local/bin/entrypoint.sh
RUN chmod +x /usr/local/bin/entrypoint.sh

WORKDIR /deployment
ENTRYPOINT ["/usr/local/bin/entrypoint.sh"]
