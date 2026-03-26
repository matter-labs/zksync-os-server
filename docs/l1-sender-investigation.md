# L1 Sender — Investigation and Proposed Solutions

## Context

The L1 sender runs as a pipeline component on the main node. When it crashes, the entire binary crashes (via `.expect("pipeline segment failed")` in `builder.rs:85`). On Kubernetes, repeated crashes trigger CrashLoopBackOff, and the only alert fires on user impact ("no batches committed in 1.5h"). This means any crash causes **1.5 hours of silent downtime** in the worst case.

The core problem: the L1 sender treats almost every error as fatal and crashes, but most errors are transient or recoverable.

---

## Issue 1: Gas Price Spikes → Crash Loop

### Current Behavior

1. `tx_request_with_gas_fields()` estimates EIP-1559 fees from the network
2. Fees are capped at configured maximums (`max_fee_per_gas` = 200 gwei, `max_priority_fee_per_gas` = 1 gwei)
3. Transaction is submitted with the capped (possibly too low) fees
4. If gas prices are above the cap, the tx sits in the mempool
5. After 300s timeout → `PendingTransactionError::Timeout` → crash
6. On restart, the old tx may still be pending. New tx submitted with same nonce. If old tx gets mined first, new one reverts → crash. If new one gets mined first, old one wastes gas.
7. **CrashLoopBackOff**

### Why It's Bad

- Gas spikes on Ethereum are common (NFT mints, market events, network congestion)
- The 300s timeout is too short for sustained congestion
- Each restart attempt wastes operator time and risks double-spending gas
- No escalation mechanism means the sender can be stuck indefinitely during a gas spike

### Proposed Solution: Retry with Fee Escalation (No Crash)

Replace the crash-on-timeout with an in-process retry loop:

```
loop {
    estimate fees (capped at config max)
    submit tx
    wait for inclusion with timeout

    if included → proceed
    if timeout:
        bump fees (e.g., 1.5x previous, up to config max)
        cancel old tx (submit zero-value replacement at same nonce)
        retry with bumped fees
    if fees would exceed config max:
        log warning, enter backoff (sleep 30s), re-estimate
        do NOT crash
}
```

Key design points:
- **Never crash on timeout.** Sleep and retry.
- **Replace, don't duplicate.** When retrying, send a replacement tx at the same nonce with higher gas to avoid double-spending.
- **Respect the configured cap.** If the network fee exceeds `max_fee_per_gas`, wait (don't send) — but also don't crash. Poll periodically until fees drop below the cap.
- **Log clearly.** Emit metrics and warnings so operators know the sender is gas-blocked, not dead.

This is what era does (exponential escalation bounded by a cap), adapted to the pipeline model.

---

## Issue 2: Failing L1 Transactions

### Current Behavior

When a transaction reverts on L1 (`validate_tx_receipt()` at `lib.rs:359-401`):
1. The receipt has `status() == false`
2. The sender fetches a debug trace (best-effort)
3. Logs the revert reason
4. `anyhow::bail!(...)` → crash

### Why It's Bad

- Real ETH was already burned on gas for the reverted tx
- The crash provides no way to inspect the state before retrying
- On restart, the sender may re-submit the same batch with the same (revert-causing) data → another revert → more wasted gas → CrashLoopBackOff
- Common revert causes: stale `previous_stored_batch_info`, L1 protocol version mismatch (if UpgradeGatekeeper has a bug), signature threshold not met

### Proposed Solution: Pre-flight Simulation + Categorized Revert Handling

**A) Add `eth_call` simulation before every `send_raw_transaction`:**

```rust
// Before sending, dry-run the tx
let sim_result = provider.call(&tx_request).block(BlockId::pending()).await;
if let Err(revert) = sim_result {
    // Transaction would revert — don't send it, don't waste gas
    tracing::error!(?revert, "pre-flight simulation failed, not sending");
    // Decide: retry later? skip batch? crash?
}
```

This catches reverts for free (no gas cost). In most cases, a revert that happens in simulation will also happen on-chain, so there's no point submitting.

**B) Categorize revert reasons and react differently:**

| Revert Reason | Action |
|---|---|
| State root mismatch | Wait and retry — L1 state may not have caught up |
| Protocol version mismatch | Wait — UpgradeGatekeeper should have caught this |
| Signature verification failed | Log error, don't retry — batch data is bad |
| Gas estimation failure | Bump gas limit, retry |
| Unknown | Log full trace, crash (truly unexpected) |

**C) If a tx does revert on-chain despite simulation:**

Don't immediately crash. Log the full debug trace, emit a metric (`l1_sender_tx_reverted`), and enter a cooldown period. Re-simulate the same command — if it still reverts, halt the specific sender (not the whole binary) and alert.

---

## Issue 3: Broken L1 Provider

### Current Behavior

