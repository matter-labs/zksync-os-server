sequencer_block_replay_download_address=localhost:3053 \
sequencer_block_replay_server_address=0.0.0.0:3054 \
general_rocks_db_path=./db/en sequencer_prometheus_port=3313 rpc_address=0.0.0.0:3051 \
genesis_bridgehub_address=0xc4fd2580c3487bba18d63f50301020132342fdbd general_l1_rpc_url=$SEPOLIA_RPC genesis_chain_id=8022833 \
prover_api_object_store_mode=S3AnonymousReadOnly \
prover_api_object_store_endpoint=https://hel1.your-objectstorage.com \
prover_api_object_store_bucket_base_url=zksync-os-testnet-testnet-alpha-fri-proofs \
general_main_node_rpc_url=https://zksync-os-testnet-alpha.zksync.dev/   \
two_fa_main_node_address=https://zksync-os-testnet-alpha.zksync.dev/ \
two_fa_enabled=true \
cargo run --release