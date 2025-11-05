# External node

Setting the `block_replay_download_address` environment variable puts the node in external node mode, which means it
receives block replays from another node instead of producing its own blocks. The node will get priority transactions
from L1 and check that they match the ones in the replay but it won't change L1 state.

To run the external node locally, you need to set its services' ports so they don't overlap with the main node.

For example:

```bash
block_replay_download_address=localhost:3053 \
block_replay_server_address=0.0.0.0:3054 \
sequencer_rocks_db_path=./db/en sequencer_prometheus_port=3313 rpc_address=0.0.0.0:3051 \
cargo run --release
```
