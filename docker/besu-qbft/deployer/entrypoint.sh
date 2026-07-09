#!/usr/bin/env bash
# One-shot ecosystem deployment against the Besu QBFT L1.
#
# Runs in besu-node-1's network namespace, so the L1 is http://localhost:8545 —
# required both for reachability and because zk-deployer only auto-funds
# operator/bundle-signer wallets on a localhost RPC URL.
#
# Idempotent: exits 0 immediately when the recorded deployment is still live on
# this L1; wipes /deployment and redeploys when the L1 is fresh (e.g. after
# `docker compose down && up`).
set -euo pipefail

: "${DEPLOYER_PK:?DEPLOYER_PK must be set (a genesis-funded L1 account)}"
RPC="${L1_RPC_URL:-http://localhost:8545}"

rpc() {
    curl -sf -X POST -H 'Content-Type: application/json' --data "$1" "$RPC"
}

echo "==> Waiting for the Besu QBFT chain at ${RPC} to produce blocks..."
head_hex=""
for i in $(seq 1 120); do
    head_hex=$(rpc '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' | jq -r '.result // empty' 2>/dev/null) || head_hex=""
    if [[ -n "${head_hex}" && $((head_hex)) -ge 1 ]]; then
        break
    fi
    if [[ ${i} -eq 120 ]]; then
        echo "!! L1 produced no blocks within 240s (QBFT needs 3 of 4 validators up)" >&2
        exit 1
    fi
    sleep 2
done
echo "==> L1 is live (head block $((head_hex)))."

cd /deployment

if [[ -f server.yaml ]]; then
    bridgehub=$(grep -E 'bridgehub_address:' server.yaml | grep -oE '0x[0-9a-fA-F]{40}' || true)
    code="0x"
    if [[ -n "${bridgehub}" ]]; then
        code=$(rpc "{\"jsonrpc\":\"2.0\",\"method\":\"eth_getCode\",\"params\":[\"${bridgehub}\",\"latest\"],\"id\":1}" | jq -r '.result // "0x"' 2>/dev/null) || code="0x"
    fi
    if [[ "${code}" != "0x" && "${code}" != "null" && -n "${code}" ]]; then
        echo "==> Existing deployment is live (bridgehub ${bridgehub} has code). Nothing to do."
        exit 0
    fi
    echo "==> Stale artifacts from a previous L1 detected — wiping /deployment."
    find /deployment -mindepth 1 -delete
fi

cp /opt/intent.yaml intent.yaml

echo "==> zk-deployer bootstrap"
zk-deployer bootstrap --broadcast --private-key "${DEPLOYER_PK}" --intent intent.yaml

echo "==> zk-deployer apply"
zk-deployer apply --broadcast --private-key "${DEPLOYER_PK}"

echo "==> zk-deployer server-config"
zk-deployer server-config

echo "==> Deployment complete. server.yaml (operator keys redacted):"
grep -v '_sk:' server.yaml
