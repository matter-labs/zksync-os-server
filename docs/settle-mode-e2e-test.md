# Permissionless Mode E2E Test Guide

This document describes how to test the `settleBatchesSharedBridge` (permissionless mode) feature end-to-end using a local Anvil-based L1 chain.

## Overview

In permissionless mode, the server uses `settleBatchesSharedBridge` on the `PermissionlessValidator` contract to atomically commit+prove+execute batches in a single L1 transaction, replacing the normal 3-transaction flow.

**Pipeline in permissionless mode:**
- Commit L1Sender: passes through without sending to L1
- Prove L1Sender: passes through without sending, stores SNARK proof in batch envelope
- Execute L1Sender: sends `settleBatchesSharedBridge` (combines commit+prove+execute in one call)

## Prerequisites

- Rust toolchain
- `anvil` (from Foundry)
- `cast` (from Foundry)

## Step-by-step Instructions

### Phase 1: Normal Mode (Build Initial L1 State)

1. **Build the server:**
   ```bash
   cargo build --release
   ```

2. **Decompress L1 state and start Anvil:**
   ```bash
   TEMP_DIR=$(mktemp -d)
   gzip -d < local-chains/v31.0/l1-state.json.gz > "$TEMP_DIR/l1-state.json"
   anvil --load-state "$TEMP_DIR/l1-state.json" --port 8545 > /tmp/anvil-test.log 2>&1 &
   ```

3. **Wait for Anvil to be ready:**
   ```bash
   curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
     --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}'
   ```

4. **Clean the db directory (if needed):**
   ```bash
   rm -rf db/*
   ```

5. **Start the server in normal mode:**
   ```bash
   cargo run --release -- --config ./local-chains/v31.0/default/config.yaml > /tmp/server-phase1.log 2>&1 &
   ```

6. **Wait for the server to produce and settle initial batches.** Monitor with:
   ```bash
   # Check L1 batch counts
   cast call <DIAMOND_PROXY> "getTotalBatchesCommitted()(uint256)" --rpc-url http://localhost:8545
   cast call <DIAMOND_PROXY> "getTotalBatchesExecuted()(uint256)" --rpc-url http://localhost:8545
   ```

   The diamond proxy address can be found in `local-chains/v31.0/default/contracts.yaml` under `diamond_proxy`.

7. **Stop the server** once you see batches committed/proved/executed (e.g., 50+ batches):
   ```bash
   kill <SERVER_PID>
   ```

### Phase 2: Add PermissionlessValidator as a Validator

The `PermissionlessValidator` contract is already deployed (its address is in the L1 state). We need to add it to the diamond proxy's validators mapping so it can call commit/prove/execute.

> **Why `anvil_setStorageAt` instead of the real activation flow?**
>
> In production, priority mode is activated through a multi-step L1 process:
> `makePermanentRollup()` → `permanentlyAllowPriorityMode()` → wait 4 days → `activatePriorityMode()`.
> We skip this in the E2E test for several reasons:
> 1. The diamond proxy admin key isn't in our test wallets (would need `anvil_impersonateAccount`)
> 2. The flow has complex prerequisites (valid DA validator pair, a priority tx with timestamp)
> 3. `activatePriorityMode()` reverts all non-executed batches, complicating test setup ordering
> 4. We're testing the *server's* permissionless mode behavior, not the contract activation logic
>
> Direct storage manipulation is deterministic, fast, and isolates what we're actually testing.

8. **Find the PermissionlessValidator address** from `contracts.yaml`:
   ```bash
   # For v31.0, it's at:
   # 0x6e225a274BC2EB94f66086a1163d43D7B1Ae52D8
   ```

9. **Compute the storage slot for the validators mapping:**

   The `validators` mapping is at storage slot 9 in the ZKChain diamond proxy. To set a validator:
   ```bash
   # Compute the storage slot: keccak256(abi.encode(validator_address, 9))
   SLOT=$(cast keccak $(cast abi-encode "f(address,uint256)" \
     0x6e225a274BC2EB94f66086a1163d43D7B1Ae52D8 9))
   echo "Validator storage slot: $SLOT"
   ```

10. **Set the PermissionlessValidator as a validator via `anvil_setStorageAt`:**
    ```bash
    curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
      --data "{
        \"jsonrpc\":\"2.0\",
        \"method\":\"anvil_setStorageAt\",
        \"params\":[
          \"<DIAMOND_PROXY>\",
          \"$SLOT\",
          \"0x0000000000000000000000000000000000000000000000000000000000000001\"
        ],
        \"id\":1
      }"
    ```

