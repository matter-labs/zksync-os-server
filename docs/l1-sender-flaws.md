# L1 Sender — Design Flaws and Improvement Opportunities

Analysis of `lib/l1_sender/src/` in zksync-os-server. Ordered from most impactful to least.

---

## 1. No Stuck Transaction Recovery

**Severity: High — most likely cause of production incidents**

If gas prices spike after a tx is submitted, the tx sits in the mempool until the 300s timeout, then the sender crashes. On restart, the old tx may still be pending. The new sender fills the same nonce with similar gas, and the cycle repeats.

There is no fee escalation, no replacement tx logic, no way to unstick a transaction without operator intervention (manually sending a replacement tx or waiting for the old one to drop from the mempool).

**How era solves it:** Exponential fee escalation model — `base^time_in_mempool * base_fee`, capped at ~6 hours (1800 L1 blocks). Enforces a minimum 2x price bump on resend (EIP-1559 replacement rule). Guarantees eventual inclusion without manual intervention.

**Where in code:** `lib.rs:187-198` — sends tx with fixed fees, only option is crash after 300s timeout.

---

## 2. No In-Flight Transaction Detection on Restart

**Severity: High**

Acknowledged in code comments (`lib.rs:55`): *"Does not attempt to detect in-flight L1 transactions on startup — just crashes if they get mined."*

If the process crashes while an L1 tx is pending, on restart it doesn't check whether that tx was mined. It re-submits the same batch, which may conflict with the in-flight tx. If the old tx gets mined first, the new one reverts (nonce already used), and the sender crashes again. If the new one gets mined first, the old one reverts (wasting gas when it eventually gets included).

**How era solves it:** Reads `eth_txs_history` from the database on startup — knows exactly which txs are pending, resumes monitoring them, and only submits new txs for batches not yet covered.

**Where in code:** `lib.rs:54-56` — documented known issue.

---

## 3. Hardcoded Gas Limit of 15M

**Severity: Medium**

`with_gas_limit(15000000)` with the comment *"Default value for max_aggregated_tx_gas from zksync-era, should always be enough."* This reserves half an L1 block's capacity (30M limit). For small batches or single-batch commits, the actual gas used is far less.

While excess gas is refunded, `maxFeePerGas * gasLimit` is locked as the tx's maximum cost in the sender's account — this reduces available balance for concurrent txs and can trigger false "insufficient balance" errors when the operator wallet has enough ETH for actual usage but not for the inflated gas reservation.

**How era solves it:** Two modes — `Maximum` (fixed cap) and `Calculated` (`predicted_gas_cost * multiplier`), allowing tighter gas limits based on actual calldata size.

**Where in code:** `lib.rs:325-326`

---

## 4. Sequential Tx Submission Blocks on Earlier Failures

**Severity: Medium**

The sending loop uses `futures::stream::iter(...).then(...)` (`lib.rs:134`), which processes commands **sequentially**. If the 1st of 5 commands fails during `provider.fill()` or `send_raw_transaction()`, commands 2-5 are never attempted. The entire cycle fails and the sender crashes.

The code comment at line 205 acknowledges this: *"We could buffer the stream here to enable sending multiple batches of transactions in parallel, but this is not necessary for now."*

The issue isn't primarily performance (waiting for inclusion is parallel) — it's that a transient RPC failure on one tx kills the whole batch. Independent sending would allow partial progress.

**Where in code:** `lib.rs:134-208`

---

## 5. No Pre-Flight Simulation Before Sending

**Severity: Medium**

The sender builds calldata and sends directly to L1. If the calldata would revert (wrong state root, stale previous batch info, signature mismatch, protocol version issue), the tx gets mined and reverts on-chain, **wasting real ETH on gas**. The sender then discovers the revert via receipt validation and crashes.

An `eth_call` dry-run before `send_raw_transaction` would catch reverts for free (no gas cost) and allow the sender to halt or retry without burning funds. This is especially valuable for execute transactions, which are the most expensive.

Note: era doesn't do this either, but it's a missed opportunity for both.

**Where in code:** `lib.rs:187-189` — goes straight from `fill()` to `send_raw_transaction()`.

---

## 6. `BatchMetadata` Is a Serialization Trap

