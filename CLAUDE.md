# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

### Basic Commands
- **Build**: `cargo build` or `cargo build --release`
- **Run locally**: `./run_local.sh ./local-chains/v30.2/default`
- **Format**: `cargo fmt --all -- --check`
- **Lint**: `cargo clippy --all-targets --all-features --workspace --exclude zksync_os_integration_tests -- -D warnings`
- **Unit tests**: `cargo nextest run --workspace --exclude zksync_os_integration_tests`
- **Integration tests**: `cargo nextest run -p zksync_os_integration_tests --profile no-pig` (no live anvil needed — each test manages its own L1/node; `--profile no-pig` disables Prover Input Generation for faster runs)

### Local Development Setup
1. Run script: `./run_local.sh ./local-chains/v30.2/default`
2. To restart chain: `rm -rf db/*` then re-run the script

### External Node Mode
Set environment variables:
```
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

## Before Submitting a PR

**Run all of the following checks before EVERY push to the branch — not just the first one.** Skipping any of these is not acceptable; every push must pass all checks.

1. **Format**: `cargo fmt --all --check`
2. **Lint**: `cargo clippy --all-targets --all-features --workspace -- -D warnings`
3. **Unit tests**: `cargo nextest run --release --workspace --exclude zksync_os_integration_tests`
4. **Integration tests**: `cargo nextest run -p zksync_os_integration_tests` (no live anvil needed — each test manages its own L1/node; use `--profile no-pig` only for a faster lightweight run)

Running every single one of these checks is critically important. CI will catch failures, but catching them locally before pushing saves everyone time and keeps the branch green.

### Tests

Judge whether the change warrants new tests:

- **Bug fix or new logic** — add a unit test covering the case.
- **New subsystem interaction or cross-component flow** — add an integration test in `zksync_os_integration_tests`.
- **Pure refactor, doc change, or config tweak** — tests may not be needed.

**Any bigger change to the server logic must have corresponding integration tests. Adding those integration tests is part of the scope of the PR — do not consider the PR complete until they are included.**

If no tests were added, include one sentence in the PR description explaining why (e.g. _"No tests added — this is a documentation-only change."_ or _"No tests added — the behaviour is already covered by existing integration tests."_).

### PR title

PR titles must follow the [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/) specification:

```
<type>(<scope>): <short description>
```

Examples: `feat(eth_sender): Support new transaction type`, `fix(state_keeper): Correctly handle edge case`, `ci: Add new workflow for linting`

### Breaking changes

If the PR title uses the breaking-change marker (`feat!: ...`, `fix!: ...`), you **must** uncomment and fill in the **Breaking Changes** and **Rollout Instructions** sections in the PR description (see `.github/pull_request_template.md`).

### Wire format immutability

Do **not** modify existing versioned wire format files under `lib/network/src/wire/replays/v*.rs`. Add a new versioned file instead.

## Architecture Overview

### Core Subsystems
The ZKsync OS Sequencer is organized into three main subsystems:

1. **Sequencer Subsystem** (mandatory) - `lib/sequencer/`
   - Executes transactions in VM and sends results downstream
   - Handles both `Produce` and `Replay` commands uniformly
   - Persists blocks in WAL (`block_replay_storage.rs`)
   - Pushes to state storage and exposes to API

2. **API Subsystem** (optional) - `lib/rpc/` and `lib/rpc_api/`
   - Shared access to state storage
   - Exposes Ethereum-compatible JSON RPC on port 3050
   - Supports `eth_` namespace and minimal `zks_` namespace
   - Includes `ots_` namespace for Otterscan integration

3. **Batcher Subsystem** (main node only) - processes blocks into batches
   - Turns stream of blocks into batches (1 batch = 1 proof = 1 L1 commit)
   - Computes Prover Input by running RiscV binary
   - Manages Merkle Tree with materialized root hashes
   - Runs L1 senders for commit/prove/execute operations

### Key Library Components
- **`lib/state/`** - VM execution state (key-value storage and preimages)
- **`lib/types/`** - Common types across the system
- **`lib/storage/`** and **`lib/storage_api/`** - Data persistence layer
- **`lib/merkle_tree/`** - Persistent Merkle tree for batch proofs
- **`lib/l1_watcher/`** - Monitors L1 for priority transactions
- **`lib/mempool/`** - L2 transaction pool (using Reth components)
- **`lib/multivm/`** - VM execution layer
- **`lib/contract_interface/`** - L1 contract interactions

### Data Flow
1. **Command Source** generates block production commands
2. **BlockContextProvider** maintains block context (L1 priority IDs, block hashes)
3. **L1Watcher** monitors priority transactions from L1
4. **L2Mempool** manages pending L2 transactions
5. **BlockExecutor** executes blocks (stateless)
6. **State** stores execution results and VM state
7. **Repositories** provide API access to block/transaction data
8. **Batcher** (if enabled) processes blocks into provable batches

### Ports
- `3050` - L2 JSON RPC
- `3060` - P2P communication (used for External Nodes)
- `3124` - Prover API (if enabled)
- `3312` - Prometheus metrics

### Configuration
- Main config: `node/sequencer/config.rs`
- Override with environment variables
- Example: `prover_api_fake_provers_enabled=false cargo run --release`

### State Recovery
Most components are designed to be stateless or recover from persistent storage. The system follows a replay-based recovery model where components can reconstruct their state by replaying blocks from the last compacted state.
