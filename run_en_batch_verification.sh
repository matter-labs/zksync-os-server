#!/bin/bash
# Run external node with batch verification enabled
# This node will connect to the main sequencer and sign batches

# External node configuration
sequencer_block_replay_download_address=localhost:3053 \
sequencer_block_replay_server_address=0.0.0.0:3054 \
general_rocks_db_path=./db/en \
general_prometheus_port=3313 \
rpc_address=0.0.0.0:3051 \
status_server_address=0.0.0.0:3073 \
batch_verification_enabled=true \
batch_verification_address=localhost:3072 \
batch_verification_signing_key=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110 \
cargo run #--release
