#!/usr/bin/env bash
#
# Redoes the chain-side setup that a restart destroys: registration, funding, token deployment.
#
# Chains only — this deliberately does not touch Prividium. Its databases live in a docker volume
# and survive `down` without `-v`, so peers, signing keys and linked wallets persist across restarts
# and are none of this script's business. Bring the stack up yourself with
# `docker compose -f docker-compose-dual.yaml up -d`.
#
# Everything chain-side, by contrast, is ephemeral: both L2s run on temp dirs and the L1 snapshot is
# loaded but never dumped, so all of the below must be redone on every boot.
#
# Start the chains first, in their own terminal:
#   RUST_MIN_STACK=67108864 ./run_local.sh ./local-chains/v32.0/multi_chain --logs-dir ./logs
#
# then run this from this directory.

set -euo pipefail

L1_RPC=${L1_RPC:-http://127.0.0.1:8545}
A_RPC=${A_RPC:-http://127.0.0.1:3050}
B_RPC=${B_RPC:-http://127.0.0.1:3051}
A_CHAIN=${A_CHAIN:-6565}
B_CHAIN=${B_CHAIN:-6566}

BRIDGEHUB=0x2a692b9f11f54858994ba8b5feac804aab380198
ASSET_ROUTER=0x0000000000000000000000000000000000010002
# anvil #0 — L1 operator for registerChain
L1_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
# preset rich account, funded on both L2s
RICH_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
# Seeded crypto-native users: Alice is the seed's "Test User", NOT the admin wallet — anvil #0 is
# already the interop and faucet operator, and sharing it makes user and operator activity
# indistinguishable on-chain.
ALICE=${ALICE:-0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC}
BOB=${BOB:-0x70997970C51812dc3A010C7d01b50e0d17dc79C8}
# Each instance's worker pays gas for executeAtomicBundle out of INTEROP_OPERATOR_PRIVATE_KEY
# (docker-compose-dual.yaml). It must be funded on the chain that instance drives — on BOTH, since
# each side executes its own incoming leg. An unfunded operator does not fail the flow loudly: that
# instance just retries forever while the other side completes, so one party pays and never gets paid.
OPERATOR=${OPERATOR:-0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266}

step() { printf '\n\033[1;34m==> %s\033[0m\n' "$1"; }
ok() { printf '    \033[0;32m%s\033[0m\n' "$1"; }

wait_for() { # wait_for <description> <command...>
    local what=$1; shift
    for _ in $(seq 1 120); do
        if "$@" >/dev/null 2>&1; then ok "$what ready"; return 0; fi
        sleep 2
    done
    echo "timed out waiting for $what" >&2; exit 1
}

step "Waiting for chains"
wait_for "L1 $L1_RPC" cast chain-id --rpc-url "$L1_RPC"
wait_for "chain $A_CHAIN" cast chain-id --rpc-url "$A_RPC"
wait_for "chain $B_CHAIN" cast chain-id --rpc-url "$B_RPC"

step "Registering the chain pair on L1"
# registerChain(Y, X) registers Y *on* X. Permissionless, but only once per ordered pair — re-running
# after a restart is fine because the L1 snapshot is reloaded fresh, but skip if already effective.
CRS=$(cast call "$BRIDGEHUB" 'chainRegistrationSender()(address)' --rpc-url "$L1_RPC")
ok "chainRegistrationSender $CRS"

registered() { # registered <rpc> <peer chain id>
    local id
    id=$(cast call "$ASSET_ROUTER" 'baseTokenAssetId(uint256)(bytes32)' "$2" --rpc-url "$1" 2>/dev/null || echo 0x0)
    [ "$id" != "0x0000000000000000000000000000000000000000000000000000000000000000" ] && [ "$id" != "0x0" ]
}

if registered "$A_RPC" "$B_CHAIN" && registered "$B_RPC" "$A_CHAIN"; then
    ok "already registered in both directions"
else
    cast send --private-key "$L1_KEY" "$CRS" "registerChain(uint256,uint256)" "$B_CHAIN" "$A_CHAIN" \
        --rpc-url "$L1_RPC" >/dev/null
    cast send --private-key "$L1_KEY" "$CRS" "registerChain(uint256,uint256)" "$A_CHAIN" "$B_CHAIN" \
        --rpc-url "$L1_RPC" >/dev/null
    ok "both registerChain calls sent"
    # The effect is asynchronous: it lands on L2 only once the watcher imports the L1 event.
    wait_for "$B_CHAIN known on $A_CHAIN" registered "$A_RPC" "$B_CHAIN"
    wait_for "$A_CHAIN known on $B_CHAIN" registered "$B_RPC" "$A_CHAIN"
fi

step "Funding wallets"
fund() { # fund <rpc> <address> <label>
    local bal
    bal=$(cast balance "$2" --rpc-url "$1")
    # Compare by digit count, not arithmetic: 10 ETH is 1e19 wei, past bash's 64-bit range.
    # Fewer than 19 digits means below 1 ETH, which is plenty of headroom for gas here.
    if [ "${#bal}" -lt 19 ]; then
        cast send --private-key "$RICH_KEY" --rpc-url "$1" "$2" --value 10ether >/dev/null
        ok "$3 funded with 10 ETH"
    else
        ok "$3 already funded ($(cast from-wei "$bal") ETH)"
    fi
}
fund "$A_RPC" "$ALICE" "Alice on $A_CHAIN"
fund "$B_RPC" "$BOB" "Bob on $B_CHAIN"
fund "$A_RPC" "$OPERATOR" "interop operator on $A_CHAIN"
fund "$B_RPC" "$OPERATOR" "interop operator on $B_CHAIN"

step "Deploying tokens"
DEPLOY_OUT=$(npx ts-node deploy-tokens.ts)
echo "$DEPLOY_OUT" | grep -E '^TOKEN_[AB]:' || true
TOKEN_A=$(echo "$DEPLOY_OUT" | grep -oE 'TOKEN_A=0x[0-9a-fA-F]{40}' | head -1 | cut -d= -f2)
TOKEN_B=$(echo "$DEPLOY_OUT" | grep -oE 'TOKEN_B=0x[0-9a-fA-F]{40}' | head -1 | cut -d= -f2)
[ -n "$TOKEN_A" ] && [ -n "$TOKEN_B" ] || { echo "could not parse token addresses" >&2; exit 1; }

step "Ready"
cat <<EOF
Chain side is set up. Still needed, outside this script:

  - Prividium up:  docker compose -f docker-compose-dual.yaml up -d
  - Wallets linked on their own instance — Alice on localhost:3000, Bob on
    localhost:3300. Persisted in the docker volume, so only needed once.

Run the swap:

  TOKEN_A=$TOKEN_A TOKEN_B=$TOKEN_B npx ts-node atomic-swap-prividium.ts

EOF
