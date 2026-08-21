#!/usr/bin/env bash
set -euo pipefail

# Prove a four-batch ZiSK fixture session (runbook step 6) on a GPU box.
#
# Inputs (env):
#   CARGO_ZISK   — GPU cargo-zisk binary
#   GUEST_ELF    — inner state-transition guest ELF (recorded-hash verified)
#   AGG_ELF      — aggregator guest ELF (recorded-hash verified)
#   ZISK_PK      — STARK proving key directory
#   ZISK_SK      — PLONK proving key directory
#   TOOLS_DIR    — holds aggregator_input and inspect_proof host binaries
#   SESSION_DIR  — holds batch-{1..4}.bin inputs; all outputs land here
#   EXPECTED_GUEST_PROGRAM_VK / EXPECTED_AGG_PROGRAM_VK / EXPECTED_ROOT_C
#                — the pinned wire-form VKs the session must reproduce
#
# The prove invocations mirror the prover daemon's own argument vector
# (prover/src/prover.rs prove_args): -y -o <file>, -g for GPU, --plonk -w
# only for the BN254 wraps. Every derived VK is checked against the pins, so
# a wrong ELF, key, or toolchain fails here rather than producing fixtures
# that mismatch on L1.

: "${CARGO_ZISK:?}" "${GUEST_ELF:?}" "${AGG_ELF:?}" "${ZISK_PK:?}" "${ZISK_SK:?}"
: "${TOOLS_DIR:?}" "${SESSION_DIR:?}"
: "${EXPECTED_GUEST_PROGRAM_VK:?}" "${EXPECTED_AGG_PROGRAM_VK:?}" "${EXPECTED_ROOT_C:?}"

# The ASM emulator needs a memlock hard limit the runner does not grant
# (mmap(rom) fails with EAGAIN), so every prove uses the standard emulator —
# the same choice the prover daemon makes with asm_emulator=false. Slower,
# runs anywhere.

vk_of_setup_dir() {
    # program-setup writes <blake3>_..._.verkey.bin: 32 bytes, four LE u64
    # limbs. Canonical form: each limb big-endian, concatenated.
    python3 - "$1" <<'EOF'
import glob, struct, sys
files = glob.glob(sys.argv[1] + "/*.verkey.bin")
assert len(files) == 1, f"expected one verkey.bin, found {files}"
limbs = struct.unpack("<4Q", open(files[0], "rb").read())
print("0x" + b"".join(l.to_bytes(8, "big") for l in limbs).hex())
EOF
}

calibrate() {
    local name="$1" elf="$2" expected="$3"
    local setup_dir="${SESSION_DIR}/setup-${name}"
    mkdir -p "${setup_dir}"
    "${CARGO_ZISK}" program-setup -e "${elf}" -k "${ZISK_PK}" -g -o "${setup_dir}"
    local derived
    derived="$(vk_of_setup_dir "${setup_dir}")"
    echo "${name} programVK: ${derived}"
    if [[ "${derived}" != "${expected}" ]]; then
        echo "ERROR: ${name} programVK ${derived} != pinned ${expected}." >&2
        echo "Wrong ELF, key, or toolchain — nothing from this session can be trusted." >&2
        exit 1
    fi
}

echo "==> calibration: both programVKs must reproduce the pins"
calibrate guest "${GUEST_ELF}" "${EXPECTED_GUEST_PROGRAM_VK}"
calibrate aggregator "${AGG_ELF}" "${EXPECTED_AGG_PROGRAM_VK}"

echo "==> proving the four per-batch vadcop_final streams"
for n in 1 2 3 4; do
    "${CARGO_ZISK}" prove -e "${GUEST_ELF}" -i "${SESSION_DIR}/batch-${n}.bin" \
        -k "${ZISK_PK}" -y -o "${SESSION_DIR}/vadcop-batch-${n}.bin" -g --emulator
done

echo "==> PLONK-wrapping batch 1 (the BATCH fixture)"
"${CARGO_ZISK}" prove -e "${GUEST_ELF}" -i "${SESSION_DIR}/batch-1.bin" \
    -k "${ZISK_PK}" -w "${ZISK_SK}" --plonk -y -o "${SESSION_DIR}/batch1-plonk.bin" -g --emulator

