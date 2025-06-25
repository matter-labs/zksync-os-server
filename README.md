# zksync-os-sequencer
New sequencer implementation for zksync-os

## Running with L1

Make sure you have a 1.x version of `anvil` installed (see [foundry guide](https://getfoundry.sh/)). Then run pre-setup L1:

```
$ anvil --load-state zkos-l1-state.json --port 8545
...
Listening on 127.0.0.1:8545
...
```

You can now start ZKsync OS server in parallel:
```
$ cargo run --release
...
INFO zksync_os_l1_watcher: initializing L1 watcher bridgehub_address=0x4b37536b9824c4a4cf3d15362135e346adb7cb9c chain_id=270 next_l1_block=0
...
```
