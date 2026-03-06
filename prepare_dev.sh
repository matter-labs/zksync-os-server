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
# cargo lives in the developer's home, not root's — find the right user.
CARGO_USER=""
for candidate in "${SUDO_USER:-}" "${DOAS_USER:-}" "${USER:-}"; do
    if [ -n "$candidate" ] && [ "$candidate" != "root" ] && [ -x "/home/$candidate/.cargo/bin/cargo" ]; then
        CARGO_USER="$candidate"
        break
    fi
done
# Fall back: find any non-root user that has cargo installed
if [ -z "$CARGO_USER" ]; then
    CARGO_USER=$(find /home -maxdepth 2 -name cargo -path '*/.cargo/bin/cargo' -executable 2>/dev/null | head -1 | cut -d/ -f3)
fi

if [ -z "$CARGO_USER" ]; then
    echo ""
    echo "Could not find a user with cargo installed."
    echo "Please run the following as your regular (non-root) user:"
    echo "  cargo install cargo-nextest --locked"
else
    echo "Installing cargo tools for user: $CARGO_USER"
    sudo -u "$CARGO_USER" bash -c '
        export PATH="$HOME/.cargo/bin:$PATH"
        cargo install cargo-nextest --locked
    '
fi

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
