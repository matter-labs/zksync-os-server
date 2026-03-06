#!/usr/bin/env bash
# prepare_dev.sh — install system build dependencies for zksync-os-server
# Run this script with sudo (or as root) once per machine.
# Based on the project's Dockerfile (builder stage).

set -euo pipefail

apt-get update

DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    build-essential \
    pkg-config \
    cmake \
    libssl-dev \
    libclang-19-dev

rm -rf /var/lib/apt/lists/*

echo ""
echo "All system dependencies installed."
echo ""
echo "Before building, make sure the following environment variables are set"
echo "(add them to your shell profile, e.g. ~/.bashrc or ~/.profile):"
echo ""
echo "  export LIBCLANG_PATH=/usr/lib/llvm-19/lib"
echo "  export LD_LIBRARY_PATH=\${LIBCLANG_PATH}\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
echo "  export PATH=\"\$HOME/.cargo/bin:\$PATH\""
echo ""
echo "Then build with:  cargo build --release --bin zksync-os-server"
