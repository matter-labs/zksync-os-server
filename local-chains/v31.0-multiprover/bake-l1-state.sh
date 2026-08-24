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

# ZiSK pins baked into the generated `ZiskVerifier`.
EXPECTED_INNER_PROGRAM_VK=0x44e3d132399c8f3a03ce9672ba0ca00c6503db918731c7ab46d6faea445236ec
EXPECTED_AGGREGATOR_PROGRAM_VK=0x4c3d7317a62f651d813ba6afbbce59e45eaa7c009ab2a9b51d2f0fb3e7987254
EXPECTED_ROOT_C_VADCOP_FINAL=0xcf2a309856f107b143836ada112806da71ae11567fa3f2d2050baba5381c7b7d
EXPECTED_ZISK_VK_HASH=0x718bdb59530514f9a62f16b2ba912de17188615d82aa31ec681be4b9cd332888

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
    # tr, not ${var,,}: the case-insensitive compare must also work on the
    # bash 3.2 that macOS ships.
    if [[ "$(tr '[:upper:]' '[:lower:]' <<<"${got}")" != "$(tr '[:upper:]' '[:lower:]' <<<"${want}")" ]]; then
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
# The Airbender inner verifier MUST be the chain's ZKsyncOSDualVerifier, not
# its bare PLONK sub-verifier: MultiProofVerifier prepends the two-word
# sub-proof envelope the dual verifier parses, and a bare verifier fed the
# envelope words rejects every proof with 'loadProof: Proof is invalid'
# (observed on the first real settle). The dual-ness check below is what
# makes that miswiring impossible to bake again.
AIRBENDER_DUAL_VERIFIER="${BASE_VERIFIER}"
PLONK_SUB_V0="$(cast call "${AIRBENDER_DUAL_VERIFIER}" 'plonkVerifiers(uint32)(address)' 0 --rpc-url "${RPC}")"
echo "==> dual verifier's PLONK sub-verifier (version 0) ${PLONK_SUB_V0}"
expect_equal "Airbender verification key hash" \
    "$(cast call "${PLONK_SUB_V0}" 'verificationKeyHash()(bytes32)' --rpc-url "${RPC}")" \
    "$(cast call "${AIRBENDER_DUAL_VERIFIER}" 'verificationKeyHash()(bytes32)' --rpc-url "${RPC}")"

echo "==> deploying the multiprover verifier set"
ZISK_PLONK_VERIFIER="$(deploy \
    contracts/dev-contracts/generated/ZiskSnarkPlonkVerifier.sol:ZiskSnarkPlonkVerifier)"
ZISK_VERIFIER="$(deploy \
    contracts/state-transition/verifiers/ZiskVerifier.sol:ZiskVerifier \
    --constructor-args "${ZISK_PLONK_VERIFIER}")"
MULTI_PROOF_VERIFIER="$(deploy \
    contracts/state-transition/verifiers/MultiProofVerifier.sol:MultiProofVerifier \
    --constructor-args "${AIRBENDER_DUAL_VERIFIER}" "${DEPLOYER}")"
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
expect_equal "MultiProofVerifier.airbenderVerifier (the dual)" \
    "$(cast call "${MULTI_PROOF_VERIFIER}" 'airbenderVerifier()(address)' --rpc-url "${RPC}")" \
    "${AIRBENDER_DUAL_VERIFIER}"
expect_equal "introspection forwards to the dual's registry" \
    "$(cast call "${MULTI_PROOF_TESTNET_VERIFIER}" 'plonkVerifiers(uint32)(address)' 0 --rpc-url "${RPC}")" \
    "${PLONK_SUB_V0}"
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
  ZKsyncOSDualVerifier (from v31.0, wrapped) ${AIRBENDER_DUAL_VERIFIER}
  its PLONK sub-verifier (version 0)         ${PLONK_SUB_V0}
EOF
