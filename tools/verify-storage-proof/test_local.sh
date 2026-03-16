#!/usr/bin/env bash
#
# End-to-end local test for verify-storage-proof:
#   1. Starts Anvil (L1) with pre-loaded state
#   2. Starts zksync-os-server (L2)
#   3. Deploys a Counter contract and increments it
#   4. Waits for batch commitment on L1
#   5. Runs the verify-storage-proof CLI tool
#
# Usage: ./test_local.sh [--skip-build]

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
CONFIG_DIR="$REPO_ROOT/local-chains/v30.2/default"

# Server uses relative paths (./local-chains, ./db), so we must run from repo root
cd "$REPO_ROOT"

# Foundry tools (installed via foundryup but may not be on PATH)
FOUNDRY_BIN="$HOME/.foundry/bin"
ANVIL="${FOUNDRY_BIN}/anvil"
CAST="${FOUNDRY_BIN}/cast"
FORGE="${FOUNDRY_BIN}/forge"

# Ports
L1_PORT=8545
L2_PORT=3050

L1_RPC="http://localhost:${L1_PORT}"
L2_RPC="http://localhost:${L2_PORT}"

# Rich wallet on the local L2 (pre-funded in genesis)
RICH_PK="0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110"
RICH_ADDR="0x36615cf349d7f6344891b1e7ca7c72883f5dc049"

# Bridgehub address from config.yaml
BRIDGEHUB="0xd8f8df05efacd52f28cdf11be22ce3d6ae0fabf7"

# Counter contract (slot 0 = counter value)
STORAGE_KEY="0x0000000000000000000000000000000000000000000000000000000000000000"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
ok()    { echo -e "${GREEN}[OK]${NC}    $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
err()   { echo -e "${RED}[ERR]${NC}   $*"; }

# --- Cleanup ---

declare -a PIDS=()
TEMP_DIR=""

cleanup() {
    trap - SIGINT SIGTERM EXIT
    echo ""
    info "Shutting down..."
    for pid in "${PIDS[@]}"; do
        kill -TERM "$pid" 2>/dev/null || true
    done
    sleep 2
    for pid in "${PIDS[@]}"; do
        kill -9 "$pid" 2>/dev/null || true
    done
    ok "Cleaned up (logs at $TEMP_DIR)"
}
trap cleanup SIGINT SIGTERM EXIT

# --- Helpers ---

wait_for_rpc() {
    local url="$1" name="$2" max="${3:-60}"
    info "Waiting for ${name} at ${url}..."
    for i in $(seq 1 "$max"); do
        if curl -sf "$url" -X POST -H "Content-Type: application/json" \
            --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' > /dev/null 2>&1; then
            ok "${name} is ready"
            return 0
        fi
        sleep 1
    done
    err "${name} did not start within ${max}s"
    return 1
}

rpc_call() {
    local url="$1" method="$2" params="$3"
    local response
    response=$(curl -sf "$url" -X POST -H "Content-Type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"method\":\"${method}\",\"params\":${params},\"id\":1}") || return 1
    local rpc_error
    rpc_error=$(echo "$response" | jq -r '.error.message // empty')
    if [ -n "$rpc_error" ]; then
        echo "RPC_ERROR:$rpc_error"
        return 0
    fi
    echo "$response" | jq -r '.result'
}

# --- Parse args ---

SKIP_BUILD=false
for arg in "$@"; do
    case "$arg" in
        --skip-build) SKIP_BUILD=true ;;
        *) err "Unknown argument: $arg"; exit 1 ;;
    esac
done

# --- Pre-flight checks ---

for tool in "$ANVIL" "$CAST" jq curl; do
    if ! command -v "$tool" &>/dev/null; then
        err "Required tool not found: $tool"
        exit 1
    fi
done

# --- Kill stale processes on our ports ---

for port in "$L1_PORT" "$L2_PORT" 3312 3124 3071; do
    pid=$(lsof -ti :"$port" 2>/dev/null || true)
    if [ -n "$pid" ]; then
        warn "Killing stale process on port $port (pid $pid)"
        kill -9 $pid 2>/dev/null || true
        sleep 0.5
    fi
done

# --- Build ---

if [ "$SKIP_BUILD" = false ]; then
    info "Building zksync-os-server and verify-storage-proof..."
    cargo build --release --manifest-path "$REPO_ROOT/Cargo.toml" \
        -p zksync_os_server -p zksync_os_verify_storage_proof
    ok "Build complete"
else
    warn "Skipping build (--skip-build)"
fi

SERVER_BIN="$REPO_ROOT/target/release/zksync-os-server"
VERIFY_BIN="$REPO_ROOT/target/release/zksync_os_verify_storage_proof"

if [ ! -x "$SERVER_BIN" ] || [ ! -x "$VERIFY_BIN" ]; then
    err "Binaries not found. Run without --skip-build first."
    exit 1
fi

# --- Decompress L1 state ---

TEMP_DIR=$(mktemp -d)
info "Decompressing L1 state..."
gzip -d < "$CONFIG_DIR/../l1-state.json.gz" > "$TEMP_DIR/l1-state.json"
ok "L1 state ready"

# --- Clean DB ---
# Each run starts Anvil from the pre-loaded L1 snapshot, so the DB must be cleaned
# to avoid state commitment mismatches with stale batch data.