echo "==> aggregating the range"
"${TOOLS_DIR}/aggregator_input" -o "${SESSION_DIR}/agg-input.bin" \
    "${SESSION_DIR}"/vadcop-batch-{1,2,3,4}.bin 2> "${SESSION_DIR}/aggregator-input.txt"
cat "${SESSION_DIR}/aggregator-input.txt"

"${CARGO_ZISK}" prove -e "${AGG_ELF}" -i "${SESSION_DIR}/agg-input.bin" \
    -k "${ZISK_PK}" -w "${ZISK_SK}" --plonk -y -o "${SESSION_DIR}/aggregated-plonk.bin" -g --emulator

echo "==> extracting the wire fixtures"
"${TOOLS_DIR}/inspect_proof" "${SESSION_DIR}/batch1-plonk.bin" | tee "${SESSION_DIR}/batch1-inspect.txt"
"${TOOLS_DIR}/inspect_proof" "${SESSION_DIR}/aggregated-plonk.bin" | tee "${SESSION_DIR}/aggregated-inspect.txt"

field() { grep -m1 "^$2" "$1" | sed 's/.*= //'; }

echo "==> self-checks against the pins"
check() {
    local what="$1" got="$2" want="$3"
    if [[ "${got}" != "${want}" ]]; then
        echo "ERROR: ${what}: ${got} != pinned ${want}" >&2
        exit 1
    fi
    echo "OK: ${what} = ${got}"
}
check "batch fixture programVK" "$(field "${SESSION_DIR}/batch1-inspect.txt" program_vk)" "${EXPECTED_GUEST_PROGRAM_VK}"
check "batch fixture vadcopVK" "$(field "${SESSION_DIR}/batch1-inspect.txt" vadcop_vk)" "${EXPECTED_ROOT_C}"
check "aggregated fixture programVK" "$(field "${SESSION_DIR}/aggregated-inspect.txt" program_vk)" "${EXPECTED_AGG_PROGRAM_VK}"
check "aggregated fixture vadcopVK" "$(field "${SESSION_DIR}/aggregated-inspect.txt" vadcop_vk)" "${EXPECTED_ROOT_C}"
check "aggregator saw the inner programVK" \
    "$(grep -m1 'inner programVK' "${SESSION_DIR}/aggregator-input.txt" | awk '{print $NF}')" \
    "${EXPECTED_GUEST_PROGRAM_VK}"

echo "==> writing SUMMARY.md"
{
    echo "# ZiSK fixture session"
    echo
    echo "Guest programVK \`${EXPECTED_GUEST_PROGRAM_VK}\`, aggregator programVK"
    echo "\`${EXPECTED_AGG_PROGRAM_VK}\`, rootCVadcopFinal \`${EXPECTED_ROOT_C}\` —"
    echo "all reproduced by this session's calibration and self-checks."
    echo
    echo "## Per-batch commitments (batch order)"
    echo
    echo '```'
    grep 'commitment' "${SESSION_DIR}/aggregator-input.txt"
    echo '```'
    echo
    echo "## Binding digest (aggregated publics[32..64])"
    echo
    echo '```'
    grep 'commitment' "${SESSION_DIR}/aggregated-inspect.txt"
    echo '```'
    echo
    echo "## era-contracts ZiskVerifierRealProofTest constants"
    echo
    echo "### BATCH_PROOF / BATCH_PUBLIC_VALUES"
    echo '```'
    grep -E '^PROOF|^PUBLIC_VALUES' "${SESSION_DIR}/batch1-inspect.txt"
    echo '```'
    echo "### AGGREGATED_PROOF / AGGREGATED_PUBLIC_VALUES"
    echo '```'
    grep -E '^PROOF|^PUBLIC_VALUES' "${SESSION_DIR}/aggregated-inspect.txt"
    echo '```'
    echo
    echo "Update together (BINDING_VECTOR.md lockstep rule): the two era-contracts"
    echo "test files, guest-aggregator/BINDING_VECTOR.md, the pins in"
    echo "guest-aggregator/src/lib.rs and prover/tests/real_aggregation_vector.rs,"
    echo "and prover/tests/data (the committed batch-1 vadcop stream)."
} > "${SESSION_DIR}/SUMMARY.md"
echo "session complete: ${SESSION_DIR}/SUMMARY.md"
