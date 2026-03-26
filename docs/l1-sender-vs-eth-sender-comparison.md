# L1 Sender (zksync-os-server) vs Eth Sender (zksync-era) — Design Comparison

## At a Glance

| Aspect | zksync-os-server (`l1_sender`) | zksync-era (`eth_sender`) |
|---|---|---|
| Architecture | Single generic async loop per operation | Two-stage: Aggregator + TxManager |
| State persistence | Stateless (crash-and-restart) | Database-backed (eth_txs + eth_txs_history tables) |
| Aggregation | Done upstream (GaplessCommitter, etc.) | Built-in pluggable criteria (count, time, gas) |
| Nonce management | Implicit via alloy provider auto-fill | Explicit tracking (finalized / latest / safe nonces) |
| Retry / stuck tx | None — crash on failure or timeout | Exponential fee escalation, resend on stuck |
| Finality model | 1 confirmation, optimistic | 3-tier: Pending → FastFinalized → Finalized |
| Multi-operator | Single provider per sender instance | 3 operator types (NonBlob, Blob, Gateway) |
| Blob handling | Transparent in commit command | Separate operator address with independent nonce |
| L1 interface | Direct alloy `Provider` usage | Abstract `AbstractL1Interface` trait |
| Codebase size | ~1,200 lines total | ~5,500+ lines (excluding DAL) |

---

## 1. Architectural Shape

### zksync-os-server

```
GaplessCommitter → UpgradeGatekeeper → L1Sender<Commit> → SnarkProving → GaplessL1ProofSender → L1Sender<Prove> → PriorityTree → L1Sender<Execute> → BatchSink
```

- **Linear pipeline** of `PipelineComponent`s connected by tokio mpsc channels.
- Each `L1Sender` is a standalone async loop parameterized by the `SendToL1` trait.
- Aggregation, ordering, and command construction happen in **upstream components** (GaplessCommitter, GaplessL1ProofSender, PriorityTreePipelineStep).
- The L1 sender itself only knows how to: receive commands, send L1 transactions, wait for inclusion, forward downstream.

### zksync-era

```
EthTxAggregator ──(writes to DB)──→ eth_txs table ──(reads from DB)──→ EthTxManager
```

- **Two independent polling loops** connected through a **shared database**.
- `EthTxAggregator` decides *what* to send (batch grouping, calldata encoding, readiness checks).
- `EthTxManager` decides *how* to send (signing, gas pricing, stuck detection, finality tracking).
- The database is the source of truth — both components can restart independently.

### Key Difference

zksync-os-server treats L1 sending as a **pipeline stage** with in-memory channel state. zksync-era treats it as a **durable queue** backed by Postgres. This is the single most important architectural difference — almost every other difference follows from it.

---

## 2. State and Recovery

### zksync-os-server

- **No persistent L1 transaction state.** The pipeline replays from the last compacted state on restart.
- If an L1 tx is in-flight when the process crashes, it may get mined. On restart, the sender doesn't detect this and may submit a new tx with the same nonce — this fails, and the system crashes again. Recovery requires the old tx to be either mined or dropped.
- Known issue documented in code: *"Does not attempt to detect in-flight L1 transactions on startup."*
- Passthrough mechanism handles batches already committed/proved/executed on L1 — upstream components detect this and wrap them as `Passthrough` commands.

### zksync-era

- **Full transaction lifecycle in database.** Every attempt is recorded in `eth_txs_history`.
- On restart, `EthTxManager` reads pending txs from the database and resumes monitoring.
- `eth_txs.confirmed_eth_tx_history_id` tracks which attempt was confirmed.
- Supports block reorgs via `unfinalize_txs()` — clears confirmation and re-checks.

### Tradeoff

zksync-os-server trades durability for simplicity and eliminates database dependency. The crash-and-restart model works when restarts are cheap and L1 tx conflicts are rare. zksync-era's model is more robust but requires a Postgres instance and complex DAL code (~1,500 lines in `eth_sender_dal.rs`).

---

## 3. Aggregation Logic

### zksync-os-server

- **No aggregation in the L1 sender itself.** The `command_limit` config controls how many *already-formed commands* are processed per cycle, but the grouping of batches into commands is done upstream:
  - `CommitCommand`: always 1 batch (hardcoded)
  - `ProofCommand`: batches grouped by upstream `SnarkProvingPipelineStep`
  - `ExecuteCommand`: batches grouped by upstream `PriorityTreePipelineStep`

### zksync-era

- **Pluggable aggregation criteria** via `L1BatchPublishCriterion` trait:
  - `NumberCriterion` — up to N batches per tx
  - `TimestampDeadlineCriterion` — deadline-based (e.g., 5 minutes)
  - `L1GasCriterion` — gas budget per tx