Every RPC call propagates errors with `?`:
- `provider.estimate_eip1559_fees().await?`
- `provider.get_blob_base_fee().await?`
- `provider.fill(tx_request).await?`
- `provider.send_raw_transaction(...).await?`
- `provider.get_block(BlockId::pending()).await?`
- `provider.get_balance(...).await?`
- `provider.get_transaction_count(...).await?`

The provider has a retry layer (`OptimisticRetryPolicy` in `provider.rs`): 2 retries with 200ms backoff, covering HTTP 429/500/502/503 and Infura's -32603. After 3 total attempts, the error propagates → crash.

### Why It's Bad

- L1 RPC providers (Alchemy, Infura, QuickNode) have outages lasting minutes to hours
- 3 attempts with 200ms backoff = the sender gives up after ~600ms
- A 5-minute Alchemy outage causes hundreds of crash-restart cycles
- Known issue in comments: *"Crashes when there is a gap in incoming L1 blocks (happens periodically with Infura provider)"* — this is `provider.get_block(BlockId::pending()).await?.expect("no pending block")` at `lib.rs:167`

### Proposed Solution: Resilient RPC Handling

**A) Wrap transient RPC errors in a retry loop at the L1 sender level (not just the transport level):**

The alloy retry layer handles transport-level errors (HTTP 500, etc.). But the L1 sender should also handle **application-level** transient failures:

```rust
// Instead of:
let eip1559_est = provider.estimate_eip1559_fees().await?;

// Do:
let eip1559_est = retry_with_backoff(|| provider.estimate_eip1559_fees(), max_retries=10, backoff=5s).await?;
```

Or better: wrap the entire "build + send" phase in a retry loop that distinguishes transient vs permanent failures.

**B) Remove the `.expect("no pending block")` at `lib.rs:167`:**

This is the Infura crash. Replace with:
```rust
let pending_block = match provider.get_block(BlockId::pending()).await? {
    Some(block) => block,
    None => {
        // Infura sometimes returns None for pending blocks.
        // Fall back to latest block timestamp for Fusaka check.
        provider.get_block(BlockId::latest()).await?
            .expect("no latest block")
    }
};
```

The pending block is only used to check the Fusaka upgrade timestamp — using latest block is an acceptable fallback.

**C) Distinguish transient from permanent errors:**

```rust
enum L1Error {
    /// RPC timeout, rate limit, network blip — retry indefinitely with backoff
    Transient(anyhow::Error),
    /// Nonce conflict, tx revert — retry may help after state changes
    Recoverable(anyhow::Error),
    /// Data corruption, unsupported version — cannot recover
    Fatal(anyhow::Error),
}
```

The main loop should:
- **Transient**: retry with exponential backoff (5s → 10s → 30s → 60s, capped)
- **Recoverable**: retry a few times, then halt the sender and alert
- **Fatal**: crash (this is the only case where crashing is appropriate)

**D) Consider provider health checks:**

Before entering the send phase, do a lightweight health check (e.g., `eth_blockNumber`). If the provider is down, enter a backoff loop instead of attempting a full transaction cycle that will fail at each step.

---

## Issue 4: Priority Fees Are Ineffective

### Current Behavior

Default config: `max_priority_fee_per_gas = 1 gwei`

In `tx_request_with_gas_fields()`:
1. Alloy's `estimate_eip1559_fees()` returns the network's suggested priority fee
2. If `estimated > configured_max`, the **configured max (1 gwei) is used**
3. A warning is logged: *"this may result in inclusion delay"*
4. The tx is submitted with 1 gwei priority fee

### Why It's Bad

- Ethereum mainnet median priority fee is typically 0.5-5 gwei, spiking to 20-100+ gwei during congestion
- With a 1 gwei cap, the sender's transactions are **always deprioritized** during any congestion
- Validators sort mempool by priority fee — 1 gwei means "include me last"
- The 300s timeout comment explicitly acknowledges this: *"60-120 is enough for lower gas price transactions"* — the design knows it's sending low-priority txs and compensates with a longer timeout
- But the timeout leads to crashes (Issue 1), so the compensation doesn't work

### The Deeper Problem

The config caps (`max_fee_per_gas`, `max_priority_fee_per_gas`) serve as a **cost safety net** — preventing the operator from overpaying during a flash spike. This is good. But the implementation conflates "safety cap" with "actual fee to use":

- When network says "5 gwei priority fee" and cap is "1 gwei", using 1 gwei is correct from a cost perspective but wrong from an inclusion perspective
- The operator set 1 gwei because they don't want to pay more than 1 gwei per gas in tips. But what they actually want is: "use the network-suggested fee, but don't let it go above X"

**Currently, the cap is always active.** On Ethereum mainnet, `estimated_priority > 1 gwei` is true most of the time, so the cap fires on almost every transaction.

### Proposed Solution: Smarter Fee Strategy

**A) Use network-estimated fees by default, cap only as a safety net:**