if [ -d "$REPO_ROOT/db" ] && [ "$(ls -A "$REPO_ROOT/db" 2>/dev/null)" ]; then
    info "Cleaning existing db/ directory"
    rm -rf "$REPO_ROOT/db"/*
fi

# --- Start Anvil (L1) ---

info "Starting Anvil (L1) on port ${L1_PORT}..."
"$ANVIL" --load-state "$TEMP_DIR/l1-state.json" --port "$L1_PORT" > "$TEMP_DIR/anvil.log" 2>&1 &
PIDS+=($!)
wait_for_rpc "$L1_RPC" "Anvil"

# --- Start L2 server ---

info "Starting zksync-os-server (L2) on port ${L2_PORT}..."
"$SERVER_BIN" --config "$CONFIG_DIR/config.yaml" > "$TEMP_DIR/server.log" 2>&1 &
PIDS+=($!)
wait_for_rpc "$L2_RPC" "L2 server" 120

# --- Deploy Counter contract ---

info "Deploying Counter contract..."

# Counter bytecode from the compiled artifact
COUNTER_BYTECODE=$(jq -r '.bytecode.object' \
    "$REPO_ROOT/integration-tests/test-contracts/out/Counter.sol/Counter.json")

DEPLOY_TX=$("$CAST" send --rpc-url "$L2_RPC" \
    --private-key "$RICH_PK" \
    --create "$COUNTER_BYTECODE" \
    --json 2>&1)
DEPLOY_TX_HASH=$(echo "$DEPLOY_TX" | jq -r '.transactionHash')
ok "Deploy tx: $DEPLOY_TX_HASH"

# Get contract address from receipt
RECEIPT=$("$CAST" receipt --rpc-url "$L2_RPC" "$DEPLOY_TX_HASH" --json)
CONTRACT_ADDRESS=$(echo "$RECEIPT" | jq -r '.contractAddress')
ok "Counter deployed at: $CONTRACT_ADDRESS"

# --- Increment counter ---

info "Calling increment(42) on Counter..."
# increment(uint256) selector = 0x7cf5dab0
INC_TX=$("$CAST" send --rpc-url "$L2_RPC" \
    --private-key "$RICH_PK" \
    "$CONTRACT_ADDRESS" \
    "increment(uint256)" 42 \
    --json 2>&1)
INC_TX_HASH=$(echo "$INC_TX" | jq -r '.transactionHash')
ok "Increment tx: $INC_TX_HASH"

# Verify the storage value
STORED_VALUE=$("$CAST" storage --rpc-url "$L2_RPC" "$CONTRACT_ADDRESS" 0)
ok "Storage slot 0 = $STORED_VALUE"

# --- Wait for a batch where the slot is populated ---

info "Waiting for a committed batch that contains the storage write (this may take a few minutes)..."

BATCH_NUMBER=1
MAX_BATCH=50
POLL_START=$(date +%s)
while true; do
    ELAPSED=$(( $(date +%s) - POLL_START ))
    if [ "$ELAPSED" -gt 600 ]; then
        err "Timed out after 10 minutes waiting for batch with storage value"
        echo ""
        warn "=== Last 50 lines of server log ==="
        tail -50 "$TEMP_DIR/server.log"
        exit 1
    fi

    if [ "$BATCH_NUMBER" -gt "$MAX_BATCH" ]; then
        err "Scanned up to batch ${MAX_BATCH} without finding the slot"
        exit 1
    fi

    PROOF_RESULT=$(rpc_call "$L2_RPC" "zks_getProof" \
        "[\"${CONTRACT_ADDRESS}\", [\"${STORAGE_KEY}\"], ${BATCH_NUMBER}]")

    if [[ "$PROOF_RESULT" == RPC_ERROR:* ]]; then
        # RPC returned an error — log it and try next batch
        RPC_MSG="${PROOF_RESULT#RPC_ERROR:}"
        warn "Batch ${BATCH_NUMBER}: RPC error — ${RPC_MSG:0:120}"
        BATCH_NUMBER=$((BATCH_NUMBER + 1))
        sleep 1
        continue
    fi

    if [ "$PROOF_RESULT" = "null" ] || [ -z "$PROOF_RESULT" ]; then
        # Batch not committed yet — wait and retry same batch
        info "Batch ${BATCH_NUMBER}: not committed yet, waiting... (${ELAPSED}s)"
        sleep 2
        continue
    fi

    # Proof available — check if slot exists
    PROOF_TYPE=$(echo "$PROOF_RESULT" | jq -r '.storageProofs[0].proof.type')
    if [ "$PROOF_TYPE" = "existing" ]; then
        PROOF_VALUE=$(echo "$PROOF_RESULT" | jq -r '.storageProofs[0].proof.value')
        ok "Slot exists in batch ${BATCH_NUMBER} with value ${PROOF_VALUE} (after ${ELAPSED}s)"
        break
    fi

    info "Batch ${BATCH_NUMBER}: committed but slot non-existing, trying next..."
    BATCH_NUMBER=$((BATCH_NUMBER + 1))
done

# --- Pretty-print the raw proof ---

echo ""
info "=== Raw storage proof (zks_getProof) for batch ${BATCH_NUMBER} ==="
rpc_call "$L2_RPC" "zks_getProof" \
    "[\"${CONTRACT_ADDRESS}\", [\"${STORAGE_KEY}\"], ${BATCH_NUMBER}]" | jq .

# --- Run verify-storage-proof CLI ---

echo ""
info "=== Running verify-storage-proof CLI ==="
"$VERIFY_BIN" \
    --l2-rpc "$L2_RPC" \
    --l1-rpc "$L1_RPC" \
    --bridgehub "$BRIDGEHUB" \
    --address "$CONTRACT_ADDRESS" \
    --keys "$STORAGE_KEY" \
    --batch-number "$BATCH_NUMBER"

echo ""
ok "All done! Storage proof verified successfully."
