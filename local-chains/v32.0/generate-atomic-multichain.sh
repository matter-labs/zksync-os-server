#!/usr/bin/env bash
#
# Regenerate the atomic multi-chain v31.0 artifact set (chains 6565 + 6566, both
# L1-settling) for the run_local.sh `multi_chain` atomic preset.
#
# Produces, all from ONE consistent atomic deploy:
#   local-chains/v32.0/genesis.json            (atomic genesis, root 0x1a1bcd…)
#   local-chains/v32.0/l1-state.json.gz        (anvil dump WITH historical states, gzipped)
#   local-chains/v32.0/wallets.yaml
#   local-chains/v32.0/multi_chain/chain_6565.yaml
#   local-chains/v32.0/multi_chain/chain_6566.yaml
#
# WHY a full deploy (not a genesis swap): run_local.sh's anvil loads l1-state.json.gz and
# the servers validate their on-chain chain registration against the atomic genesis root at
# boot (the genesis-upgrade tx binary-search via historical eth_getCode). The L1 state, the
# atomic genesis, and the per-chain bridgehub/bytecode_supplier addresses + operators must
# therefore all come from the same deploy. Swapping only the genesis yields a root mismatch
# (BlockOutOfRangeError at boot).
#
# REQUIREMENTS (sibling repos READ-ONLY except their build artifacts):
#   IT_ROOT   zksync-os-integration-tests checkout (branch kl/l1-settled-interop-proof) with
#             `zk-deployer` built:  (cd "$IT_ROOT" && cargo build -p zk-deployer --bin zk-deployer)
#   ERA_ROOT  era-contracts checkout (branch atomic-imt-interop); the deployer runs its
#             `build-contracts` against it (forge build).  PROTOCOL_CONTRACTS_ROOT=$ERA_ROOT.
#   anvil v1.5.x in PATH (run_local.sh also uses host anvil). The graceful --dump-state with
#   --preserve-historical-states is REQUIRED so historical eth_getCode works after reload.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"           # local-chains/v32.0
PRESET="$HERE/multi_chain"
IT_ROOT="${IT_ROOT:?set IT_ROOT to the zksync-os-integration-tests checkout}"
ERA_ROOT="${ERA_ROOT:?set ERA_ROOT to the era-contracts atomic-imt-interop checkout}"
ANVIL_PORT="${ANVIL_PORT:-28545}"
SEED="${ECOSYSTEM_SEED:-atomic-multichain}"
DEPLOYER_KEY="0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80"  # anvil #0
ZK_DEPLOYER="$IT_ROOT/target/debug/zk-deployer"; [[ -x "$ZK_DEPLOYER" ]] || ZK_DEPLOYER="$IT_ROOT/target/release/zk-deployer"
[[ -x "$ZK_DEPLOYER" ]] || { echo "build zk-deployer first: (cd $IT_ROOT && cargo build -p zk-deployer --bin zk-deployer)" >&2; exit 1; }
command -v anvil >/dev/null || { echo "anvil not in PATH" >&2; exit 1; }

WORK="$(mktemp -d)"
RAW="$WORK/l1-state.json"
cleanup() { [[ -n "${APID:-}" ]] && kill "$APID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

# 1. anvil with graceful historical-state dump (flushed on SIGTERM).
anvil --host 127.0.0.1 --port "$ANVIL_PORT" --dump-state "$RAW" --preserve-historical-states >/tmp/anvil-atomic-multichain.log 2>&1 &
APID=$!; sleep 2
L1_RPC="http://127.0.0.1:$ANVIL_PORT"

# 2. intent: two L1-settling rollup chains.
cat > "$WORK/intent.yaml" <<YAML
schema_version: 1
l1_rpc_url: $L1_RPC
wallets:
  ecosystem_seed: $SEED
chains:
  - chain_id: 6565
    da_mode: rollup
  - chain_id: 6566
    da_mode: rollup
YAML

export PROTOCOL_CONTRACTS_ROOT="$ERA_ROOT"
( cd "$ERA_ROOT" && "$ZK_DEPLOYER" build-contracts )
( cd "$ERA_ROOT" && "$ZK_DEPLOYER" bootstrap --intent "$WORK/intent.yaml" --state "$WORK/state.json" --out "$WORK/out" \
    --private-key "$DEPLOYER_KEY" --wallets-out "$WORK/wallets.yaml" --genesis-out "$WORK/genesis.json" --broadcast --l1-state "$RAW" )
( cd "$ERA_ROOT" && "$ZK_DEPLOYER" apply --intent "$WORK/intent.yaml" --state "$WORK/state.json" --wallets "$WORK/wallets.yaml" \
    --out "$WORK/out" --private-key "$DEPLOYER_KEY" --broadcast --l1-state "$RAW" )
for c in 6565 6566; do
  ( cd "$ERA_ROOT" && "$ZK_DEPLOYER" server-config --intent "$WORK/intent.yaml" --state "$WORK/state.json" \
      --wallets "$WORK/wallets.yaml" --chain "$c" --output "$WORK/server-$c.yaml" )
done

# 3. SIGTERM anvil to flush --dump-state (with historical snapshots).
kill -TERM "$APID" 2>/dev/null || true
for _ in $(seq 1 60); do kill -0 "$APID" 2>/dev/null || break; sleep 1; done
APID=""
[[ -s "$RAW" ]] || { echo "anvil did not write $RAW" >&2; exit 1; }

# 4. Lay down the preset: genesis, gzipped L1 state, wallets, per-chain run_local configs.
mkdir -p "$PRESET"
cp "$WORK/genesis.json" "$HERE/genesis.json"
cp "$WORK/wallets.yaml" "$HERE/wallets.yaml"
gzip -c "$RAW" > "$HERE/l1-state.json.gz"

python3 - "$WORK" "$PRESET" <<'PY'
import sys, re
work, preset = sys.argv[1], sys.argv[2]
def parse(path):
    d={}; sect=None
    for raw in open(path):
        line=raw.rstrip("\n")
        if not line.strip(): continue
        if not line.startswith(" "): sect=line.split(":")[0].strip(); d[sect]={}
        elif sect is not None and ":" in line.strip():
            k,v=line.strip().split(":",1); d[sect][k.strip()]=v.strip().strip('"').strip("'")
    return d
ports={6565:3050,6566:3051}
for cid,port in ports.items():
    y=parse(f"{work}/server-{cid}.yaml"); g=y["genesis"]; ls=y["l1_sender"]
    open(f"{preset}/chain_{cid}.yaml","w").write(f"""general:
  ephemeral: true
genesis:
  bridgehub_address: '{g['bridgehub_address']}'
  bytecode_supplier_address: '{g['bytecode_supplier_address']}'
  genesis_input_path: ./local-chains/v32.0/genesis.json
  chain_id: {cid}
l1_sender:
  pubdata_mode: Blobs
  operator_commit_sk: '{ls['operator_commit_sk']}'
  operator_prove_sk: '{ls['operator_prove_sk']}'
  operator_execute_sk: '{ls['operator_execute_sk']}'
rpc:
  address: 0.0.0.0:{port}
""")
    print(f"wrote chain_{cid}.yaml (bridgehub {g['bridgehub_address']}, :{port})")
PY

echo "[generate-atomic-multichain] done."
echo "  genesis_root: $(python3 -c "import json;print(json.load(open('$HERE/genesis.json'))['genesis_root'])")"
echo "  Run: ./run_local.sh ./local-chains/v32.0/multi_chain"
