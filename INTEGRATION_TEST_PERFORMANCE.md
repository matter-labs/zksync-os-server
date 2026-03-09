# Integration Test Performance Analysis

## Executive Summary

Several integration tests are extremely slow due to: (1) full infrastructure spin-up per test with no sharing, (2) negative-proof tests that must wait the full 180s timeout, (3) the 10x stress multiplier on `main` applying to all tests including the slowest ones, and (4) no nextest parallelism limits causing resource contention. This document ranks the biggest offenders and proposes concrete solutions.

---

## Biggest Offenders (Ranked by Cost)

### Tier 1 — Extremely Expensive (estimated 3-20 min each)

| # | Test | File | Root Cause | Est. Time |
|---|------|------|-----------|-----------|
| 1 | `test_interop_bundle_send` | `tests/interop.rs` | Spins up **3 nodes** (2 L2 + 1 L1), cross-chain deposits, 300s relayer timeout | 5-20 min |
| 2 | `does_not_get_stuck` | `tests/external_node.rs:160` | Deploys **200 contracts** sequentially, each retried on EN (up to 10s each). Already excluded from coverage due to "550s proving computations" | 3-10 min |
| 3 | `batch_verification_with_2_ens` | `tests/external_node.rs:74` | 3 nodes (main + 2 ENs) + **`wait_not_finalized` must wait full 180s** to prove negative | ~3-4 min |
| 4 | `batch_verification_without_enough_ens` | `tests/external_node.rs:44` | 2 nodes + **`wait_not_finalized` = full 180s wait** | ~3-4 min |

### Tier 2 — Expensive (estimated 30s-3 min each)

| # | Test | File | Root Cause | Est. Time |
|---|------|------|-----------|-----------|
| 5 | `transaction_replay` | `tests/external_node.rs:122` | 3 L2 servers spun up sequentially | 30-60s |
| 6 | `batch_verification_works` | `tests/external_node.rs:16` | 2 nodes + wait for finalization (180s timeout) | 30-60s |
| 7 | `upgrade_to_v31_with_deployments` | `tests/upgrade/mod.rs` | Protocol upgrade + EN launch + unbounded sync polling | 30-60s |
| 8 | `erc20_withdrawal` | `tests/erc20.rs` | Multi-layer round trips: deploy → L1 deposit → L2 withdraw → L1 finalize | 30-60s |
| 9 | `l1_withdraw` | `tests/l1.rs` | `expect_to_execute` polls for L1 execution (180s timeout) + withdrawal finalization | 30-60s |
| 10 | `test_interop_erc20_transfer_manual` | `tests/interop.rs` | 3 nodes + cross-chain ERC20 transfer | 30-60s |
| 11 | `test_interop_root_propagation` | `tests/interop.rs` | 3 nodes + interop root propagation | 30-60s |

### Tier 3 — Moderate (10-30s each)

Tracing tests (7 tests, each spins up full Tester), `basic_transfers` (100 concurrent txs), filter/pubsub tests.

### Tier 4 — Baseline (~5-15s each)

Simple API tests, `call.rs`, `mempool.rs::sensitive_to_balance_changes`.

---

## Systemic Issues

### 1. No Infrastructure Sharing Between Tests

**Impact: HIGH** — Every test calls `Tester::setup()` → `TesterBuilder::build()` which:
- Starts a fresh Anvil L1 node (decompress state, boot, retry connect up to 10s)
- Launches a full ZKsync OS L2 server (acquire 5 ports, create RocksDB, retry connect up to 10s)
- Waits for L2 wallet funding (poll up to 10s)

**Minimum overhead per test: ~3-5s, worst case: ~30s.** With 49 tests, that's 2.5-25 min of pure setup.

### 2. `wait_not_finalized` Is Inherently O(timeout)

**Impact: HIGH** — `provider.rs:61-71`: This function proves a negative by waiting the *entire* `DEFAULT_TIMEOUT` (180s). Used by 2 tests (`batch_verification_without_enough_ens` and `batch_verification_with_2_ens`), this alone accounts for **~6 minutes** of wall-clock time.

### 3. 10x Stress Multiplier on `main` Applies to Everything

**Impact: CRITICAL on `main` branch** — `.github/workflows/ci.yml:22`:
```yaml
NEXTEST_ITERATIONS: ${{ ... '--stress-count 10' || '' }}
```
On `main`, the 180s `wait_not_finalized` tests run 10 times each = **60 minutes** just for those 2 tests.

### 4. No Nextest Thread/Slot Limits

**Impact: MEDIUM** — No `threads-required` or parallelism limits configured in `.config/nextest.toml`. Each test spawns 1-3 full server processes, so running them all in parallel causes CPU/memory/port contention.

---

## Concrete Recommendations

### R1. Reduce `wait_not_finalized` Timeout (Quick Win — saves ~5 min per run)

The 180s timeout for proving a negative is excessive. If finalization hasn't happened in 15-30s, it's not going to.

```rust
// external_node.rs — change these two calls:
.wait_not_finalized(1, Duration::from_secs(20))  // was DEFAULT_TIMEOUT (180s)
```