- Different criteria sets for commit, prove, and execute.
- Aggregator checks readiness conditions (proofs available, execution delay passed, system contracts updated).

### Tradeoff

zksync-os-server's approach is simpler but less flexible — changing aggregation behavior requires modifying upstream pipeline stages. zksync-era's pluggable criteria allow runtime-configurable aggregation strategies without code changes.

---

## 4. Gas Pricing and Fee Management

### zksync-os-server

- **Simple cap-based model:**
  1. Estimate EIP-1559 fees from provider
  2. Use `min(estimated, configured_cap)` for both base fee and priority fee
  3. Warn if cap is lower than estimate (may delay inclusion)
  4. For blobs: fetch blob base fee, use configured cap
- No escalation on stuck transactions — if a tx isn't included within 300s, the sender crashes.
- Hardcoded gas limit: `15_000_000`.

### zksync-era

- **Exponential escalation model:**
  1. Base fees calculated via `GasAdjuster` (separate component)
  2. `time_in_mempool` tracks how long a tx has been waiting
  3. Fees increase exponentially: `base^time_in_mempool * base_fee`
  4. Capped at `time_in_mempool_in_l1_blocks_cap` (default 1800 blocks ≈ 6h)
  5. Enforces minimum 2x price bump on resend (EIP-1559 replacement rule)
- Gas limit mode: `Maximum` (fixed cap) or `Calculated` (predicted_gas_cost * multiplier).
- Separate `EthFeesOracle` abstraction for fee calculation.

### Tradeoff

zksync-os-server's model is straightforward and works well when gas prices are stable. zksync-era's model handles sustained congestion gracefully — a stuck tx will eventually get mined as fees escalate, rather than crashing. The downside is significantly more complexity.

---

## 5. Nonce Management

### zksync-os-server

- Nonce is managed **implicitly** by the alloy provider's `fill()` method.
- Sequential sending within a cycle preserves nonce ordering automatically.
- **Critical constraint**: the same provider/address must not be used by anything else.
- No explicit nonce tracking — the provider queries the pending nonce from L1 each time.

### zksync-era

- **Explicit 3-tier nonce tracking** per operator:
  - `finalized` — nonce on the finalized block
  - `latest` — nonce on the latest block
  - `fast_finality` — nonce on the safe/finalized block
- Used to determine tx status: if `operator_nonce.latest > tx.nonce`, the tx was mined.
- Nonce assigned by `EthTxAggregator` when creating the `eth_tx` record.

### Tradeoff

Implicit nonce handling is simpler but fragile — any external nonce interference causes cascading failures. Explicit tracking enables stuck detection and recovery but adds a layer of bookkeeping.

---

## 6. Finality Model

### zksync-os-server

- **Optimistic 1-confirmation model.**
- After `send_raw_transaction`, waits for 1 confirmation with a 300s timeout.
- If a reorg happens and the tx is excluded, the sender crashes. Recovery is via restart + passthrough.
- Comment in code: *"We are being optimistic with our transaction inclusion here."*

### zksync-era

- **3-tier finality model:**
  - `Pending` — tx in mempool or unfinalized block
  - `FastFinalized` — tx in safe/finalized block (high confidence)
  - `Finalized` — tx in canonical finalized block (irreversible)
- Supports reorg handling: `unfinalize_txs()` rolls back confirmations.
- `wait_confirmations` config overrides the default finality tracking.

### Tradeoff

zksync-os-server's 1-confirmation model is acceptable when L1 reorgs affecting the tx are extremely rare (which they are on Ethereum post-merge). zksync-era's model is strictly safer but adds complexity in tracking state transitions.

---

## 7. Error Handling

### zksync-os-server

| Error | Response |
|---|---|
| TX timeout (300s) | Crash |
| TX reverted on L1 | Log debug trace, crash |
| Zero operator balance | Crash on startup |
| Protocol version mismatch | Block and poll (UpgradeGatekeeper) |

Recovery model: crash → restart → replay pipeline → passthrough already-done batches → resume from first unsent.

### zksync-era

| Error | Response |
|---|---|
| TX stuck in mempool | Resend with higher fees |
| TX reverted on L1 | Mark as failed, log failure reason, panic |
| Retriable network error | Retry (transient_errors metric) |
| Block reorg | Unfinalize, re-check confirmations |
| Circuit breaker tripped | Halt sending |

Recovery model: resume from database state, re-send stuck txs, escalate fees.

### Key Difference

zksync-os-server uses a **fail-fast** philosophy — any unexpected state causes a crash, and correctness is ensured by replay. zksync-era uses a **resilient** philosophy — the system tries to self-heal within a running process, only crashing on truly unrecoverable errors.

---

## 8. Multi-Operator Support

### zksync-os-server

