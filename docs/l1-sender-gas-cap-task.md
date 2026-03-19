# Task: Fix L1 Sender Gas Price Cap Handling

## Problem

When the L1 network gas price exceeds the configured cap (`max_fee_per_gas_wei`), the node currently:

1. Logs a warning and submits the transaction with the capped fee anyway (`lib/l1_sender/src/lib.rs:289–312`)
2. The transaction sits in the mempool unaccepted by validators
3. After 300 seconds, the receipt timeout fires and the node crashes
4. On restart, the old transaction may still be in the mempool — if it gets mined between restart and the new send, the L1 contract reverts (batch already committed), crashing again

The same issue applies to `max_fee_per_blob_gas_wei` for blob transactions (`lib.rs:147–153`).

## Desired Behaviour

Instead of submitting a below-market transaction, the node should **wait before submitting** until the network gas price drops to or below the configured cap. The upstream channel backs up in the meantime. No crash, no mempool pollution, no nonce/restart complications.

A metric or log should fire as soon as the node enters the waiting state so operators know L1 finality is being delayed.

## Relevant Code

All changes are in `lib/l1_sender/src/lib.rs`.

### Where gas fields are set (`tx_request_with_gas_fields`, line 275)

```rust
async fn tx_request_with_gas_fields(
    provider: &dyn Provider,
    operator_address: Address,
    max_fee_per_gas: u128,
    max_priority_fee_per_gas: u128,
) -> anyhow::Result<TransactionRequest> {
    let eip1559_est = provider.estimate_eip1559_fees().await?;
    // ...
    let capped_max_fee_per_gas = if eip1559_est.max_fee_per_gas > max_fee_per_gas {
        tracing::warn!(...);
        max_fee_per_gas   // <-- submits with cap, should wait here instead
    } else {
        eip1559_est.max_fee_per_gas
    };
    // same pattern for max_priority_fee_per_gas
}
```

### Where blob fee is set (line 142, inside main loop)

```rust
let fee_per_blob_gas = provider.get_blob_base_fee().await?;
let max_fee_per_blob_gas = config.max_fee_per_blob_gas_wei;
if fee_per_blob_gas > max_fee_per_blob_gas {
    tracing::warn!(...);  // <-- submits with cap, should wait here instead
}
tx_request.set_max_fee_per_blob_gas(max_fee_per_blob_gas);
```

### Transaction timeout constant (line 38)

```rust
const TRANSACTION_TIMEOUT: Duration = Duration::from_secs(300);
```

## What the Fix Should Do

In `tx_request_with_gas_fields`, instead of immediately falling back to the cap when `eip1559_est.max_fee_per_gas > max_fee_per_gas`, loop and re-poll until the estimate is within the cap. Same for blob fee before setting it on the request.

During the wait, emit a `tracing::warn!` on the first occurrence and a periodic reminder (e.g. every 60 seconds) so operators are alerted. Use `tokio::time::sleep` between polls — the existing `config.poll_interval` is a reasonable interval to reuse.

## Config

`lib/l1_sender/src/config.rs` — relevant fields:

- `max_fee_per_gas_wei: u128`
- `max_priority_fee_per_gas_wei: u128`
- `max_fee_per_blob_gas_wei: u128`
- `poll_interval: Duration` — already used for L1 polling, reuse for the wait loop

## Constraints

- Do not add new dependencies
- Do not change the public API or config struct fields
- The fix should be entirely within `lib/l1_sender/src/lib.rs` (and `config.rs` if needed)
- Follow the repo's Rust style: use `.context()` on all `?` propagations, `tracing::` macros for logging
