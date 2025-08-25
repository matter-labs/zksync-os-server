#!/usr/bin/env bash
set -euo pipefail

# This script checks that CURRENT_REPLAY_VERSION is incremented if ReplayRecord changes.
# It compares the TypeHash of ReplayRecord and the CURRENT_REPLAY_VERSION between the PR tip and the latest release.

# Find the merge base between this branch and main
MERGE_BASE=$(git merge-base HEAD origin/main)
echo "Using merge base: $MERGE_BASE"

# Fully check out the merge base tree into a temp directory
OLD_TREE=/tmp/zksync-os-server-merge-base
rm -rf "$OLD_TREE"
git clone . "$OLD_TREE" --shared --quiet
cd "$OLD_TREE"
git checkout "$MERGE_BASE" --quiet
cd - > /dev/null

# Extract CURRENT_REPLAY_VERSION from both versions using Rust example
OLD_TREE=/tmp/zksync-os-server-merge-base
pushd "$OLD_TREE/lib/storage_api" > /dev/null
OLD_VERSION=$(cargo run --quiet --example print_replayrecord_version 2>/dev/null || echo "error")
popd > /dev/null

pushd lib/storage_api > /dev/null
NEW_VERSION=$(cargo run --quiet --example print_replayrecord_version 2>/dev/null || echo "error")
popd > /dev/null

echo "Old version: $OLD_VERSION, New version: $NEW_VERSION"


# Compute hash for merge base (old)
OLD_TREE=/tmp/zksync-os-server-merge-base
pushd "$OLD_TREE/lib/storage_api" > /dev/null
OLD_HASH=$(cargo run --quiet --example print_replayrecord_typehash 2>/dev/null || echo "error")
popd > /dev/null

# Compute hash for current branch (new)
pushd lib/storage_api > /dev/null
NEW_HASH=$(cargo run --quiet --example print_replayrecord_typehash 2>/dev/null || echo "error")
popd > /dev/null

echo "Old hash: $OLD_HASH, New hash: $NEW_HASH"

if [[ "$OLD_HASH" != "$NEW_HASH" && "$OLD_VERSION" == "$NEW_VERSION" ]]; then
    echo "ReplayRecord changed but CURRENT_REPLAY_VERSION was not incremented!"
    exit 1
fi

echo "ReplayRecord versioning check passed."