Change the semantics: the configured values are **upper bounds**, not target values. When the network estimate is below the cap, use the estimate. When above, use the cap. This is how it already works for `max_fee_per_gas` — but the `max_priority_fee_per_gas` default of 1 gwei is so low that it's effectively always the cap.

**Recommendation: raise the default `max_priority_fee_per_gas` to something realistic** — e.g., 10-20 gwei. This still provides cost protection but allows normal inclusion during moderate congestion.

**B) Consider separating "target" from "cap":**

```rust
struct FeeConfig {
    /// Target priority fee — used when network estimate is below this
    target_priority_fee: u128,   // e.g., 2 gwei
    /// Maximum priority fee — hard cap, never exceeded
    max_priority_fee: u128,      // e.g., 50 gwei
}
```

Logic:
- If `estimated < target`: use target (ensures minimum tip for fast inclusion)
- If `target <= estimated <= max`: use estimated (follow the market)
- If `estimated > max`: use max (cost protection, accept slower inclusion)

**C) For `max_fee_per_gas`: the 200 gwei default is reasonable** for current Ethereum (base fee rarely exceeds 100 gwei). But it should be documented as "this is the maximum ETH you're willing to burn per gas unit, not a target."

**D) For `max_fee_per_blob_gas`: the 2 gwei default is very low.** Blob base fee can spike to 10-100+ gwei during high blob demand. Raise to 50-100 gwei as a default, or at least to match blob market conditions.

---

## Issue 5: Every Error Crashes the Entire Binary

### Current Behavior

```rust
// builder.rs:84-85
res = component.run(input_receiver, output_sender) => {
    res.expect("pipeline segment failed");
```

Any `Err` from `run_l1_sender()` panics the pipeline component task, which is spawned as `spawn_critical_with_graceful_shutdown_signal` — causing the entire binary to terminate.

### Why It's Bad

- A 200ms RPC timeout kills the same binary that runs the sequencer, API, mempool, and every other subsystem
- On Kubernetes, binary crash → pod restart → CrashLoopBackOff if it happens repeatedly
- All pipeline components crash together — one L1 sender taking down the sequencer is not proportional

### Proposed Solution: Error Containment in the Main Loop

Move error handling **inside** `run_l1_sender` rather than propagating everything outward:

```rust
loop {
    match try_send_cycle(&mut inbound, &outbound, &provider, &config).await {
        Ok(()) => continue,
        Err(L1Error::Transient(e)) => {
            tracing::warn!(?e, "transient error, retrying in 10s");
            metrics.transient_errors.inc();
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
        Err(L1Error::Recoverable(e)) => {
            tracing::error!(?e, "recoverable error, cooling down for 60s");
            metrics.recoverable_errors.inc();
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        Err(L1Error::Fatal(e)) => {
            tracing::error!(?e, "fatal error, cannot continue");
            return Err(e);  // Only this propagates to the pipeline
        }
    }
}
```

This way:
- Transient RPC errors → retry with short backoff (the sender stays alive)
- Recoverable errors (gas too high, nonce conflict) → longer backoff, then retry
- Only truly fatal errors (data corruption, unsupported protocol version) crash the binary

---

## Summary: Error Classification

| Error | Current | Proposed | Category |
|---|---|---|---|
| RPC timeout/500/502/503 | Crash after 3 transport retries | Retry indefinitely with backoff | Transient |
| `estimate_eip1559_fees` failure | Crash | Retry with backoff | Transient |
| `get_blob_base_fee` failure | Crash | Retry with backoff | Transient |
| `get_block(pending)` returns None | Panic | Fallback to latest block | Transient |
| Tx timeout (300s, gas too high) | Crash | Bump fees, replace tx, retry | Recoverable |
| Blob fee above cap | Warn, send anyway, stuck, timeout, crash | Wait until fee drops below cap | Recoverable |
| Tx reverted on L1 | Crash | Pre-flight simulation prevents; categorize reverts | Recoverable / Fatal |
| Nonce conflict on restart | Crash loop | Detect in-flight txs on startup | Recoverable |
| Zero operator balance | Crash | Crash (correct — needs manual funding) | Fatal |
| Unsupported protocol version | Panic | Crash (correct — needs code update) | Fatal |
| Corrupted blob data | Crash | Crash (correct — data issue) | Fatal |

---

## Implementation Priority

1. **Error containment** (Issue 5) — wrap the main loop in retry logic. This is the foundation for all other fixes. Without it, every improvement still crashes the binary.

2. **Gas price spike handling** (Issue 1) — add fee escalation and tx replacement. This is the most common production incident.

3. **RPC resilience** (Issue 3) — add application-level retries, fix the pending block panic. This handles the second most common failure mode.

4. **Priority fee defaults** (Issue 4) — raise `max_priority_fee_per_gas` default, consider target/cap separation. Quick config change with immediate impact.

5. **Pre-flight simulation** (Issue 2) — add `eth_call` before `send_raw_transaction`. Prevents wasting gas on reverts.