**Severity: Medium — will get worse over time**

`batcher_model.rs:23`: *"any change to this struct is breaking since we serialize it in ProofStorage."*

The struct has accumulated:
- `#[serde(default = "...")]` annotations on 5 fields to handle missing data from older formats
- `#[serde(rename = "commit_batch_info")]` for backwards compatibility
- A mix of domain data (block numbers, tx count) and L1-contract-specific types (`StoredBatchInfo`, `BatchInfo`)

The comment at line 19 acknowledges the design issue: *"instead of putting computed CommitBatchInfo/StoredBatchInfo here (L1 contract-specific classes), we may want to include lower-level fields."*

Each protocol version risks adding more `#[serde(default)]` fields. Consider versioning the serialization format explicitly (e.g., an envelope with a version discriminant) rather than relying on serde defaults to bridge all past and future formats.

**Where in code:** `batcher_model.rs:25-50`

---

## 7. Protocol Version Handling Is Fragile

**Severity: Medium**

Two places with hardcoded protocol version match arms that panic on unknown versions:

- `execute.rs:103-149` — matches `protocol_version.minor` on `29 | 30` vs `31 | 32`, panics on anything else
- `prove.rs:142-151` — matches `proving_execution_version` on `4 | 5 | 6`, panics on anything else

Each new protocol version requires touching these match arms manually. There's no versioning abstraction — encoding strategy is embedded directly in the command types.

A `CallDataEncoder` trait (or at least a central registry of version → encoding logic) would make upgrades less error-prone and would make it obvious exactly which files need to change for a protocol upgrade.

**Where in code:** `commands/execute.rs:103-149`, `commands/prove.rs:142-151`

---

## 8. Shared Types Live in the Wrong Crate

**Severity: Low — organizational, not functional**

The code itself says it:
- `batcher_model.rs:14`: *"these models are used throughout the batcher subsystem — not only l1 sender. We will move them to `types` or `batcher_types` when an analogous crate is created."*
- `batcher_metrics.rs:6`: same comment for metrics.

Having `BatchMetadata`, `SignedBatchEnvelope`, `FriProof`, and `BatchExecutionStage` in `l1_sender` creates an inverted dependency — upstream pipeline stages (batcher, prover, signing) depend on `l1_sender` for types that have nothing to do with L1 sending.

**Where in code:** `batcher_model.rs:14-15`, `batcher_metrics.rs:6-7`

---

## 9. No Testability Story

**Severity: Low — but compounds over time**

`run_l1_sender` takes concrete alloy types (`FillProvider<impl TxFiller + WalletProvider, impl Provider>`). There's no trait abstraction for L1 interaction, making it impossible to unit test the main loop without a real or fully mocked Ethereum provider.

The entire crate has **zero unit tests** for the sending logic — only a single serde deserialization test in `batcher_model.rs`.

This means every change to the sending loop, gas logic, or passthrough mechanism is tested only via integration tests hitting a real L1 (or anvil). This is slow and makes it hard to test edge cases (timeouts, partial failures, reorgs).

**How era solves it:** `AbstractL1Interface` trait with a `MockL1Interface` implementation, enabling fast unit tests for `EthTxManager` logic.

**Where in code:** `lib.rs:59-74` — concrete provider types in function signature.

---

## 10. Blob Base Fee Check Is Warn-Only

**Severity: Low**

When `fee_per_blob_gas > max_fee_per_blob_gas` (`lib.rs:152-158`), the sender warns but sends anyway using the configured cap. The tx will sit in the mempool indefinitely (miners won't include a blob tx below the blob base fee) and eventually time out → crash.

It would be better to either:
- **Block** until the blob fee drops below the configured cap, or
- **Fail fast** with a clear error explaining the blob fee mismatch

Rather than submitting a tx that is near-guaranteed to be stuck.

**Where in code:** `lib.rs:146-161`

---

## 11. Memory Leak in `register_operator`

**Severity: Negligible — bounded but unnecessary**

`lib.rs:343` — `address.to_string().leak()` deliberately leaks a `String` to get `&'static str` for the metrics label. This happens once per sender startup (3 times total), so it's bounded. But it's a code smell — the `vise` metrics library may support owned labels.

**Where in code:** `lib.rs:343`
