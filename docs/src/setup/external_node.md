# External node

Setting the `general_node_role=external` environment variable puts the node in external node mode, which means it
receives block replays from another node instead of producing its own blocks. The node will get priority transactions
from L1 and check that they match the ones in the replay but it won't change L1 state.

To run the external node locally, you need to enable networking on both main node and external node. Then set external node's services' ports so they don't overlap with the main node.

For example:

```bash
network_enabled=true \
network_secret_key=9cc842aaeb1492e567d989a34367c7239d1db21bad31557689c3d9d16e45b0b3 \
network_address=127.0.0.1 \
network_port=3061 \
network_boot_nodes=enode://dbd18888f17bad7df7fa958b57f4993f47312ba5364508fd0d9027e62ea17a037ca6985d6b0969c4341f1d4f8763a802785961989d07b1fb5373ced9d43969f6@127.0.0.1:3060 \
sequencer_rocks_db_path=./db/en \
sequencer_prometheus_port=3313 \
rpc_address=0.0.0.0:3051 \
cargo run --release
```
