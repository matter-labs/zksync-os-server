# zksync-os-server Benchmark Notes

Use these defaults when benchmarking `zksync-os-server`.

## Setup

Install system packages:

```bash
apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y \
  build-essential clang lld pkg-config libssl-dev protobuf-compiler git curl jq htop tmux
```

Install Rust if missing:

```bash
curl https://sh.rustup.rs -sSf | sh -s -- -y
. "$HOME/.cargo/env"
rustup toolchain install nightly
rustup default nightly
```

Install Foundry if Anvil is needed:

```bash
curl -L https://foundry.paradigm.xyz | bash
~/.foundry/bin/foundryup
export PATH="$HOME/.foundry/bin:$PATH"
```

## Repository

Clone or sync the repo, then record:

```bash
git rev-parse HEAD
git status --short
lscpu
free -h
df -h .
```

Build benchmark target before measuring:

```bash
cargo test --release -p zksync_os_integration_tests --features in-memory-storage --test suite "parallel_injection_tps" --no-run
```

## Benchmark Commands

Full pipeline:

```bash
PATH="$HOME/.foundry/bin:$PATH" \
RUST_LOG=error \
PARALLEL_BLOCKS=8 \
cargo test --release -p zksync_os_integration_tests --features in-memory-storage \
  --test suite "parallel_injection_tps" -- --nocapture
```

Timing diagnosis:

```bash
PATH="$HOME/.foundry/bin:$PATH" \
RUST_LOG=zksync_os_sequencer::execution::block_executor=info \
PARALLEL_BLOCKS=8 LOAD_TEST_DURATION_SECS=20 LOAD_TEST_WARMUP_SECS=5 \
cargo test --release -p zksync_os_integration_tests --features in-memory-storage \
  --test suite "parallel_injection_tps" -- --nocapture
```

Direct VM-only comparison:

```bash
PATH="$HOME/.foundry/bin:$PATH" \
RUST_LOG=error \
cargo test --release -p zksync_os_integration_tests --features in-memory-storage \
  --test suite "parallel_blocks_tps" -- --nocapture
```

For K sweeps, keep the same instance, commit, log level, warmup, duration, and corpus settings.
