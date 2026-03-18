#!/usr/bin/env bash
# Validates that protocol-versions.toml is consistent with Cargo.toml.
#
# Checks performed:
# 1. Every forward_system_crate in protocol-versions.toml is declared in Cargo.toml.
# 2. The git tags in protocol-versions.toml match the tags in Cargo.toml for that crate.
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

echo "Checking protocol-versions.toml against Cargo.toml..."
echo ""

# Parse protocol-versions.toml: extract all (crate, tag) pairs from
# execution_version sections (forward_system_crate/tag, simulation_crate/tag).
current_section=""
declare -A crate_entries
declare -A tag_entries

while IFS= read -r line; do
    # Skip comments and empty lines.
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    [[ -z "${line// /}" ]] && continue

    # Detect section headers like [execution_version."V4"].
    if [[ "$line" =~ ^\[([a-zA-Z0-9._\"]+)\] ]]; then
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
            forward_system_crate)
                crate_entries["${current_section}:forward"]="$value"
                ;;
            forward_system_tag)
                tag_entries["${current_section}:forward"]="$value"
                ;;
            simulation_crate)
                crate_entries["${current_section}:simulation"]="$value"
                ;;
            simulation_tag)
                tag_entries["${current_section}:simulation"]="$value"
                ;;
        esac
    fi
done < "$VERSIONS_TOML"

# Now validate each (crate, tag) pair against Cargo.toml.
for entry_key in "${!crate_entries[@]}"; do
    crate_name="${crate_entries[$entry_key]}"
    expected_tag="${tag_entries[$entry_key]:-}"

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

echo ""
if [ "$errors" -gt 0 ]; then
    echo "FAILED: $errors inconsistencies found between protocol-versions.toml and Cargo.toml."
    echo "Please update protocol-versions.toml or Cargo.toml so they agree."
    exit 1
else
    echo "PASSED: protocol-versions.toml is consistent with Cargo.toml."
    exit 0
fi
