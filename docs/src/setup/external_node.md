# External node

Setting the `general_node_role=external` environment variable puts the node in external node mode, which means it
receives block replays from another node instead of producing its own blocks. The node will get priority transactions
from L1 and check that they match the ones in the replay but it won't change L1 state.

To run the external node locally, you need to enable networking on both main node and external node. Then set external node's services' ports so they don't overlap with the main node.
Main Node:
```bash
network_enabled=true \
network_secret_key=0af6153646bbf600f55ce455e1995283542b1ae25ce2622ce1fda443927c5308 \
network_boot_nodes=enode://246e07030b4c48b8f28ab1fdf797a02308b0ca724696b695aabee48ea48298ff221144a0c0f14ebf030aea6d5fb6b31bd3a02676204bb13e78336bb824e32f1d@127.0.0.1:3060,enode://d2db8005d59694a5b79b7c58d4d375c60c9323837e852bbbfd05819621c48a4218cefa37baf39a164e2a6f6c1b34c379c4a72c7480b5fbcc379d1befb881e8fc@127.0.0.1:3060 \
cargo run
```
EN: 
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
network_port="3061" \
general_rocks_db_path="db/en" \
status_server_enabled=false \
rpc_address=0.0.0.0:3051 \
cargo run 
```
