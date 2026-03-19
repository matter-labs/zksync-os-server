#!/usr/bin/env bash
# Validates that protocol-versions.toml is consistent with Cargo.toml.
#
# Checks performed:
# 1. Forward check: every forward_system crate in protocol-versions.toml exists in Cargo.toml
#    with a matching git tag.
# 2. Reverse check: every forward_system crate in a "# ---- execution_version = vN ----"
#    section of Cargo.toml is referenced by the corresponding execution_version section in
#    protocol-versions.toml.
#
# Usage: ./tools/check-versions-toml.sh
# Returns 0 on success, 1 on any mismatch.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSIONS_TOML="$REPO_ROOT/protocol-versions.toml"
CARGO_TOML="$REPO_ROOT/Cargo.toml"

if [ ! -f "$VERSIONS_TOML" ]; then
    echo "ERROR: protocol-versions.toml not found at $VERSIONS_TOML"
    exit 1
fi

if [ ! -f "$CARGO_TOML" ]; then
    echo "ERROR: Cargo.toml not found at $CARGO_TOML"
    exit 1
fi

errors=0

# ===========================================================================
# Forward check: protocol-versions.toml → Cargo.toml
# ===========================================================================

echo "Forward check: protocol-versions.toml → Cargo.toml"
echo ""

# Parse protocol-versions.toml: extract all (crate, tag) pairs from
# execution_version sections (crate/tag, simulation_crate/simulation_tag).
# Sections are [execution_version."vN"].
current_section=""
declare -A toml_crate_entries
declare -A toml_tag_entries

while IFS= read -r line; do
    # Skip comments and empty lines.
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${line// /}" ]] && continue

    # Detect section headers like [execution_version."v5"].
    if [[ "$line" =~ ^\[([a-zA-Z0-9._\"\-]+)\] ]]; then
        current_section="${BASH_REMATCH[1]}"
        continue
    fi

    # Only process execution_version sections.
    if [[ ! "$current_section" =~ ^execution_version\. ]]; then
        continue
    fi

    # Extract key = "value" pairs.
    if [[ "$line" =~ ^([a-z_]+)[[:space:]]*=[[:space:]]*\"(.+)\" ]]; then
        key="${BASH_REMATCH[1]}"
        value="${BASH_REMATCH[2]}"

        case "$key" in
            crate)
                toml_crate_entries["${current_section}:forward"]="$value"
                ;;
            tag)
                toml_tag_entries["${current_section}:forward"]="$value"
                ;;
            simulation_crate)
                toml_crate_entries["${current_section}:simulation"]="$value"
                ;;
            simulation_tag)
                toml_tag_entries["${current_section}:simulation"]="$value"
                ;;
            replay_crate)
                toml_crate_entries["${current_section}:replay"]="$value"
                ;;
            replay_tag)
                toml_tag_entries["${current_section}:replay"]="$value"
                ;;
        esac
    fi
done < "$VERSIONS_TOML"

# Now validate each (crate, tag) pair against Cargo.toml.
for entry_key in "${!toml_crate_entries[@]}"; do
    crate_name="${toml_crate_entries[$entry_key]}"
    expected_tag="${toml_tag_entries[$entry_key]:-}"

    if [ -z "$expected_tag" ]; then
        echo "  WARN: $entry_key has crate '$crate_name' but no tag"
        continue
    fi

    # Find this crate in Cargo.toml and extract its tag.
    # Handle both single-line and multi-line Cargo.toml entries.
    cargo_tag=$(grep -P "^${crate_name}\s*=" "$CARGO_TOML" | head -1 | grep -oP 'tag\s*=\s*"\K[^"]+' || true)

    if [ -z "$cargo_tag" ]; then
        echo "  ERROR: [$entry_key] references crate '$crate_name' but it is not found in Cargo.toml"
        errors=$((errors + 1))
        continue
    fi

    if [ "$cargo_tag" != "$expected_tag" ]; then
        echo "  ERROR: [$entry_key] crate '$crate_name' has tag '$expected_tag' in protocol-versions.toml but '$cargo_tag' in Cargo.toml"
        errors=$((errors + 1))
    else
        echo "  OK: [$entry_key] $crate_name @ $expected_tag"
    fi
done

# ===========================================================================
# Reverse check: Cargo.toml → protocol-versions.toml
# ===========================================================================

echo ""
echo "Reverse check: Cargo.toml → protocol-versions.toml"
echo ""

# Collect all (crate, tag) pairs known in protocol-versions.toml for quick lookup.
declare -A toml_known_pairs
for entry_key in "${!toml_crate_entries[@]}"; do
    crate_name="${toml_crate_entries[$entry_key]}"
    tag="${toml_tag_entries[$entry_key]:-}"
    if [ -n "$tag" ]; then
        toml_known_pairs["${crate_name}:${tag}"]=1
    fi
done

# Parse Cargo.toml: find "# ---- execution_version = vN ----" section markers,
# then check each forward_system crate under them.
cargo_exec_version=""

while IFS= read -r line; do
    # Detect section markers: # ---- execution_version = vN ----
    if [[ "$line" =~ ^#[[:space:]]*----[[:space:]]*execution_version[[:space:]]*=[[:space:]]*(v[0-9]+)[[:space:]]*---- ]]; then
        cargo_exec_version="${BASH_REMATCH[1]}"
        continue
    fi

    # Stop tracking when we hit a non-execution_version section marker or end of deps.
    if [[ "$line" =~ ^#[[:space:]]*---- ]] && [[ ! "$line" =~ execution_version ]]; then
        cargo_exec_version=""
        continue
    fi

    # Skip if we're not in an execution_version section.
    [ -z "$cargo_exec_version" ] && continue

    # Skip comments and empty lines.
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${line// /}" ]] && continue

    # Match forward_system crate lines: name = { package = "forward_system", ... tag = "..." ... }
    if [[ "$line" =~ ^([a-z_0-9]+)[[:space:]]*= ]] && [[ "$line" =~ package[[:space:]]*=[[:space:]]*\"forward_system\" ]]; then
        cargo_crate=$(echo "$line" | grep -oP '^[a-z_0-9]+')
        cargo_tag=$(echo "$line" | grep -oP 'tag\s*=\s*"\K[^"]+' || true)
        [ -z "$cargo_tag" ] && continue

        if [ -n "${toml_known_pairs["${cargo_crate}:${cargo_tag}"]:-}" ]; then
            echo "  OK: [execution_version.\"$cargo_exec_version\"] $cargo_crate @ $cargo_tag"
        else
            echo "  ERROR: Cargo.toml has forward_system crate '$cargo_crate' @ '$cargo_tag' under execution_version $cargo_exec_version, but it is not referenced in protocol-versions.toml"
            errors=$((errors + 1))
        fi
    fi
done < "$CARGO_TOML"

# ===========================================================================
# Result
# ===========================================================================

echo ""
if [ "$errors" -gt 0 ]; then
    echo "FAILED: $errors inconsistencies found between protocol-versions.toml and Cargo.toml."
    echo "Please update protocol-versions.toml or Cargo.toml so they agree."
    exit 1
else
    echo "PASSED: protocol-versions.toml is consistent with Cargo.toml."
    exit 0
fi
