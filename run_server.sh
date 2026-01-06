#!/bin/bash

# Script to load environment variables from an env file and run zksync-os-server

if [ $# -eq 0 ]; then
    echo "Usage: $0 <path_to_env_file>"
    echo "Example: $0 chains/chain_6566.env"
    exit 1
fi

ENV_FILE="$1"

if [ ! -f "$ENV_FILE" ]; then
    echo "Error: Environment file '$ENV_FILE' not found"
    exit 1
fi

# Export all variables from the env file
# Using set -a to automatically export all variables
set -a
source "$ENV_FILE"
set +a

# Run the server
cargo run --release --bin zksync-os-server

