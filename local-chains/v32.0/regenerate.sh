#!/usr/bin/env bash
#
# Regenerate local-chains/v32.0/l1-state.json.gz.
#
# The chain in the original snapshot was deployed before era-contracts#2323, so its
# Executor/Committer compute the pre-v32 batch proof public input and its dual verifier has
# no verifier registered at version 8. Neither can verify a V8 proof. This script applies
# those upgrades on top of the existing snapshot and dumps the result.
#
# Usage:
#   ERA_CONTRACTS=/path/to/era-contracts VERIFIER_SRC=/path/to/ZKsyncOSVerifierPlonk.sol \
#     ./local-chains/v32.0/regenerate.sh
#
# The verifier source is generated from the SNARK VK with era-contracts'
# `tools/verifier-gen` (`--variant custom --plonk_input_path snark_vk.json`), where
# snark_vk.json comes from `zkos-wrapper generate-vk --bin multiblock_batch.bin \
# --text multiblock_batch.text --check-aux-params --trusted-setup setup_compact.key`.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ERA_CONTRACTS="${ERA_CONTRACTS:?set ERA_CONTRACTS to an era-contracts checkout}"
VERIFIER_SRC="${VERIFIER_SRC:?set VERIFIER_SRC to the generated ZKsyncOSVerifierPlonk.sol}"
L1_CHAIN_ID="${L1_CHAIN_ID:-31337}"
RPC="http://localhost:8545"

# well-known anvil account #0; owns the dual verifier in this fixture
PK=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
DIAMOND=0xD36307AD1049A678AdFa86EDd1DE0E3D14Ad8713
DUAL_VERIFIER=0x08802EF5A0893f9c013039e490deDeD1D8a50224
CTM=0xd569F8bAb8892C71d54d480b52C0DDF14C989C5E
FFLONK=0xCC233b7A2423d0aBeED187Cc3D8B622112C1B24e

SRC="$HERE/l1-state.json"
DUMP="$(mktemp -d)/dump.json"
gzip -dfk "$HERE/l1-state.json.gz"

# Auto-mining (no --block-time) keeps the dump small. --max-persisted-states is required or
# anvil keeps only recent historical states.
anvil --load-state "$SRC" --port 8545 --dump-state "$DUMP" \
  --max-persisted-states 100000 --transaction-block-keeper 100000 &
ANVIL=$!
trap 'kill $ANVIL 2>/dev/null || true' EXIT
until cast block-number --rpc-url "$RPC" >/dev/null 2>&1; do sleep 1; done

# 1. V8 PLONK verifier, registered at dual-verifier version 8
VERIFIER=$(forge create "$VERIFIER_SRC:VerifierPlonk" \
  --private-key $PK --rpc-url "$RPC" --broadcast \
  | grep -oE 'Deployed to: 0x[0-9a-fA-F]{40}' | awk '{print $3}')
cast send "$DUAL_VERIFIER" 'addVerifier(uint32,address,address)' 8 "$FFLONK" "$VERIFIER" \
  --private-key $PK --rpc-url "$RPC" >/dev/null
echo "verifier@v8 $VERIFIER vk=$(cast call "$DUAL_VERIFIER" 'verificationKeyHash(uint256)(bytes32)' 2050 --rpc-url "$RPC")"

# 2. Executor + Committer facets carrying #2323 and the full-hash fold
pushd "$ERA_CONTRACTS/l1-contracts" >/dev/null
EXECUTOR=$(forge create contracts/state-transition/chain-deps/facets/Executor.sol:ExecutorFacet \
  --private-key $PK --rpc-url "$RPC" --broadcast --constructor-args "$L1_CHAIN_ID" \
  | grep -oE 'Deployed to: 0x[0-9a-fA-F]{40}' | awk '{print $3}')
COMMITTER=$(forge create contracts/state-transition/chain-deps/facets/Committer.sol:CommitterFacet \
  --private-key $PK --rpc-url "$RPC" --broadcast --constructor-args "$L1_CHAIN_ID" \
  | grep -oE 'Deployed to: 0x[0-9a-fA-F]{40}' | awk '{print $3}')
cast rpc anvil_setBalance "$CTM" 0xDE0B6B3A7640000 --rpc-url "$RPC" >/dev/null
cast rpc anvil_impersonateAccount "$CTM" --rpc-url "$RPC" >/dev/null
# executeUpgrade is onlyChainTypeManager; Action.Replace == 1
cast send "$DIAMOND" 'executeUpgrade(((address,uint8,bool,bytes4[])[],address,bytes))' \
  "([($EXECUTOR,1,true,[0xa085344d,0x7ca4eff7,0x9271e450]),($COMMITTER,1,true,[0x0b6db820,0x0db9eb87])],0x0000000000000000000000000000000000000000,0x)" \
  --from "$CTM" --unlocked --rpc-url "$RPC" >/dev/null
cast rpc anvil_stopImpersonatingAccount "$CTM" --rpc-url "$RPC" >/dev/null

# 3. dual verifier code, keeping its verifier mappings
DV_CODE=$(python3 -c "import json;print(json.load(open('out/ZKsyncOSDualVerifier.sol/ZKsyncOSDualVerifier.json'))['deployedBytecode']['object'])")
cast rpc anvil_setCode "$DUAL_VERIFIER" "$DV_CODE" --rpc-url "$RPC" >/dev/null
popd >/dev/null

echo "committed batches: $(cast call "$DIAMOND" 'getTotalBatchesCommitted()(uint256)' --rpc-url "$RPC")"
kill -TERM $ANVIL; trap - EXIT; sleep 5

# anvil does not re-dump historical states that arrived via --load-state, and the server's
# genesis lookup binary-searches historical eth_calls. Graft the original history back on.
python3 - "$SRC" "$DUMP" "$HERE/l1-state.json" <<'PY'
import json, sys
orig, dump, out = sys.argv[1], sys.argv[2], sys.argv[3]
o = json.load(open(orig)); n = json.load(open(dump))
n['historical_states'] = o['historical_states']
json.dump(n, open(out, 'w'), separators=(',', ':'))
print(f"accounts={len(n['accounts'])} blocks={len(n['blocks'])} historical_states={len(n['historical_states'])}")
PY

gzip -f "$HERE/l1-state.json"
echo "wrote $HERE/l1-state.json.gz"