11. **Verify the storage was set correctly:**
    ```bash
    cast storage <DIAMOND_PROXY> $SLOT --rpc-url http://localhost:8545
    # Should return: 0x0000000000000000000000000000000000000000000000000000000000000001
    ```

### Phase 3: Run in Permissionless Mode

12. **Restart the server with permissionless mode enabled:**
    ```bash
    l1_sender_permissionless_mode=true \
    l1_sender_permissionless_contract_address=0x6e225a274BC2EB94f66086a1163d43D7B1Ae52D8 \
    cargo run --release -- --config ./local-chains/v31.0/default/config.yaml > /tmp/server-phase2.log 2>&1 &
    ```

13. **Monitor the logs for permissionless mode activity:**
    ```bash
    # Look for permissionless passthrough messages
    grep -i "permissionless" /tmp/server-phase2.log

    # Should see:
    # permissionless passthrough (skipping L1 send) command_name="commit"
    # permissionless passthrough (skipping L1 send) command_name="prove"

    # Look for batch completion
    grep "Batch has been fully processed" /tmp/server-phase2.log
    ```

14. **Verify L1 batch counts continue to increase:**
    ```bash
    cast call <DIAMOND_PROXY> "getTotalBatchesCommitted()(uint256)" --rpc-url http://localhost:8545
    cast call <DIAMOND_PROXY> "getTotalBatchesVerified()(uint256)" --rpc-url http://localhost:8545
    cast call <DIAMOND_PROXY> "getTotalBatchesExecuted()(uint256)" --rpc-url http://localhost:8545
    ```

    All three counts should be equal (since settle atomically does commit+prove+execute).

15. **Check for errors:**
    ```bash
    grep -i "error\|WARN\|panic" /tmp/server-phase2.log | grep -v "Prometheus\|config\|Loaded"
    ```

### Phase 4: Cleanup

16. **Stop the server:**
    ```bash
    kill <SERVER_PID>
    ```

17. **Stop Anvil:**
    ```bash
    kill <ANVIL_PID>
    ```

18. **Clean up temp files:**
    ```bash
    rm -rf "$TEMP_DIR"
    rm -rf db/*
    ```

## Key Addresses (v31.0)

| Contract | Address |
|----------|---------|
| Diamond Proxy | `0x1882a9ae62597d8f37e4f091135a8450e553c49a` |
| PermissionlessValidator | `0x6e225a274BC2EB94f66086a1163d43D7B1Ae52D8` |
| ValidatorTimelock | `0x89466662ab79c875b1e637eb0fd169d11937eac8` |
| Bridgehub | `0x69c3388f45f7f944300141ff5735d59f15e4c9f0` |
| ChainTypeManager | `0x50a0505ce8e746711c9fe9ce7ab4b6d0cfff9104` |

## Configuration

Permissionless mode is controlled by two environment variables:

| Variable | Description | Default |
|----------|-------------|---------|
| `l1_sender_permissionless_mode` | Enable permissionless mode | `false` |
| `l1_sender_permissionless_contract_address` | PermissionlessValidator contract address | `0x0` |

These can also be set in `config.yaml` under the `l1_sender` section:
```yaml
l1_sender:
  permissionless_mode: true
  permissionless_contract_address: '0x6e225a274BC2EB94f66086a1163d43D7B1Ae52D8'
```

## Storage Layout Notes

The ZKChain diamond proxy uses the following storage layout (relevant slots):

- **Slot 9**: `validators` mapping (`address => bool`)
- **Slot 36**: `admin` address
- **Slot 41**: `bridgehub` address
- **Slot 63**: `priorityModeInfo` (packed struct with `canBeActivated`, `activated`, `permissionlessValidator`)

Note: Solidity packing may cause actual slot numbers to differ from source code comments. Verify empirically when needed.

## Test Results

In our test run:
- Phase 1: 67 batches committed/proved/executed in normal mode
- Phase 2: PermissionlessValidator added as validator via `anvil_setStorageAt`
- Phase 3: 12 additional batches (68-79) settled via `settleBatchesSharedBridge` (permissionless mode) with zero errors
- All three L1 counts (committed/proved/executed) stayed in sync, confirming atomic settlement
