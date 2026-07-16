#!/usr/bin/env bash

set -Eeuo pipefail

usage() {
  cat <<'EOF'
Generate a DB-free, single-chain ZKsync OS local fixture with zk-deployer.

Usage:
  generate-local-state.sh \
    --server-root PATH \
    --contracts-rev GIT_SHA \
    --protocol-version v31[.PATCH] \
    [--chain-id 506] \
    [--zk-deployer-repo PATH] \
    [--zk-deployer-rev REV] \
    [--output-dir PATH] \
    [--anvil-port 8545] \
    [--force]

The output contains l1-state.json.gz, genesis.json, versions.yaml, and a
default/ directory with config.yaml, wallets.yaml, and README.md. It never
creates a node DB or contracts.yaml.
EOF
}

die() {
  echo "error: $*" >&2
  exit 1
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

SERVER_ROOT=""
CONTRACTS_REV=""
PROTOCOL_VERSION=""
CHAIN_ID=506
ZK_DEPLOYER_REPO=""
ZK_DEPLOYER_REV=HEAD
OUTPUT_DIR=""
ANVIL_PORT=8545
FORCE=0

while (($#)); do
  case "$1" in
    --server-root)
      SERVER_ROOT=${2:?missing value for --server-root}
      shift 2
      ;;
    --contracts-rev)
      CONTRACTS_REV=${2:?missing value for --contracts-rev}
      shift 2
      ;;
    --protocol-version)
      PROTOCOL_VERSION=${2:?missing value for --protocol-version}
      shift 2
      ;;
    --chain-id)
      CHAIN_ID=${2:?missing value for --chain-id}
      shift 2
      ;;
    --zk-deployer-repo)
      ZK_DEPLOYER_REPO=${2:?missing value for --zk-deployer-repo}
      shift 2
      ;;
    --zk-deployer-rev)
      ZK_DEPLOYER_REV=${2:?missing value for --zk-deployer-rev}
      shift 2
      ;;
    --output-dir)
      OUTPUT_DIR=${2:?missing value for --output-dir}
      shift 2
      ;;
    --anvil-port)
      ANVIL_PORT=${2:?missing value for --anvil-port}
      shift 2
      ;;
    --force)
      FORCE=1
      shift
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
[[ -n "$CONTRACTS_REV" ]] || die "--contracts-rev is required"
[[ -n "$PROTOCOL_VERSION" ]] || die "--protocol-version is required"
[[ "$CONTRACTS_REV" =~ ^[0-9a-fA-F]{7,40}$ ]] || die "--contracts-rev must be a Git commit hash"
[[ "$CHAIN_ID" =~ ^[0-9]+$ ]] || die "--chain-id must be numeric"
[[ "$ANVIL_PORT" =~ ^[0-9]+$ ]] || die "--anvil-port must be numeric"

if [[ "$PROTOCOL_VERSION" =~ ^v?([0-9]+)(\.([0-9]+))?$ ]]; then
  PROTOCOL_MINOR=${BASH_REMATCH[1]}
  PROTOCOL_PATCH=${BASH_REMATCH[3]:-0}
else
  die "--protocol-version must look like v31, v31.0, v32, etc."
fi
FIXTURE_VERSION="v${PROTOCOL_MINOR}.${PROTOCOL_PATCH}"
SEMANTIC_VERSION="0.${PROTOCOL_MINOR}.${PROTOCOL_PATCH}"

for command in anvil cargo curl git gzip jq realpath rg sed; do
  require_command "$command"
done

SERVER_ROOT=$(realpath "$SERVER_ROOT")
[[ -f "$SERVER_ROOT/Cargo.toml" ]] || die "not a Rust workspace: $SERVER_ROOT"
[[ -f "$SERVER_ROOT/local-chains/local_dev.yaml" ]] || die "local-chains/local_dev.yaml not found in $SERVER_ROOT"

if [[ -z "$ZK_DEPLOYER_REPO" ]]; then
  ZK_DEPLOYER_REPO="$(dirname "$SERVER_ROOT")/zksync-os-integration-tests"
fi
ZK_DEPLOYER_REPO=$(realpath "$ZK_DEPLOYER_REPO")
[[ -f "$ZK_DEPLOYER_REPO/bin/zk-deployer/Cargo.toml" ]] || die "zk-deployer repo not found: $ZK_DEPLOYER_REPO"

if [[ -z "$OUTPUT_DIR" ]]; then
  OUTPUT_DIR="$SERVER_ROOT/local-chains/$FIXTURE_VERSION"
