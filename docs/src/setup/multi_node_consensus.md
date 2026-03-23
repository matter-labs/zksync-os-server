# Multi-node consensus

This document describes the two node roles in a multi-node setup:

1. **ConsensusNode**: participates in Raft consensus and can become leader at any time.
2. **ExternalNode**: does **not** participate in consensus; it downloads canonized blocks from `ZksProtocol` and replays them locally.

A ConsensusNode will propose blocks when it is leader and will follow canonized blocks when it is a replica. An ExternalNode only replays canonized blocks and never proposes.

**ConsensusNode**

Requirements:
- Networking must be enabled (Raft RPC is transported over `lib/network`).
- `consensus_enabled=true` and a list of Raft peer IDs (`consensus_peer_ids`).
- Exactly one node should set `consensus_bootstrap=true` for initial cluster membership.
- Local Raft node ID is always derived from `network_secret_key`.
- `consensus_peer_ids` must include that derived local ID.

Example: three-node consensus (local dev, leader failover)

Use three enodes in `network_boot_nodes` and three peer IDs in `consensus_peer_ids__json`.
Only one node should use `consensus_bootstrap=true`.

For convenience, define these first:
```bash
ENODE_1="enode://246e07030b4c48b8f28ab1fdf797a02308b0ca724696b695aabee48ea48298ff221144a0c0f14ebf030aea6d5fb6b31bd3a02676204bb13e78336bb824e32f1d@127.0.0.1:3060"
ENODE_2="enode://d2db8005d59694a5b79b7c58d4d375c60c9323837e852bbbfd05819621c48a4218cefa37baf39a164e2a6f6c1b34c379c4a72c7480b5fbcc379d1befb881e8fc@127.0.0.1:3061"
ENODE_3="enode://2991880ae3ff81b881c86f54d2af0ee85a325231bb75f903f06f432020101614dbfbc75ddec885f5e101fed272bee661f492fb6dd80147b656da990635a7e581@127.0.0.1:3062"
PEER_IDS_JSON='[
  "0x246e07030b4c48b8f28ab1fdf797a02308b0ca724696b695aabee48ea48298ff221144a0c0f14ebf030aea6d5fb6b31bd3a02676204bb13e78336bb824e32f1d",
  "0xd2db8005d59694a5b79b7c58d4d375c60c9323837e852bbbfd05819621c48a4218cefa37baf39a164e2a6f6c1b34c379c4a72c7480b5fbcc379d1befb881e8fc",
  "0x2991880ae3ff81b881c86f54d2af0ee85a325231bb75f903f06f432020101614dbfbc75ddec885f5e101fed272bee661f492fb6dd80147b656da990635a7e581"
]'
BOOT_NODES="${ENODE_1},${ENODE_2},${ENODE_3}"
```

ConsensusNode #1 (bootstrap):
```bash
RUST_LOG="INFO,zksync_os_network=debug,zksync_os_raft=debug,openraft=info" \
network_enabled=true \
network_secret_key=0af6153646bbf600f55ce455e1995283542b1ae25ce2622ce1fda443927c5308 \
network_address=127.0.0.1 \
network_port=3060 \
network_boot_nodes="${BOOT_NODES}" \
consensus_enabled=true \
consensus_bootstrap=true \
consensus_peer_ids__json="${PEER_IDS_JSON}" \
cargo run
```

ConsensusNode #2:
```bash
RUST_LOG="INFO,zksync_os_network=debug,zksync_os_raft=debug,openraft=info" \
network_enabled=true \
network_secret_key=c2c8042b03801e2e14b395ed24f970ead7646a9ff315b54f747bcefdb99afda7 \
network_address=127.0.0.1 \
network_port=3061 \
network_boot_nodes="${BOOT_NODES}" \
consensus_enabled=true \
consensus_bootstrap=false \
consensus_peer_ids__json="${PEER_IDS_JSON}" \
general_run_batcher_subsystem=false \
status_server_enabled=false \
rpc_address=0.0.0.0:3051 \
observability_prometheus_port=3313 \
general_rocks_db_path="db/en-2" \
cargo run
```

ConsensusNode #3:
```bash
RUST_LOG="INFO,zksync_os_network=debug,zksync_os_raft=debug,openraft=info" \
network_enabled=true \
network_secret_key=8b50ece5c94762fb0b8dcd2f859fb0132b86c0540c388806b6a03e0b1c25978d \
network_address=127.0.0.1 \
network_port=3062 \
network_boot_nodes="${BOOT_NODES}" \
consensus_enabled=true \
consensus_bootstrap=false \
consensus_peer_ids__json="${PEER_IDS_JSON}" \
general_run_batcher_subsystem=false \
status_server_enabled=false \
rpc_address=0.0.0.0:3052 \
observability_prometheus_port=3314 \
general_rocks_db_path="db/en-3" \
cargo run
```

Failover check:
1. Start all three nodes and wait until one is leader.
2. Stop the leader process.
3. Verify one of the remaining two nodes becomes leader (after election timeout).
4. Restart the stopped node and verify it rejoins as follower.

**ExternalNode**

External nodes do not participate in Raft. They download canonized blocks using the existing p2p protocol and replay them locally.

Example (local dev):

Main Node:
```bash
network_enabled=true \
network_secret_key=0af6153646bbf600f55ce455e1995283542b1ae25ce2622ce1fda443927c5308 \
network_boot_nodes=enode://246e07030b4c48b8f28ab1fdf797a02308b0ca724696b695aabee48ea48298ff221144a0c0f14ebf030aea6d5fb6b31bd3a02676204bb13e78336bb824e32f1d@127.0.0.1:3060,enode://d2db8005d59694a5b79b7c58d4d375c60c9323837e852bbbfd05819621c48a4218cefa37baf39a164e2a6f6c1b34c379c4a72c7480b5fbcc379d1befb881e8fc@127.0.0.1:3060 \
cargo run
```

External Node:
```bash
RUST_LOG="info,zksync_os_storage_api=debug" \
network_enabled=true \
network_secret_key=c2c8042b03801e2e14b395ed24f970ead7646a9ff315b54f747bcefdb99afda7 \
network_address=127.0.0.1 \
network_port=3061 \
network_boot_nodes="enode://246e07030b4c48b8f28ab1fdf797a02308b0ca724696b695aabee48ea48298ff221144a0c0f14ebf030aea6d5fb6b31bd3a02676204bb13e78336bb824e32f1d@127.0.0.1:3060,enode://d2db8005d59694a5b79b7c58d4d375c60c9323837e852bbbfd05819621c48a4218cefa37baf39a164e2a6f6c1b34c379c4a72c7480b5fbcc379d1befb881e8fc@127.0.0.1:3060" \
general_main_node_rpc_url="http://127.0.0.1:3050" \
general_node_role=external \
observability_prometheus_port=3313 \
general_rocks_db_path="db/en" \
status_server_enabled=false \
rpc_address=0.0.0.0:3051 \
cargo run
```