**Expected saving: ~5 min per normal run, ~50 min on `main` with stress.**

### R2. Exclude Slow Integration Tests from `--stress-count` (Quick Win)

Add a nextest filter to the CI to exclude slow tests from stress runs:

```yaml
# ci.yml
NEXTEST_ITERATIONS: ${{ ... '--stress-count 10 -E "not test(~interop) & not test(~batch_verification) & not test(~does_not_get_stuck)"' || '' }}
```

Or create a `[profile.stress]` in nextest.toml that filters them:

```toml
[[profile.stress.overrides]]
filter = 'test(~wait_not_finalized) | test(~interop) | test(~does_not_get_stuck)'
# Run these only once even during stress testing
max-retries = 0
```

### R3. Add `threads-required` for Multi-Node Tests (Quick Win)

Prevent resource contention by telling nextest how many resources each test needs:

```toml
# .config/nextest.toml
[[profile.default.overrides]]
filter = 'test(~interop)'
threads-required = 4  # 3 nodes need significant resources

[[profile.default.overrides]]
filter = 'test(~external_node) | test(~batch_verification)'
threads-required = 2

[[profile.default.overrides]]
filter = 'test(~upgrade)'
threads-required = 2
```

### R4. Reduce `does_not_get_stuck` Contract Deploys (Medium Effort)

200 sequential deploys is excessive for testing liveness. Reduce to 20-50:

```rust
const REPEATS: usize = 50;  // was 200
```

**Expected saving: 75% reduction in that test's runtime.**

### R5. Share Tester Infrastructure Across Related Tests (Higher Effort — biggest payoff)

Group tests that use the same setup into modules that share a single `Tester`:

```rust
// Example: tests that just need a basic Tester could share one
mod basic_node_tests {
    use std::sync::OnceLock;
    static TESTER: OnceLock<Tester> = OnceLock::new();

    async fn shared_tester() -> &'static Tester {
        // Initialize once, reuse across tests
    }
}
```

Or use nextest's `test-group` feature to run related tests sequentially on a shared fixture.

**Expected saving: Eliminate ~40 redundant node startups → save 2-5+ min.**

### R6. Add a `slow` Test Group in Nextest Config

Tag slow tests and let developers skip them during local development:

```toml
# .config/nextest.toml
[profile.fast]
default-filter = 'not test(~interop) & not test(~does_not_get_stuck) & not test(~batch_verification_without) & not test(~batch_verification_with_2)'
```

Developers can run `cargo nextest run -p zksync_os_integration_tests --profile fast` for a quick feedback loop.

---

## era-contracts: Additional Findings

### E1. `optimizer_runs = 9999999` in `foundry.toml` (line 45)

Extremely high optimizer runs maximizes deployed code efficiency but massively inflates compilation time and bytecode size. Tests never deploy to mainnet.

**Fix:** Add a test profile with lower optimization:
```toml
[profile.test]
optimizer_runs = 200  # or optimizer = false
```

### E2. Full Ecosystem Redeployment per Foundry Test Function

`L1GatewayTests.t.sol`, `AssetRouterTest.t.sol`, etc. call `prepare()` in `setUp()`, deploying 40-50 contracts per `test_*` function. With ~15 integration test functions, that's ~600-750 contract deployments per suite run.

**Fix:** Use Foundry's `vm.snapshot()` / `vm.revertTo()` to snapshot state after `prepare()` and restore it instead of redeploying:

```solidity
uint256 snapshotId;

function setUp() public {
    if (snapshotId == 0) {
        prepare();
        snapshotId = vm.snapshot();
    } else {
        vm.revertTo(snapshotId);
        snapshotId = vm.snapshot(); // re-snapshot since revertTo consumes it
    }
}
```

### E3. Coverage Runs Tests Twice

`l1-contracts-ci.yaml` runs `yarn test:foundry && yarn coverage:foundry`, which runs overlapping test sets.

**Fix:** Run coverage in a single pass or exclude integration tests from the duplicate run.

---

## Priority Matrix

| # | Recommendation | Effort | Impact | Priority |
|---|---------------|--------|--------|----------|
| R1 | Reduce `wait_not_finalized` timeout | 5 min | ~5 min saved/run | **P0** |
| R2 | Exclude slow tests from stress | 15 min | ~50 min saved on main | **P0** |
| R3 | Add `threads-required` to nextest | 10 min | Prevents contention | **P0** |
| R4 | Reduce `does_not_get_stuck` repeats | 5 min | ~2-5 min saved | **P1** |
| R6 | Add `fast` nextest profile | 10 min | Better dev experience | **P1** |
| E1 | Lower optimizer_runs for tests | 5 min | Faster compilation | **P1** |
| E2 | Use vm.snapshot() in Foundry tests | 1-2 hr | ~10x fewer deploys | **P1** |
| R5 | Share Tester across tests | 2-4 hr | ~3-5 min saved | **P2** |
| E3 | Deduplicate coverage run | 30 min | ~50% CI time saved | **P2** |
