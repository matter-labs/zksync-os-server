#!/usr/bin/env bash
set -euo pipefail

SERVER_LOGFILE=${SERVER_LOGFILE:-server.log}
TIMEOUT=${TIMEOUT:-120}
INTERVAL=${INTERVAL:-3}

chmod a+x ./zksync-os-server

# name|state|config
CONFIGS=(
  "v30 default|local-chains/v30.2/l1-state.json|local-chains/v30.2/default/config.yaml"
  "v30 multi-chain 1|local-chains/v30.2/l1-state.json|local-chains/v30.2/multi_chain/chain_6565.yaml"
  "v30 multi-chain 2|local-chains/v30.2/l1-state.json|local-chains/v30.2/multi_chain/chain_6566.yaml"
#  "v31 default|local-chains/v31.0/l1-state.json|local-chains/v31.0/default/config.yaml"
#  "v31 multi-chain 1|local-chains/v31.0/l1-state.json|local-chains/v31.0/multi_chain/chain_6565.yaml"
#  "v31 multi-chain 2|local-chains/v31.0/l1-state.json|local-chains/v31.0/multi_chain/chain_6566.yaml"
)

cleanup() {
  rm -rf ./db
  if [[ -n "${SERVER_PID:-}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi

  if [[ -n "${ANVIL_PID:-}" ]] && kill -0 "${ANVIL_PID}" 2>/dev/null; then
    kill "${ANVIL_PID}" 2>/dev/null || true
    wait "${ANVIL_PID}" 2>/dev/null || true
  fi

  SERVER_PID=""
  ANVIL_PID=""
}

on_error() {
  echo "❌ Smoke test failed"
  echo "Config name: ${CUR_NAME:-unknown}"
  echo "State:       ${CUR_STATE:-unknown}"
  echo "Config:      ${CUR_CONFIG:-unknown}"
  echo "---- ${SERVER_LOGFILE} ----"
  [[ -f "${SERVER_LOGFILE}" ]] && cat "${SERVER_LOGFILE}" || true
  cleanup
}

trap on_error ERR
trap cleanup EXIT

for entry in "${CONFIGS[@]}"; do
  cleanup

  IFS="|" read -r CUR_NAME CUR_STATE CUR_CONFIG <<< "${entry}"

  echo ""
  echo "============================================================"
  echo "▶ Running: ${CUR_NAME}"
  echo "  state:  ${CUR_STATE}"
  echo "  config: ${CUR_CONFIG}"
  echo "============================================================"

  : > "${SERVER_LOGFILE}"

  echo "Decompressing anvil state..."
  gzip -dfk "${CUR_STATE}.gz"

  echo "Starting anvil..."
  anvil --load-state "${CUR_STATE}" --port 8545 > anvil.log 2>&1 &
  ANVIL_PID=$!

  echo "Starting server..."
  ./zksync-os-server --config "local-chains/local_dev.yaml" --config "${CUR_CONFIG}" > "${SERVER_LOGFILE}" 2>&1 &
  SERVER_PID=$!

  RPC_PORT=$(yq -r '
    (
      (.rpc.address // "")
      | select(length > 0)
      // "0.0.0.0:3050"
    )
    | split(":") | .[-1]
  ' "${CUR_CONFIG}")

  echo "Waiting for server on port ${RPC_PORT}..."
  START_TIME=$(date +%s)

  while ! nc -z localhost "${RPC_PORT}"; do
    NOW=$(date +%s)
    ELAPSED=$((NOW - START_TIME))
    if [[ "${ELAPSED}" -ge "${TIMEOUT}" ]]; then
      echo "⏰ Timed out after ${TIMEOUT}s"
      cat "${SERVER_LOGFILE}"
      exit 1
    fi
    echo "Waiting... (${ELAPSED}s)"
    sleep "${INTERVAL}"
  done

  echo "✅ Server is up"

  TEST_PRIVATE_KEY=0x7726827caac94a7f9e1b160f7ea819f172f7b6f9d2a97f992c38edeab82d4110
  FROM=0x36615cf349d7f6344891b1e7ca7c72883f5dc049
  TO=0x5A67EE02274D9Ec050d412b96fE810Be4D71e7A0

  echo "Sending test transaction..."
  MAX_RETRIES=5
  RETRY_DELAY=2  # seconds
  attempt=1
  while true; do
    cast balance --rpc-url "http://localhost:${RPC_PORT}" "${FROM}"
    if cast send \
      --private-key "${TEST_PRIVATE_KEY}" \
      --rpc-url "http://localhost:${RPC_PORT}" \
      "${TO}" \
      --value 10; then
      echo "✅ Test transaction succeeded!"
      break
    fi
    if [ "${attempt}" -ge "${MAX_RETRIES}" ]; then
      echo "❌ Test transaction failed after ${MAX_RETRIES} attempts!"
      cat "${SERVER_LOGFILE}"
      exit 1
    fi
    echo "⚠️  Cast send failed (attempt ${attempt}/${MAX_RETRIES}), retrying in ${RETRY_DELAY}s..."
    attempt=$((attempt + 1))
    sleep "${RETRY_DELAY}"
  done

  echo "✅ ${CUR_NAME} passed"
done

echo ""
echo "🎉 All configs passed successfully"
