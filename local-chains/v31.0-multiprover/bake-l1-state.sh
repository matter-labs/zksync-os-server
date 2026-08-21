#!/usr/bin/env bash
#
# Bake `l1-state.json.gz` for the multiprover chain.
#
# The state is the v31.0 baked state with the multiprover verifier set added on
# top, so every v31.0 address stays valid. The chain's verifier becomes
# `MultiProofTestnetVerifier`, which keeps the empty-proof and mock-proof
# escape hatches and delegates every real proof to `MultiProofVerifier`. A real
# settlement on this chain therefore needs an Airbender proof and an aggregated
# ZiSK proof together (proof type 5).
#
# Usage:
#   ERA_CONTRACTS=/path/to/era-contracts ./bake-l1-state.sh
#
# The era-contracts checkout must carry the multiprover verifiers and the
# generated snarkJS Plonk verifier at
# `l1-contracts/contracts/dev-contracts/generated/ZiskSnarkPlonkVerifier.sol`
# (see that repository's `contracts/state-transition/verifiers/README.md`).
set -euo pipefail

FIXTURE_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASE_STATE_GZ="${FIXTURE_DIR}/../v31.0/l1-state.json.gz"
ERA_CONTRACTS="${ERA_CONTRACTS:?set ERA_CONTRACTS to an era-contracts checkout with the multiprover verifiers}"
L1_CONTRACTS="${ERA_CONTRACTS}/l1-contracts"
PORT="${PORT:-18545}"
RPC="http://127.0.0.1:${PORT}"

# Anvil development account 0. It owns the v31.0 verifier registry, so the
# multiprover set keeps one owner.
DEPLOYER_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
DEPLOYER=0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266

# v31.0 chain addresses.
DIAMOND=0xb573DfdA099F9567c8E20d924A32A75bF834A5ef
# `verifier` in the diamond's ZKChainStorage. The script asserts the slot holds
# the v31.0 verifier before it writes, and asserts `getVerifier()` afterwards.
VERIFIER_SLOT=10
# The PLONK verifier that checks Airbender proofs on the v31.0 chain.
# `MultiProofVerifier` keeps it, so Airbender verification is unchanged.
AIRBENDER_VERIFIER=0x40c2a18c576b4864b2cf3a458499137ee96057aa

# ZiSK pins baked into the generated `ZiskVerifier`.
EXPECTED_INNER_PROGRAM_VK=0x1d16f620e2bc7e58044df7ee8d4284422a0dd37cf151cf79ecf324c131e50468
EXPECTED_AGGREGATOR_PROGRAM_VK=0x4c3d7317a62f651d813ba6afbbce59e45eaa7c009ab2a9b51d2f0fb3e7987254
EXPECTED_ROOT_C_VADCOP_FINAL=0xcf2a309856f107b143836ada112806da71ae11567fa3f2d2050baba5381c7b7d
EXPECTED_ZISK_VK_HASH=0xd261b4cb68d1c58d0539e1364ba93fe65f6d009bb268a8de1ec68535f0ebe5a0

WORK_DIR="$(mktemp -d)"
ANVIL_PID=""
cleanup() {
    if [[ -n "${ANVIL_PID}" ]] && kill -0 "${ANVIL_PID}" 2>/dev/null; then
        kill "${ANVIL_PID}" 2>/dev/null || true
        wait "${ANVIL_PID}" 2>/dev/null || true
    fi
    rm -rf "${WORK_DIR}"
}
trap cleanup EXIT

expect_equal() {
    local what="$1" got="$2" want="$3"
    if [[ "${got,,}" != "${want,,}" ]]; then
        echo "${what}: got ${got}, expected ${want}" >&2
        exit 1
    fi
    echo "  ok  ${what} = ${got}"
}

deploy() {
    # `forge create` prints a "Deployed to: 0x..." line among build output.
    forge create --root "${L1_CONTRACTS}" --rpc-url "${RPC}" \
        --private-key "${DEPLOYER_KEY}" --broadcast "$@" |
        sed -n 's/^Deployed to: //p'
}

# era-contracts gitignores the generated snarkJS verifier, so a fresh checkout
# does not carry it.
ZISK_PLONK_SOURCE="${L1_CONTRACTS}/contracts/dev-contracts/generated/ZiskSnarkPlonkVerifier.sol"
if [[ ! -f "${ZISK_PLONK_SOURCE}" ]]; then
    echo "missing ${ZISK_PLONK_SOURCE}; generate it with the steps in" >&2
    echo "${L1_CONTRACTS}/contracts/state-transition/verifiers/README.md" >&2
    exit 1
fi

echo "==> building era-contracts artifacts"
forge build --root "${L1_CONTRACTS}" >/dev/null

echo "==> starting anvil on the v31.0 state"
gzip -dc "${BASE_STATE_GZ}" >"${WORK_DIR}/base-state.json"
# `--preserve-historical-states` keeps the per-block state snapshots the v31.0
# state carries, so a node can still read L1 state at a historical block.
anvil --chain-id 31337 --port "${PORT}" \
    --load-state "${WORK_DIR}/base-state.json" \
    --dump-state "${WORK_DIR}/multiprover-state.json" \
    --preserve-historical-states \
    >"${WORK_DIR}/anvil.log" 2>&1 &
ANVIL_PID=$!
for _ in $(seq 60); do
    cast chain-id --rpc-url "${RPC}" >/dev/null 2>&1 && break
    sleep 1
done
cast chain-id --rpc-url "${RPC}" >/dev/null

