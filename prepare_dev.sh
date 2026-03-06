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

# Install cargo tools required for development and testing.
# These must be run as the target (non-root) user, so we detect who that is.
if [ -n "${SUDO_USER:-}" ]; then
    TARGET_USER="$SUDO_USER"
else
    TARGET_USER="$(whoami)"
fi

echo "Installing cargo tools for user: $TARGET_USER"
sudo -u "$TARGET_USER" bash -c '
    export PATH="$HOME/.cargo/bin:$PATH"
    cargo install cargo-nextest --locked
'

echo ""
echo "All dependencies installed."
echo ""
echo "Before building, make sure the following environment variables are set"
echo "(add them to your shell profile, e.g. ~/.bashrc or ~/.profile):"
echo ""
echo "  export LIBCLANG_PATH=/usr/lib/llvm-19/lib"
echo "  export LD_LIBRARY_PATH=\${LIBCLANG_PATH}\${LD_LIBRARY_PATH:+:\$LD_LIBRARY_PATH}"
echo "  export PATH=\"\$HOME/.cargo/bin:\$PATH\""
echo ""
echo "Then build with:  cargo build --release --bin zksync-os-server"