fi
OUTPUT_DIR=$(realpath -m "$OUTPUT_DIR")
[[ "$OUTPUT_DIR" != "/" && "$OUTPUT_DIR" != "$SERVER_ROOT" ]] || die "refusing unsafe output directory: $OUTPUT_DIR"

if curl --silent --fail --max-time 1 \
  --header 'Content-Type: application/json' \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  "http://127.0.0.1:$ANVIL_PORT" >/dev/null 2>&1; then
  die "port $ANVIL_PORT is already serving HTTP"
fi

WORK_ROOT=$(mktemp -d /tmp/generate-zksync-local-state.XXXXXX)
DEPLOYER_WORKTREE="$WORK_ROOT/zk-deployer-src"
DEPLOYMENT_DIR="$WORK_ROOT/deployment"
STAGING_DIR="$WORK_ROOT/output"
ANVIL_PID=""
WORKTREE_ADDED=0
SUCCESS=0

cleanup() {
  local status=$?
  if [[ -n "$ANVIL_PID" ]] && kill -0 "$ANVIL_PID" 2>/dev/null; then
    kill -INT "$ANVIL_PID" 2>/dev/null || true
    wait "$ANVIL_PID" 2>/dev/null || true
  fi
  if ((WORKTREE_ADDED)); then
    git -C "$ZK_DEPLOYER_REPO" worktree remove --force "$DEPLOYER_WORKTREE" >/dev/null 2>&1 || true
  fi
  if ((SUCCESS)); then
    rm -rf "$WORK_ROOT"
  else
    echo "generation workspace preserved at $WORK_ROOT" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

if ! git -C "$ZK_DEPLOYER_REPO" rev-parse --verify "${ZK_DEPLOYER_REV}^{commit}" >/dev/null 2>&1; then
  git -C "$ZK_DEPLOYER_REPO" fetch origin "$ZK_DEPLOYER_REV"
fi
ZK_DEPLOYER_SHA=$(git -C "$ZK_DEPLOYER_REPO" rev-parse "${ZK_DEPLOYER_REV}^{commit}")
SERVER_SHA=$(git -C "$SERVER_ROOT" rev-parse HEAD)

git -C "$ZK_DEPLOYER_REPO" worktree add --detach "$DEPLOYER_WORKTREE" "$ZK_DEPLOYER_SHA" >/dev/null
WORKTREE_ADDED=1

sed -E -i \
  '/^(protocol_ops|zksync_os_genesis_gen) = .*git = "https:\/\/github.com\/matter-labs\/era-contracts",/ s/(branch|rev) = "[^"]+"/rev = "'"$CONTRACTS_REV"'"/' \
  "$DEPLOYER_WORKTREE/Cargo.toml"

[[ $(rg -c "^(protocol_ops|zksync_os_genesis_gen) = .*rev = \"$CONTRACTS_REV\"" "$DEPLOYER_WORKTREE/Cargo.toml") -eq 2 ]] \
  || die "could not pin both era-contracts dependencies in Cargo.toml"

TARGET_DIR="$ZK_DEPLOYER_REPO/target"
cargo update \
  --manifest-path "$DEPLOYER_WORKTREE/Cargo.toml" \
  -p protocol_ops \
  -p zksync-os-genesis-gen
cargo build \
  --release \
  --manifest-path "$DEPLOYER_WORKTREE/Cargo.toml" \
  --target-dir "$TARGET_DIR" \
  --package zk-deployer \
  --bin zk-deployer
ZK_DEPLOYER="$TARGET_DIR/release/zk-deployer"

CONTRACTS_SHA=$(
  cargo metadata --format-version 1 --manifest-path "$DEPLOYER_WORKTREE/Cargo.toml" \
    | jq -r '.packages[] | select(.name == "protocol_ops") | .source | capture("#(?<sha>[0-9a-f]{40})$").sha'
)
[[ "$CONTRACTS_SHA" =~ ^[0-9a-f]{40}$ ]] || die "could not resolve the full era-contracts revision"

mkdir -p "$DEPLOYMENT_DIR"
cat > "$DEPLOYMENT_DIR/intent.yaml" <<EOF
schema_version: 1
l1_rpc_url: "http://127.0.0.1:$ANVIL_PORT"

chains:
  - chain_id: $CHAIN_ID
    da_mode: rollup
EOF

(
  cd "$DEPLOYMENT_DIR"
  "$ZK_DEPLOYER" build-contracts
)

anvil \
  --host 127.0.0.1 \
  --port "$ANVIL_PORT" \
  --preserve-historical-states \
  --slots-in-an-epoch 2 \
  --dump-state "$DEPLOYMENT_DIR/l1-state.json" \
  --silent \
  > "$DEPLOYMENT_DIR/anvil.log" 2>&1 &
ANVIL_PID=$!

for _ in $(seq 1 100); do
  if curl --silent --fail \
    --header 'Content-Type: application/json' \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    "http://127.0.0.1:$ANVIL_PORT" >/dev/null; then
    break
  fi
  sleep 0.1
done
kill -0 "$ANVIL_PID" 2>/dev/null || die "Anvil failed to start; see $DEPLOYMENT_DIR/anvil.log"

(
  cd "$DEPLOYMENT_DIR"
  "$ZK_DEPLOYER" bootstrap --broadcast
  "$ZK_DEPLOYER" apply --broadcast
  "$ZK_DEPLOYER" server-config --chain "$CHAIN_ID" --output server.yaml
)

kill -INT "$ANVIL_PID"
wait "$ANVIL_PID"
ANVIL_PID=""
[[ -s "$DEPLOYMENT_DIR/l1-state.json" ]] || die "Anvil did not dump l1-state.json"

jq -e '
  (.best_block_number == (.transactions | length))
  and ((.historical_states | length) == (.transactions | length))
  and ((.blocks | length) == ((.transactions | length) + 1))
' "$DEPLOYMENT_DIR/l1-state.json" >/dev/null \
  || die "snapshot contains non-transaction blocks; interval mining may have been enabled"

mkdir -p "$STAGING_DIR/default"
gzip -9 -c "$DEPLOYMENT_DIR/l1-state.json" > "$STAGING_DIR/l1-state.json.gz"
cp "$DEPLOYMENT_DIR/genesis.json" "$STAGING_DIR/genesis.json"
cp "$DEPLOYMENT_DIR/wallets.yaml" "$STAGING_DIR/default/wallets.yaml"
sed -E \
  's#^  genesis_input_path:.*#  genesis_input_path: "./local-chains/'"$FIXTURE_VERSION"'/genesis.json"#' \
  "$DEPLOYMENT_DIR/server.yaml" > "$STAGING_DIR/default/config.yaml"

cat > "$STAGING_DIR/versions.yaml" <<EOF
general:
  zksync_os_version: "$SEMANTIC_VERSION"
  verification_key: "TBD"

era-contracts:
  sha: "$CONTRACTS_SHA"

zksync-os-integration-tests:
  sha: "$ZK_DEPLOYER_SHA"

zksync-os-server:
  sha: "$SERVER_SHA"
EOF

TX_COUNT=$(jq -r '.transactions | length' "$DEPLOYMENT_DIR/l1-state.json")
cat > "$STAGING_DIR/default/README.md" <<EOF
# Single Chain ($FIXTURE_VERSION)

Default single-chain configuration for running ZKsync OS directly against L1 for protocol version $SEMANTIC_VERSION.

## Chain

| Config | Chain ID | RPC Port |
|--------|----------|----------|
| \`config.yaml\` | $CHAIN_ID | 3050 |

The ecosystem and chain were deployed with \`zk-deployer\`. No Gateway chain,
Gateway database, or pre-generated node database is included. The L1 snapshot
contains $TX_COUNT transaction blocks and no interval-mined empty blocks.

## Quick Start

\`\`\`bash
./run_local.sh ./local-chains/$FIXTURE_VERSION/default
\`\`\`

Wallets and operator keys are in [wallets.yaml](./wallets.yaml). Node-required
contract addresses are in the \`genesis\` section of [config.yaml](./config.yaml).
Source revisions are recorded in [versions.yaml](../versions.yaml).
EOF

if [[ -e "$OUTPUT_DIR" ]]; then
  ((FORCE)) || die "$OUTPUT_DIR already exists; inspect it and rerun with --force to replace it"
  rm -rf "$OUTPUT_DIR"
fi
mkdir -p "$(dirname "$OUTPUT_DIR")"
mv "$STAGING_DIR" "$OUTPUT_DIR"

gzip -t "$OUTPUT_DIR/l1-state.json.gz"
[[ -s "$OUTPUT_DIR/default/config.yaml" ]] || die "generated config.yaml is empty"
[[ -s "$OUTPUT_DIR/default/wallets.yaml" ]] || die "generated wallets.yaml is empty"
[[ -s "$OUTPUT_DIR/versions.yaml" ]] || die "generated versions.yaml is empty"

echo "generated $FIXTURE_VERSION single-chain fixture at $OUTPUT_DIR"
echo "era-contracts: $CONTRACTS_SHA"
echo "zk-deployer:   $ZK_DEPLOYER_SHA"
echo "transactions:  $TX_COUNT"
du -h "$OUTPUT_DIR/l1-state.json.gz"

SUCCESS=1
