#!/usr/bin/env bash
# Stable lifecycle harness: run a command block against a throwaway Anvil and
# GUARANTEE that Anvil — and anything the block spawned (deployer, node) — is
# torn down on every exit path. No deployer/verifier specifics live here; those
# are authored per-run from the current zk-deployer README and passed as the
# COMMAND after `--`, so schema drift never requires editing this file.
set -Eeuo pipefail

usage() {
  cat <<'EOF'
anvil-session.sh --workdir DIR [anvil flags...] -- COMMAND [ARGS...]

Starts Anvil (127.0.0.1, --silent) with the given flags, waits for RPC
readiness, then runs COMMAND with $L1_RPC and $WORKDIR exported. On any exit it
SIGINTs Anvil (so --dump-state flushes) and kills COMMAND's process group.

  --workdir DIR   scratch directory; anvil.log is written here (required)
  anvil flags     forwarded verbatim to `anvil` (e.g. --port, --dump-state)
  -- COMMAND      the command block to run while Anvil is up (required)
EOF
}

WORKDIR=""
ANVIL_FLAGS=()
while (($#)); do
  case "$1" in
    --workdir)
      WORKDIR=${2:?missing value for --workdir}
      shift 2
      ;;
    --)
      shift
      break
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      ANVIL_FLAGS+=("$1")
      shift
      ;;
  esac
done

(($#)) || { usage >&2; echo "error: no COMMAND given after --" >&2; exit 2; }
[[ -n "$WORKDIR" ]] || { echo "error: --workdir is required" >&2; exit 2; }
[[ -d "$WORKDIR" ]] || { echo "error: --workdir does not exist: $WORKDIR" >&2; exit 2; }

# Derive the port from the forwarded flags for the readiness probe.
PORT=8545
for ((i = 0; i < ${#ANVIL_FLAGS[@]}; i++)); do
  [[ ${ANVIL_FLAGS[i]} == --port ]] && PORT=${ANVIL_FLAGS[i + 1]:-8545}
done

ANVIL_PID=""
CMD_PID=""
cleanup() {
  local status=$?
  # Kill the command's whole process group (negative PID) so grandchildren
  # such as the node started inside a verify block are reaped too.
  [[ -n "$CMD_PID" ]] && kill -- -"$CMD_PID" 2>/dev/null || true
  if [[ -n "$ANVIL_PID" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
    kill -INT "$ANVIL_PID" 2>/dev/null || true # SIGINT so --dump-state flushes
    wait "$ANVIL_PID" 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT INT TERM

# Job control: each backgrounded job becomes its own process group leader, which
# is what makes `kill -- -PID` above reap the command's descendants.
set -m

anvil --host 127.0.0.1 --silent "${ANVIL_FLAGS[@]}" > "$WORKDIR/anvil.log" 2>&1 &
ANVIL_PID=$!

for _ in $(seq 1 100); do
  if curl --silent --fail \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "http://127.0.0.1:$PORT" >/dev/null 2>&1; then
    break
  fi
  sleep 0.1
done
kill -0 "$ANVIL_PID" 2>/dev/null || {
  echo "error: Anvil failed to start; see $WORKDIR/anvil.log" >&2
  exit 1
}

L1_RPC="http://127.0.0.1:$PORT" WORKDIR="$WORKDIR" "$@" &
CMD_PID=$!
wait "$CMD_PID"
