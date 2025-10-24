#!/bin/bash
# Run the main sequencer/server with batch verification enabled
# This requires at least one external node to connect and sign batches

batch_verification_server_enabled=true \
rust_log=info,zksync_os_batch_verification=debug \
cargo run #--release
