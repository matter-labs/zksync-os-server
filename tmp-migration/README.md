# L1->Gateway Migration Testing Guide

## Setup

First, build and install zkstack from zksync-era commit d1f681c395a5b40fd4cfa591dea8ac3d3f80ebdc. Your output should match below:
```
$ zkstack --version                                                                                                                                                                                   17:01:18
zkstack v0.2.1-d1f681c395
Branch: di/zkstack-zksync-os-fixes
Submodules:
  - contracts: e84aaddf0f8ee0ca175394b0b888436e6724e405
  - proof-manager-contracts: ada04d2b313da1ca66faf771d49dc1f569a9b359
Build timestamp: 2026-03-31 18:31:14
```

Next, run L1 (first terminal window):
```
$ gzip -dfk ./local-chains/v31.0/l1-state.json.gz
$ anvil --load-state local-chains/v31.0/l1-state.json
```

Then, run gateway (second terminal window):
```
$ cargo run --release -- --config ./local-chains/local_dev.yaml --config ./local-chains/v31.0/gateway/config.yaml
```

Finally, run L1-settling chain 6565 (third terminal window):
```
# Clean db/node1/ as the 6565 chain will initialize clean state there
$ rm -rf db/node1/
# Make sure to disable ephemeral mode by passing GENERAL_EPHEMERAL=false, otherwise there will be no persistence and
# the flow requires one restart during migration
$ GENERAL_EPHEMERAL=false cargo run --release -- --config ./local-chains/local_dev.yaml --config ./local-chains/v31.0/l1_settling/config.yaml
```

Wait for gateway and chain 6565 to stop producing logs, they should stop at something like this:
```
# Gateway
▶▶▶ Batch has been fully processed batch_number=10 ...
# Chain 6565
▶▶▶ Batch has been fully processed batch_number=2
```

## Run Migration

For this section make sure you are inside `tmp-migration` directory in a forth terminal window.

First, we need to pause deposits for chain 6565:
```
$ zkstack chain pause-deposits --chain 6565 --l1-rpc-url http://localhost:8545

┌   ZK Stack CLI 
│
◐  Executing pausing deposits before initiating migration for chain 6565
Command: forge script deploy-scripts/AdminFunctions.s.sol --legacy --ffi --rpc-url=http://localhost:8545 --sig=8e465dc90000000000000000000000001b375d88353e65ec63558649cac8a965b500c98100000000000000000000000000000000000000000000000000000000000019a50000000000000000000000000000000000000000000000000000000000000001 --broadcast --private-key=fdf8576ccde9e1e83de63861c09c9da942afc484a11af65e11586b1c9d043fcf
◇  Executing pausing deposits before initiating migration for chain 6565 done in 2.860540923 secs
```

Then, notify server about gateway migration:
```
$ zkstack chain gateway notify-about-to-gateway-update --chain 6565 --l1-rpc-url http://localhost:8545
...
◇  Waiting for transaction to complete done in 7.000592864 secs
│
●  Transaction 0x6253753655bae0a99c6c7b35065255859d34c9b7a53b5f8fa468260001a357ac completed!
```

Now, run migration itself:
```
$ zkstack chain gateway migrate-to-gateway --chain 6565 --gateway-chain-name 506 --l1-rpc-url http://localhost:8545 --gateway-rpc-url http://localhost:3052
...
●  Waiting for transaction with hash 0x9ae6f37bc96e0657c35b0247ffe647c6c596995babb3b07913babf1a25262ad8 to complete...
│  
●  Transaction completed successfully!
```

Make sure it finished successfully. Meanwhile, chain 6565 MUST have crashed with the following log:
```
ERROR zksync_os_l1_watcher::settlement_layer_watcher: all migration preconditions met; restarting node to reinitialise against new settlement layer initial=0x0000000000000000000000000000000000000000 current=0x56324fc48abe809463ff9Bcf2f79f11702d467D0 trigger_batch_number=3 executed=2
```

Make sure gateway migration has been finalized (todo: unsure what exactly happens there, weirdly everything works without this step right now):
```
$ zkstack chain gateway finalize-chain-migration-to-gateway --chain 6565 --gateway-chain-name 506 --l1-rpc-url http://localhost:8545 --gateway-rpc-url http://localhost:3052 --deploy-paymaster=false
...
◇  Executing setting DA validator pair (SL = 0xd03ae123c6b05b2687aacdc3afebe6be8e521ce5, L2 = BlobsAndPubdataKeccak256) via gateway done in 2.809097738 secs
│
◆  Chain initialized successfully
```

Restart chain 6565 with the following config:
```
GENERAL_EPHEMERAL=false \
    GENERAL_GATEWAY_RPC_URL=http://localhost:3052 \
    L1_SENDER_PUBDATA_MODE=RelayedL2Calldata \
    L1_SENDER_OPERATOR_COMMIT_SK=0x5d84f980a57308bcb639c842718fe52008ef6d45ac34e568273c0056a221aa39 \
    cargo run --release -- --config ./local-chains/local_dev.yaml --config ./local-chains/v31.0/l1_settling/config.yaml
```

It will then continuously settle on GW and import interop roots/fees
