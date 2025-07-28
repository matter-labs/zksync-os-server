## Integration Tests

This directory contains a fairly simple framework designed to test ZKsync OS node's behavior e2e on a local setup.

Main design considerations:
* **Isolation**. Every test initializes its own node, L1, persistence resources etc. Meaning you can run every test independently of each other.
* **Speed**. Every test environment spins up very quickly (<3 seconds ) thanks to pre-initialized L1 with some reasonable defaults.
* **ZKsync-agnostic**. Test try to use general Ethereum tooling where this makes sense (e.g., base `alloy`) to enforce our compatibility with Ethereum.
* **Rust-first**. No reliance on external tooling to run the tests, regular Rust flows should work as expected. `RUST_LOG`/`RUST_BACKTRACE` get propagated to all relevant components (node, prover).

Known limitations:
* No support for node restarts - not a fundamental issue, can be added in the future
* No L1 logs - `alloy` provider swallows `anvil` logs by default with no option to disable this behavior, but this can be solved by writing our own `anvil` spawner
* No graceful shutdown - support is blocked by the node itself not doing graceful shutdown yet

### Run

Pre-requisites:
* Make sure you have `cargo-nextest` version `0.9.101` or later installed in your system as we use some recently added features.
* Make sure you have `forge` installed to be able to build test contracts (see [README](./test-contracts/README.md)).
* Use `RUST_BACKTRACE=1` (or `RUST_BACKTRACE=full`) to enable backtraces.
* Configure logging level via `RUST_LOG=debug` (or `RUST_LOG=info,zksync_os_sequencer=debug` for specific components).

#### Examples

```shell
# All tests (excluding prover)
cargo nextest run -p zksync_os_integration_tests

# Single test with exact name match
cargo nextest run -p zksync_os_integration_tests -E 'test(=basic_transfers)'
# If you want logs to be printed during the test:
cargo nextest run -p zksync_os_integration_tests -E 'test(=basic_transfers)' --no-capture
# If you want all tests containing a substring:
cargo nextest run -p zksync_os_integration_tests -E 'test(~call)'

# All tests inside `./tests/call.rs`
cargo nextest run -p zksync_os_integration_tests -E 'binary(call)'

# Run prover tests (CPU prover)
# !! Note `--release`, important to avoid stack overflow and low performance in prover !!
cargo nextest run --release -p zksync_os_integration_tests --features prover-tests -E 'binary(prover)'

# Run prover tests (GPU prover)
# !! Note `--release`, important to avoid stack overflow and low performance in prover !!
# !! Requires 24GB of VRAM and CUDA toolkit 12.x installed !!
cargo nextest run --release -p zksync_os_integration_tests --features gpu-prover-tests -E 'binary(prover)'
```
