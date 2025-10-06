# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

### Basic Commands
- **Build**: `cargo build` or `cargo build --release`
- **Run locally**: `cargo run` (requires `anvil --load-state zkos-l1-state.json --port 8545` running first)
- **Format**: `cargo fmt --all -- --check`
- **Lint**: `cargo clippy --all-targets --all-features --workspace --exclude zksync_os_integration_tests -- -D warnings`
- **Unit tests**: `cargo nextest run --workspace --exclude zksync_os_integration_tests`
- **Integration tests**: `cargo nextest run --profile ci -p zksync_os_integration_tests`

### Local Development Setup
1. Start anvil: `anvil --load-state zkos-l1-state.json --port 8545`
2. Run server: `cargo run`
3. To restart chain: `rm -rf db/*` then restart anvil

### External Node Mode
Use `run_en.sh` script or set environment variables:
```
block_replay_download_address=localhost:3053 \
block_replay_server_address=0.0.0.0:3054 \
sequencer_rocks_db_path=./db/en \
sequencer_prometheus_port=3313 \
rpc_address=0.0.0.0:3051 \
cargo run --release
```

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
- `3053` - Block replay server (for External Nodes)
- `3124` - Prover API (if enabled)
- `3312` - Prometheus metrics

### Configuration
- Main config: `node/sequencer/config.rs`
- Override with environment variables
- Example: `prover_api_fake_provers_enabled=false cargo run --release`

### State Recovery
Most components are designed to be stateless or recover from persistent storage. The system follows a replay-based recovery model where components can reconstruct their state by replaying blocks from the last compacted state.