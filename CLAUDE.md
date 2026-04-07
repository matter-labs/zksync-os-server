# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build and Development Commands

### Basic Commands
- **Build**: `cargo build` or `cargo build --release`
- **Format**: `cargo fmt --all -- --check`
- **Lint**: `cargo clippy --all-targets --all-features --workspace --exclude zksync_os_integration_tests -- -D warnings`
- **Unit tests**: `cargo nextest run --workspace --exclude zksync_os_integration_tests`
- **Integration tests**: `cargo nextest run -p zksync_os_integration_tests --profile no-pig` (no live anvil needed — each test manages its own L1/node; `--profile no-pig` disables Prover Input Generation for faster runs)

### Local Development Setup
1. Run script: `./run_local.sh ./local-chains/v30.2/default`
2. To restart chain: `rm -rf db/*` then re-run the script

## Submitting a PR

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
