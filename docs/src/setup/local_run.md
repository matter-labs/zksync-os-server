## Run

### Local

To run node locally, first launch `anvil`:

```
anvil --load-state zkos-l1-state.json --port 8545
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
PRIVATE_KEY = 0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
ACCOUNT_ID = 0x36615Cf349d7F6344891B1e7CA7C72883F5dc049
```

Example transaction to send:

```
cast send -r http://localhost:3050 0x5A67EE02274D9Ec050d412b96fE810Be4D71e7A0 --value 
100 --private-key 0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
```

**Config options**

See `node/sequencer/config.rs` for config options and defaults. Use env variables to override, e.g.:

```
prover_api_fake_provers_enabled=false cargo run --release
```
