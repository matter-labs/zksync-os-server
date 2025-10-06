#!/bin/bash
# Run the main sequencer/server with batch verification enabled
# This requires at least one external node to connect and sign batches

batch_verification_enabled=true \
batch_verification_address=0.0.0.0:3072 \
batch_verification_threshold=1 \
cargo run #--release
