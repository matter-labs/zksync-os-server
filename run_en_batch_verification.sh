#!/bin/bash
# Run external node with batch verification enabled
# This node will connect to the main sequencer and sign batches

# External node configuration
sequencer_block_replay_download_address=localhost:3053 \
sequencer_block_replay_server_address=0.0.0.0:3054 \
general_main_node_rpc_url=http://localhost:3050 \
general_rocks_db_path=./db/en \
general_prometheus_port=3313 \
rpc_address=0.0.0.0:3051 \
status_server_address=0.0.0.0:3073 \
batch_verification_enabled=true \
RUST_LOG=info,zksync_os_batch_verification=debug \
cargo run #--release
