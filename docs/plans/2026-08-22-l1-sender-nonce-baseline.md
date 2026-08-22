# Eager L1 Sender Nonce Baseline Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Capture the block-pinned confirmed nonce before the L1 sender can wait for its first command, preserving alignment with the startup batch snapshot.

**Architecture:** `run_l1_sender` will resolve the confirmed nonce at `l1_block_number` before processing prepending passthrough commands. It will pass the captured value into the selected recovery implementation, which will no longer perform a delayed RPC or fall back to `latest`.

**Tech Stack:** Rust, Tokio, Alloy mocked transport, `cargo nextest`, Clippy.

---

### Task 1: Add the eager-query regression test

**Files:**
- Modify: `lib/l1_sender/src/lib.rs` test module

**Step 1: Add mocked-provider test support**

Import Alloy's `ProviderBuilder`, `EthereumWallet`, `Header`, `U64`, and mock `Asserter`, plus a local `SigningKey`. Add a helper that queues the four `NodeProvider` capability-detection responses and constructs a wallet-capable mocked provider.

**Step 2: Write the failing test**

Add `captures_confirmed_nonce_before_waiting_for_first_command`. Construct a pipelined `L1Sender<CommitCommand>` with `l1_block_number = 42`, queue one transaction-count response, and start `run_l1_sender` with an open but empty input channel. Poll until the mock response queue is empty:

```rust
tokio::time::timeout(Duration::from_secs(1), async {
    while !asserter.read_q().is_empty() {
        tokio::task::yield_now().await;
    }
})
.await
.expect("confirmed nonce should be captured before waiting for input");
```

Keep the sender task alive only for the assertion, then abort it.

**Step 3: Run the test and verify RED**

Run:

```bash
cargo nextest run -p zksync_os_l1_sender captures_confirmed_nonce_before_waiting_for_first_command
```

Expected: FAIL because the nonce response remains queued while the current implementation waits for the first input command.

### Task 2: Capture and thread the nonce baseline

**Files:**
- Modify: `lib/l1_sender/src/lib.rs`
- Modify: `lib/l1_sender/src/pipelined.rs`

**Step 1: Add the eager capture helper**

Add an async helper on `L1Sender<Input>` that resolves the operator address and calls:

```rust
self.provider
    .get_transaction_count(operator_address)
    .block_id(BlockId::number(self.l1_block_number))
    .await
    .context("get confirmed transaction count")
```

**Step 2: Capture before the input wait**

At the beginning of `run_l1_sender`, capture the value when pipelining is enabled or stop-and-wait recovery is enabled. Do this before `process_prepending_passthrough_commands`.

**Step 3: Thread the captured value through recovery**

Pass the captured nonce into `run_stop_and_wait` / `recover_in_flight_txs` and `run_pipelined` / `plan_pipelined_recovery`. Remove both delayed block queries and both `latest` fallbacks.

**Step 4: Run the regression test and verify GREEN**

Run the focused command from Task 1. Expected: PASS.

### Task 3: Verify and commit

**Files:**
- Verify: `lib/l1_sender/src/lib.rs`
- Verify: `lib/l1_sender/src/pipelined.rs`
- Verify: `docs/plans/2026-08-22-l1-sender-nonce-baseline-design.md`
- Verify: `docs/plans/2026-08-22-l1-sender-nonce-baseline.md`

**Step 1: Run focused tests**

```bash
cargo nextest run -p zksync_os_l1_sender
```

Expected: all tests pass.

**Step 2: Run formatting and linting**

```bash
cargo fmt --all -- --check
cargo clippy -p zksync_os_l1_sender --all-targets --all-features -- -D warnings
```

Expected: both commands exit successfully with no warnings.

**Step 3: Review the final diff**

Run `git diff --check` and inspect `git diff origin/main...HEAD` for snapshot alignment, duplicated logic, and unrelated changes.

**Step 4: Commit and push**

Commit the test and implementation with:

```bash
git commit -m "fix(l1_sender): capture nonce baseline eagerly"
```

Push `HEAD` to `origin/fix/l1-sender-nonce-baseline-fallback` so PR #1543 updates.
