#!/usr/bin/env bash
#
# Prepare a freshly-started anvil L1 fork so a local zksync-os-server can act as a
# drop-in replica of a live chain:
#   - sync the fork timestamp to wall clock
#   - authorize the three anvil dev accounts as the commit/prove/execute operators
#   - fund those accounts
#
# The live ValidatorTimelock (post-V29) authorizes operators in TWO independent layers,
# both required — this script handles both:
#   - isValidator(chain, account)          gates commit
#   - hasRole(chain, role, account)        per-chain COMMITTER/PROVER/EXECUTOR_ROLE, gates
#                                          prove and execute
# Every storage slot is derived at runtime by tracing the contract's own SLOAD, so this
# works for any chain / timelock without hardcoded slot constants.
#
# The chain diamond and timelock are resolved from the bridgehub against the fork itself,
# so you only need the bridgehub address and chain id (both from your genesis/common config).
#
# Usage (start anvil first, then):
#   CHAIN_ID=<id> BRIDGEHUB=<addr> ./setup-anvil.sh
#   # optional overrides: ANVIL_URL, ZK_CHAIN, VTL
set -euo pipefail

ANVIL_URL="${ANVIL_URL:-http://localhost:8545}"
: "${CHAIN_ID:?set CHAIN_ID (from genesis.chain_id in your common config)}"
: "${BRIDGEHUB:?set BRIDGEHUB (from genesis.bridgehub_address in your common config)}"

# anvil dev accounts 0/1/2 — the standard, publicly-known foundry test accounts.
COMMIT_ADDR=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
PROVE_ADDR=0x70997970C51812dc3A010C7d01b50e0d17dc79C8
EXECUTE_ADDR=0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC

ONE=0x0000000000000000000000000000000000000000000000000000000000000001

# Resolve chain diamond and timelock from the bridgehub against the fork (falls back to
# any caller-provided overrides).
ZK_CHAIN="${ZK_CHAIN:-$(cast call --rpc-url "$ANVIL_URL" "$BRIDGEHUB" 'getZKChain(uint256)(address)' "$CHAIN_ID")}"
if [ -z "${VTL:-}" ]; then
  CTM=$(cast call --rpc-url "$ANVIL_URL" "$BRIDGEHUB" 'chainTypeManager(uint256)(address)' "$CHAIN_ID")
  # Pre-V29 chains expose validatorTimelock(); V29+ expose validatorTimelockPostV29().
  VTL=$(cast call --rpc-url "$ANVIL_URL" "$CTM" 'validatorTimelockPostV29()(address)' 2>/dev/null || true)
  if [ -z "$VTL" ] || [ "$VTL" = "0x0000000000000000000000000000000000000000" ]; then
    VTL=$(cast call --rpc-url "$ANVIL_URL" "$CTM" 'validatorTimelock()(address)')
  fi
fi
echo "chain=$ZK_CHAIN timelock=$VTL"

# Trace the last SLOAD performed by a view call and return its slot (0x-prefixed).
trace_last_sload() {
  local to=$1 data=$2
  curl -s -X POST "$ANVIL_URL" -H "Content-Type: application/json" \
    -d '{"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{"to":"'"$to"'","data":"'"$data"'"},"latest",{"disableMemory":true}]}' \
    | python3 -c 'import json,sys; l=json.load(sys.stdin)["result"]["structLogs"]; s=[x for x in l if x["op"]=="SLOAD"]; assert s, "no SLOAD traced"; v=s[-1]["stack"][-1]; print(v if v.startswith("0x") else "0x"+v)'
}

set_slot() { cast rpc anvil_setStorageAt "$VTL" "$1" "$ONE" --rpc-url "$ANVIL_URL" >/dev/null; }

echo "== syncing fork timestamp to wall clock"
cast rpc evm_setNextBlockTimestamp "$(date +%s)" --rpc-url "$ANVIL_URL" >/dev/null
cast rpc evm_mine --rpc-url "$ANVIL_URL" >/dev/null

echo "== whitelisting validators (isValidator)"
for addr in "$COMMIT_ADDR" "$PROVE_ADDR" "$EXECUTE_ADDR"; do
  set_slot "$(trace_last_sload "$VTL" "$(cast calldata 'isValidator(address,address)' "$ZK_CHAIN" "$addr")")"
done

echo "== granting per-chain roles (hasRole)"
COMMITTER_ROLE=$(cast keccak COMMITTER_ROLE)
PROVER_ROLE=$(cast keccak PROVER_ROLE)
EXECUTOR_ROLE=$(cast keccak EXECUTOR_ROLE)
grant_role() {
  set_slot "$(trace_last_sload "$VTL" "$(cast calldata 'hasRole(address,bytes32,address)' "$ZK_CHAIN" "$1" "$2")")"
}
grant_role "$COMMITTER_ROLE" "$COMMIT_ADDR"
grant_role "$PROVER_ROLE" "$PROVE_ADDR"
grant_role "$EXECUTOR_ROLE" "$EXECUTE_ADDR"

echo "== funding operators"
for addr in "$COMMIT_ADDR" "$PROVE_ADDR" "$EXECUTE_ADDR"; do
  cast rpc anvil_setBalance "$addr" 0x56BC75E2D630FFFFF --rpc-url "$ANVIL_URL" >/dev/null
done

echo "== verifying (all must be true)"
fail=0
check() { local label=$1 got=$2; printf '  %-24s %s\n' "$label" "$got"; [ "$got" = true ] || fail=1; }
check "isValidator commit"  "$(cast call --rpc-url "$ANVIL_URL" "$VTL" 'isValidator(address,address)(bool)' "$ZK_CHAIN" "$COMMIT_ADDR")"
check "isValidator prove"    "$(cast call --rpc-url "$ANVIL_URL" "$VTL" 'isValidator(address,address)(bool)' "$ZK_CHAIN" "$PROVE_ADDR")"
check "isValidator execute"  "$(cast call --rpc-url "$ANVIL_URL" "$VTL" 'isValidator(address,address)(bool)' "$ZK_CHAIN" "$EXECUTE_ADDR")"
check "hasRole COMMITTER"    "$(cast call --rpc-url "$ANVIL_URL" "$VTL" 'hasRole(address,bytes32,address)(bool)' "$ZK_CHAIN" "$COMMITTER_ROLE" "$COMMIT_ADDR")"
check "hasRole PROVER"       "$(cast call --rpc-url "$ANVIL_URL" "$VTL" 'hasRole(address,bytes32,address)(bool)' "$ZK_CHAIN" "$PROVER_ROLE" "$PROVE_ADDR")"
check "hasRole EXECUTOR"     "$(cast call --rpc-url "$ANVIL_URL" "$VTL" 'hasRole(address,bytes32,address)(bool)' "$ZK_CHAIN" "$EXECUTOR_ROLE" "$EXECUTE_ADDR")"

if [ "$fail" -ne 0 ]; then echo "FAILED: a check returned non-true — see above" >&2; exit 1; fi
echo "== anvil ready"
