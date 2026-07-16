#!/usr/bin/env bash

set -Eeuo pipefail

usage() {
  cat <<'EOF'
Verify that a generated local fixture boots without a packaged node database.

Usage:
  verify-local-state.sh \
    --server-root PATH \
    --fixture-dir PATH \
    [--l1-port 18545] \
    [--rpc-port 13050]
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

SERVER_ROOT=""
FIXTURE_DIR=""
L1_PORT=18545
RPC_PORT=13050

while (($#)); do
  case "$1" in
    --server-root)
      SERVER_ROOT=${2:?missing value for --server-root}
      shift 2
      ;;
    --fixture-dir)
      FIXTURE_DIR=${2:?missing value for --fixture-dir}
      shift 2
      ;;
    --l1-port)
      L1_PORT=${2:?missing value for --l1-port}
      shift 2
      ;;
    --rpc-port)
      RPC_PORT=${2:?missing value for --rpc-port}
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      die "unknown argument: $1"
      ;;
  esac
done

[[ -n "$SERVER_ROOT" ]] || die "--server-root is required"
[[ -n "$FIXTURE_DIR" ]] || die "--fixture-dir is required"
[[ "$L1_PORT" =~ ^[0-9]+$ && "$RPC_PORT" =~ ^[0-9]+$ ]] || die "ports must be numeric"

for command in anvil cargo curl gzip realpath rg sed; do
  command -v "$command" >/dev/null 2>&1 || die "required command not found: $command"
done

SERVER_ROOT=$(realpath "$SERVER_ROOT")
FIXTURE_DIR=$(realpath "$FIXTURE_DIR")
[[ -f "$FIXTURE_DIR/l1-state.json.gz" ]] || die "l1-state.json.gz not found in $FIXTURE_DIR"
[[ -f "$FIXTURE_DIR/default/config.yaml" ]] || die "default/config.yaml not found in $FIXTURE_DIR"
[[ ! -e "$FIXTURE_DIR/default/db.tar.gz" ]] || die "fixture unexpectedly contains db.tar.gz"
[[ ! -e "$FIXTURE_DIR/default/contracts.yaml" ]] || die "fixture unexpectedly contains contracts.yaml"

WORK_ROOT=$(mktemp -d /tmp/verify-zksync-local-state.XXXXXX)
ANVIL_PID=""
SERVER_PID=""
SUCCESS=0

cleanup() {
  local status=$?
  if [[ -n "$SERVER_PID" ]] && kill -0 "$SERVER_PID" 2>/dev/null; then
    kill -INT "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  if [[ -n "$ANVIL_PID" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
    kill -INT "$ANVIL_PID" 2>/dev/null || true
    wait "$ANVIL_PID" 2>/dev/null || true
  fi
  if ((SUCCESS)); then
    rm -rf "$WORK_ROOT"
  else
    echo "verification logs preserved at $WORK_ROOT" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

gzip -d -c "$FIXTURE_DIR/l1-state.json.gz" > "$WORK_ROOT/l1-state.json"

STATUS_PORT=$((RPC_PORT + 21))
PROVER_PORT=$((RPC_PORT + 74))
METRICS_PORT=$((RPC_PORT + 262))
cat > "$WORK_ROOT/verify.yaml" <<EOF
general:
  rocks_db_path: "$WORK_ROOT/node"

genesis:
  genesis_input_path: "$FIXTURE_DIR/genesis.json"

rpc:
  address: "127.0.0.1:$RPC_PORT"

status_server:
  address: "127.0.0.1:$STATUS_PORT"

sequencer:
  block_dump_path: "$WORK_ROOT/block_dumps"

prover_input_generator:
  enable_input_generation: false

prover_api:
  enabled: false
  address: "127.0.0.1:$PROVER_PORT"
  proof_storage:
    path: "$WORK_ROOT/fri_proofs"

observability:
  prometheus:
    port: $METRICS_PORT
EOF

anvil \
  --load-state "$WORK_ROOT/l1-state.json" \
  --host 127.0.0.1 \
  --port "$L1_PORT" \
  --block-time 0.25 \
  --mixed-mining \
  --slots-in-an-epoch 10 \
  --silent \
  > "$WORK_ROOT/anvil.log" 2>&1 &
ANVIL_PID=$!

for _ in $(seq 1 100); do
  if curl --silent --fail \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "http://127.0.0.1:$L1_PORT" >/dev/null; then
    break
  fi
  sleep 0.1
done
kill -0 "$ANVIL_PID" 2>/dev/null || die "Anvil failed to start; see $WORK_ROOT/anvil.log"

SERVER_BIN="$SERVER_ROOT/target/release/zksync-os-server"
if [[ ! -x "$SERVER_BIN" ]]; then
  cargo build --release --manifest-path "$SERVER_ROOT/Cargo.toml"
fi

(
  cd "$SERVER_ROOT"
  L1_PROVIDER_RPC_URL="http://127.0.0.1:$L1_PORT" \
    "$SERVER_BIN" \
    --config "$SERVER_ROOT/local-chains/local_dev.yaml" \
    --config "$FIXTURE_DIR/default/config.yaml" \
    --config "$WORK_ROOT/verify.yaml"
) > "$WORK_ROOT/server.log" 2>&1 &
SERVER_PID=$!

L2_BLOCK=0
for _ in $(seq 1 240); do
  if ! kill -0 "$SERVER_PID" 2>/dev/null; then
    die "server exited during startup; see $WORK_ROOT/server.log"
  fi
  RESPONSE=$(curl --silent --max-time 1 \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "http://127.0.0.1:$RPC_PORT" || true)
  HEX_BLOCK=$(printf '%s' "$RESPONSE" | sed -n 's/.*"result":"0x\([0-9a-fA-F]*\)".*/\1/p')
  if [[ -n "$HEX_BLOCK" ]]; then
    L2_BLOCK=$((16#$HEX_BLOCK))
    ((L2_BLOCK >= 2)) && break
  fi
  sleep 0.25
done

((L2_BLOCK >= 2)) || die "node did not produce the upgrade and priority-transaction blocks; see $WORK_ROOT/server.log"
rg -q 'event_count.*10' "$WORK_ROOT/server.log" || die "node did not discover all 10 priority transactions"
[[ $(rg -c 'State diffs match' "$WORK_ROOT/server.log") -ge 2 ]] || die "state-diff validation did not pass for both initial blocks"

kill -INT "$SERVER_PID"
wait "$SERVER_PID"
SERVER_PID=""
kill -INT "$ANVIL_PID"
wait "$ANVIL_PID"
ANVIL_PID=""

echo "verified DB-free startup through L2 block $L2_BLOCK"
SUCCESS=1