- Each `L1Sender` instance has **one provider** (one signing key, one nonce stream).
- All three senders (commit, prove, execute) can use different providers, but this is configured at the pipeline level.
- No concept of operator "types" — blob and non-blob txs go through the same provider.

### zksync-era

- **3 operator types** with independent nonce management:
  - `NonBlob` — standard EIP-1559 txs
  - `Blob` — EIP-4844 txs (separate address to avoid nonce interference)
  - `Gateway` — settlement layer txs
- `EthTxManager` iterates over all supported operator types in each loop iteration.
- `from_addr` field on `EthTx` determines which operator sends it.

### Tradeoff

zksync-era's multi-operator design allows blob commits to use a dedicated address, preventing blob-specific fee dynamics from affecting proof/execute txs. zksync-os-server achieves natural isolation because each L1 sender is a separate pipeline stage with its own provider.

---

## 9. L1 Interface Abstraction

### zksync-os-server

- **Direct alloy usage.** The `run_l1_sender` function takes a generic `FillProvider<impl TxFiller, impl Provider>`.
- No abstraction layer between the sender and the Ethereum client.
- Contract calls encoded via `alloy::sol_types::SolCall` (generated from Solidity ABIs).

### zksync-era

- **`AbstractL1Interface` trait** with methods: `sign_tx`, `send_raw_tx`, `get_tx_status`, `get_operator_nonce`, `failure_reason`, etc.
- `RealL1Interface` wraps `BoundEthInterface` (web3/ethers-based).
- Enables easy mocking for tests and swapping L1 implementations.

### Tradeoff

zksync-os-server's direct approach has less indirection but makes testing harder (must mock the alloy provider). zksync-era's trait abstraction enables clean testing but adds a layer of code.

---

## 10. Observability

### zksync-os-server

- **State tracking**: 4 states per sender (WaitingRecv, SendingToL1, WaitingL1Inclusion, WaitingSend)
- **Batch lifecycle**: `BatchExecutionStage` enum with 21 stages tracked via `LatencyDistributionTracker`
- **Metrics**: gas used, fees, blob gas, balance, nonce, EIP-1559 estimates
- No health check endpoint.

### zksync-era

- **Health checks**: `ReactiveHealthCheck` per component with structured details
- **Metrics**: gas used, block numbers, inflight tx count, tx latency, aggregation reasons, pubdata usage, transient errors
- **Circuit breakers**: `FailedL1TransactionChecker` can halt the system on persistent failures.

### Tradeoff

zksync-era has richer operational tooling (health checks, circuit breakers). zksync-os-server's `LatencyDistributionTracker` provides good pipeline-level visibility but lacks health check integration.

---

## 11. Configuration

### zksync-os-server

```rust
L1SenderConfig {
    operator_signer,
    max_fee_per_gas_wei,
    max_priority_fee_per_gas_wei,
    max_fee_per_blob_gas_wei,
    command_limit,
    poll_interval,           // unused
    fusaka_upgrade_timestamp,
}
```

~7 fields. Shared across all three sender instances (with different operator signers configurable externally).

### zksync-era

```rust
SenderConfig {
    wait_confirmations, tx_poll_period, aggregate_tx_poll_period,
    max_txs_in_flight, proof_sending_mode, max_aggregated_tx_gas,
    max_aggregated_blocks_to_commit, max_aggregated_blocks_to_execute,
    aggregated_block_commit_deadline, aggregated_block_prove_deadline,
    aggregated_block_execute_deadline, timestamp_criteria_max_allowed_lag,
    max_acceptable_priority_fee_in_gwei, pubdata_sending_mode,
    tx_aggregation_paused, tx_aggregation_only_prove_and_execute,
    time_in_mempool_in_l1_blocks_cap, is_verifier_pre_fflonk,
    gas_limit_mode, max_acceptable_base_fee_in_wei,
    time_in_mempool_multiplier_cap, precommit_params,
    fusaka_upgrade_block, fusaka_upgrade_timestamp,
    settlement_fee_payer,
}
```

~25+ fields. Covers aggregation, gas, finality, feature flags, and migration modes.

---

## Summary

zksync-os-server's L1 sender is a **lean, pipeline-oriented design** optimized for simplicity and composability. It pushes complexity (aggregation, ordering, restart recovery) to upstream pipeline stages and relies on crash-and-restart for error recovery.

zksync-era's eth sender is a **full-featured, self-contained subsystem** with database-backed durability, fee escalation, multi-operator support, and 3-tier finality. It handles more edge cases but at significantly higher complexity.

The designs reflect different operational contexts: zksync-os-server assumes fast restarts and a simpler deployment model; zksync-era assumes a long-running service managing high-value L1 transactions where downtime and stuck txs are costly.