BASE_VERIFIER="$(cast call "${DIAMOND}" 'getVerifier()(address)' --rpc-url "${RPC}")"
echo "==> v31.0 verifier ${BASE_VERIFIER}"
expect_equal "diamond slot ${VERIFIER_SLOT}" \
    "$(cast parse-bytes32-address "$(cast storage "${DIAMOND}" "${VERIFIER_SLOT}" --rpc-url "${RPC}")")" \
    "${BASE_VERIFIER}"
expect_equal "Airbender verification key hash" \
    "$(cast call "${AIRBENDER_VERIFIER}" 'verificationKeyHash()(bytes32)' --rpc-url "${RPC}")" \
    "$(cast call "${BASE_VERIFIER}" 'verificationKeyHash()(bytes32)' --rpc-url "${RPC}")"

echo "==> deploying the multiprover verifier set"
ZISK_PLONK_VERIFIER="$(deploy \
    contracts/dev-contracts/generated/ZiskSnarkPlonkVerifier.sol:ZiskSnarkPlonkVerifier)"
ZISK_VERIFIER="$(deploy \
    contracts/state-transition/verifiers/ZiskVerifier.sol:ZiskVerifier \
    --constructor-args "${ZISK_PLONK_VERIFIER}")"
MULTI_PROOF_VERIFIER="$(deploy \
    contracts/state-transition/verifiers/MultiProofVerifier.sol:MultiProofVerifier \
    --constructor-args "${AIRBENDER_VERIFIER}" "${DEPLOYER}")"
cast send "${MULTI_PROOF_VERIFIER}" 'setZiskRangeVerifier(address)' "${ZISK_VERIFIER}" \
    --rpc-url "${RPC}" --private-key "${DEPLOYER_KEY}" >/dev/null
MULTI_PROOF_TESTNET_VERIFIER="$(deploy \
    contracts/state-transition/verifiers/MultiProofTestnetVerifier.sol:MultiProofTestnetVerifier \
    --constructor-args "${MULTI_PROOF_VERIFIER}")"

echo "==> pointing the chain at MultiProofTestnetVerifier"
cast rpc anvil_setStorageAt "${DIAMOND}" \
    "$(cast to-uint256 "${VERIFIER_SLOT}")" \
    "$(cast to-uint256 "${MULTI_PROOF_TESTNET_VERIFIER}")" \
    --rpc-url "${RPC}" >/dev/null

echo "==> checking the wiring"
expect_equal "chain verifier" \
    "$(cast call "${DIAMOND}" 'getVerifier()(address)' --rpc-url "${RPC}")" \
    "${MULTI_PROOF_TESTNET_VERIFIER}"
expect_equal "MultiProofTestnetVerifier.INNER_VERIFIER" \
    "$(cast call "${MULTI_PROOF_TESTNET_VERIFIER}" 'INNER_VERIFIER()(address)' --rpc-url "${RPC}")" \
    "${MULTI_PROOF_VERIFIER}"
expect_equal "MultiProofVerifier.airbenderVerifier" \
    "$(cast call "${MULTI_PROOF_VERIFIER}" 'airbenderVerifier()(address)' --rpc-url "${RPC}")" \
    "${AIRBENDER_VERIFIER}"
expect_equal "MultiProofVerifier.ziskRangeVerifier" \
    "$(cast call "${MULTI_PROOF_VERIFIER}" 'ziskRangeVerifier()(address)' --rpc-url "${RPC}")" \
    "${ZISK_VERIFIER}"
expect_equal "ZiskVerifier.PLONK_VERIFIER" \
    "$(cast call "${ZISK_VERIFIER}" 'PLONK_VERIFIER()(address)' --rpc-url "${RPC}")" \
    "${ZISK_PLONK_VERIFIER}"
expect_equal "ZiskVerifier.innerProgramVK" \
    "$(cast call "${ZISK_VERIFIER}" 'innerProgramVK()(bytes32)' --rpc-url "${RPC}")" \
    "${EXPECTED_INNER_PROGRAM_VK}"
expect_equal "ZiskVerifier.aggregatorProgramVK" \
    "$(cast call "${ZISK_VERIFIER}" 'aggregatorProgramVK()(bytes32)' --rpc-url "${RPC}")" \
    "${EXPECTED_AGGREGATOR_PROGRAM_VK}"
expect_equal "ZiskVerifier.rootCVadcopFinal" \
    "$(cast call "${ZISK_VERIFIER}" 'rootCVadcopFinal()(bytes32)' --rpc-url "${RPC}")" \
    "${EXPECTED_ROOT_C_VADCOP_FINAL}"
expect_equal "ZiskVerifier.verificationKeyHash" \
    "$(cast call "${ZISK_VERIFIER}" 'verificationKeyHash()(bytes32)' --rpc-url "${RPC}")" \
    "${EXPECTED_ZISK_VK_HASH}"

echo "==> dumping the state"
kill "${ANVIL_PID}"
wait "${ANVIL_PID}" 2>/dev/null || true
ANVIL_PID=""
for _ in $(seq 60); do
    [[ -s "${WORK_DIR}/multiprover-state.json" ]] && break
    sleep 1
done
# `-n` keeps the temporary file's name and mtime out of the artifact.
gzip -9 -n -c "${WORK_DIR}/multiprover-state.json" >"${FIXTURE_DIR}/l1-state.json.gz"

cat <<EOF

Baked ${FIXTURE_DIR}/l1-state.json.gz

  chain verifier (MultiProofTestnetVerifier) ${MULTI_PROOF_TESTNET_VERIFIER}
  MultiProofVerifier                         ${MULTI_PROOF_VERIFIER}
  ZiskVerifier                               ${ZISK_VERIFIER}
  ZiskSnarkPlonkVerifier                     ${ZISK_PLONK_VERIFIER}
  Airbender PLONK verifier (from v31.0)      ${AIRBENDER_VERIFIER}
EOF
