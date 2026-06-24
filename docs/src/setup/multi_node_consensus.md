# Multi-node consensus

This document describes the two node roles in a multi-node setup:

1. **ConsensusNode**: participates in Raft consensus and can become leader at any time.
2. **ExternalNode**: does **not** participate in consensus; it downloads canonized blocks from `ZksProtocol` and replays them locally.

A ConsensusNode will propose blocks when it is leader and will follow canonized blocks when it is a replica. An ExternalNode only replays canonized blocks and never proposes.

> **Batcher subsystem (L1 settlement) is not yet highly available.** The batcher pipeline — proof generation, L1 batch submission — must be enabled on exactly one consensus node via `batcher_enabled=true` (which is the default). The other consensus nodes should set `batcher_enabled=false`. This choice is independent of `consensus_bootstrap` and the current Raft leader: the batcher-enabled node may be a leader or a follower. Leader failover works for block production. If the node running the batcher fails, another node will be elected leader and continue producing blocks, but those blocks will not be submitted to L1 until the original batcher node restarts or the cluster is manually reconfigured to enable the batcher on a different node.

### Prerequisites

Before starting any node, an L1 must be running at `general_l1_rpc_url` (defaults to `http://localhost:8545`) with the chain's contract state preloaded. The `run_local.sh` script does this automatically; to do it manually for the v30.2 default chain:

```bash
gzip -d < ./local-chains/v30.2/l1-state.json.gz > /tmp/l1-state.json
anvil --load-state /tmp/l1-state.json --port 8545 --block-time 0.25 --mixed-mining --slots-in-an-epoch 10
```

The default ports used by ConsensusNode #1 below (`3050` RPC, `3060` p2p, `3071` status server, `3124` prover API, `3312` Prometheus) must be free on the host. In this example ConsensusNode #1 is also the only batcher-enabled node, so ConsensusNode #2 and #3 set `BATCHER_ENABLED=false` and `PROVER_API_ENABLED=false`. Each node keeps its status server enabled on a unique port so failover can be checked through `/status`.

**ConsensusNode**

Requirements:
- Networking must be enabled (Raft RPC is transported over `lib/network`).
- `consensus_enabled=true` and a list of Raft peer IDs (`consensus_peer_ids`).
- Set `consensus_bootstrap=true` on each consensus node that may initialize cluster membership.
  It is safe to set this on all consensus nodes; the first initializer wins.
- Local Raft node ID is always derived from `network_secret_key`.
- `consensus_peer_ids` must include that derived local ID.

Example: three-node consensus (local dev, leader failover)

Use three enodes in `network_boot_nodes` and three peer IDs in `consensus_peer_ids__json`.
All three nodes may use `consensus_bootstrap=true`, and exactly one consensus node should leave `batcher_enabled=true`.

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

ConsensusNode #1:
```bash
NETWORK_ENABLED=true \
NETWORK_SECRET_KEY=0af6153646bbf600f55ce455e1995283542b1ae25ce2622ce1fda443927c5308 \
NETWORK_ADDRESS=127.0.0.1 \
NETWORK_PORT=3060 \
NETWORK_BOOT_NODES="${BOOT_NODES}" \
CONSENSUS_ENABLED=true \
CONSENSUS_BOOTSTRAP=true \
CONSENSUS_PEER_IDS__JSON="${PEER_IDS_JSON}" \
GENERAL_ROCKS_DB_PATH="db/en-1" \
cargo run -- --config ./local-chains/local_dev.yaml --config ./local-chains/v30.2/default/config.yaml
```

ConsensusNode #2:
```bash
NETWORK_ENABLED=true \
NETWORK_SECRET_KEY=c2c8042b03801e2e14b395ed24f970ead7646a9ff315b54f747bcefdb99afda7 \
NETWORK_ADDRESS=127.0.0.1 \
NETWORK_PORT=3061 \
NETWORK_BOOT_NODES="${BOOT_NODES}" \
CONSENSUS_ENABLED=true \
CONSENSUS_BOOTSTRAP=true \
CONSENSUS_PEER_IDS__JSON="${PEER_IDS_JSON}" \
BATCHER_ENABLED=false \
PROVER_API_ENABLED=false \
STATUS_SERVER_ADDRESS=0.0.0.0:3072 \
RPC_ADDRESS=0.0.0.0:3051 \
OBSERVABILITY_PROMETHEUS_PORT=3313 \
GENERAL_ROCKS_DB_PATH="db/en-2" \
cargo run -- --config ./local-chains/local_dev.yaml --config ./local-chains/v30.2/default/config.yaml
```

ConsensusNode #3:
```bash
NETWORK_ENABLED=true \
NETWORK_SECRET_KEY=8b50ece5c94762fb0b8dcd2f859fb0132b86c0540c388806b6a03e0b1c25978d \
NETWORK_ADDRESS=127.0.0.1 \
NETWORK_PORT=3062 \
NETWORK_BOOT_NODES="${BOOT_NODES}" \
CONSENSUS_ENABLED=true \
CONSENSUS_BOOTSTRAP=true \
CONSENSUS_PEER_IDS__JSON="${PEER_IDS_JSON}" \
BATCHER_ENABLED=false \
PROVER_API_ENABLED=false \
STATUS_SERVER_ADDRESS=0.0.0.0:3073 \
RPC_ADDRESS=0.0.0.0:3052 \
OBSERVABILITY_PROMETHEUS_PORT=3314 \
GENERAL_ROCKS_DB_PATH="db/en-3" \
cargo run -- --config ./local-chains/local_dev.yaml --config ./local-chains/v30.2/default/config.yaml
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
