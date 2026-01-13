## Run

### Local

To run node locally, first launch `anvil`:

```
anvil --load-state ./local-chains/v30/zkos-l1-state.json --port 8545
```

then launch the server:

```
cargo run
```

To restart the chain, erase the local DB and re-run anvil:

```
rm -rf db/*
```

By default, fake (dummy) proofs are used both for FRI and SNARK proofs.

**Rich account:**

```
PRIVATE_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
ACCOUNT_ID=0x36615Cf349d7F6344891B1e7CA7C72883F5dc049
```

Example transaction to send:

```
cast send -r http://localhost:3050 0x5A67EE02274D9Ec050d412b96fE810Be4D71e7A0 --value 
100 --private-key 0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
```

**Config options**

See `node/sequencer/config.rs` for config options and defaults. Use a JSON configuration file to override the defaults, e.g.:
```
cargo run --release -- --config ./local-chains/v30/config.json
```
Explore the `local-chains` folder for additional chain configs grouped by protocol version. Detailed information is available in `local-chains/README.md`.

You can also use environment variables to override the default settings:
```
prover_api_fake_provers_enabled=false cargo run --release
```
If both the JSON config file and environment variables are set, the latter takes precedence.


**Ephemeral mode**
Ephemeral mode runs the node using a temporary, isolated state directory, allowing you to spin up one or more local chains without them interfering with the same folder. When enabled, the node creates a temporary base directory for RocksDB and the file-backed object store, this directory is automatically removed on shutdown. To remain as lightweight as possible, Ephemeral mode disables all APIs except for JSON-RPC (status, prometheus APIs etc are unavailable). It can be used for quick local testing and multi-chain setups.

The `ephemeral` setting is part of the general config and can be set like any other config value:
```
general_ephemeral=true cargo run --release
```