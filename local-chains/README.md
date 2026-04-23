# Local Chains

This directory contains configuration files for running ZKsync OS nodes locally.

## Directory Structure

```
local-chains/
├── README.md                    # Top-level documentation for local chain configurations
├── v30.1/                       # Protocol version v30.1
│   ├── genesis.json             # Genesis configuration
├── v30.2/                       # Protocol version v30.2
│   ├── default/                 # Default (single-chain) setup
│   │   ├── README.md            # Scenario-specific documentation
│   │   ├── config.yaml          # Sequencer configuration
│   │   ├── genesis.json         # Genesis configuration (symlink to parent genesis)
│   │   ├── wallets.yaml         # Wallets configuration (symlink to multi_chain/wallets_6565.yaml)
│   │   └── contracts.yaml       # Contracts configuration (symlink to multi_chain/contracts_6565.yaml)
│   ├── multi_chain/             # Multi-chain scenario
│   │   ├── README.md            # Scenario-specific documentation
│   │   ├── genesis.json         # Genesis configuration (symlink to parent genesis)
│   │   ├── chain_6565.yaml      # Configuration for chain with ID 6565
│   │   ├── chain_6566.yaml      # Configuration for chain with ID 6566
│   │   ├── wallets_6565.yaml    # Wallets for chain 6565
│   │   ├── wallets_6566.yaml    # Wallets for chain 6566
│   │   ├── contracts_6565.yaml  # Contracts for chain 6565
│   │   └── contracts_6566.yaml  # Contracts for chain 6566
│   ├── l1-state.json.gz         # Shared L1 state for protocol v30.2
│   ├── genesis.json             # Genesis configuration for protocol v30.2
│   └── versions.yaml            # Version metadata for protocol v30.2
└── v31.0/                       # Protocol version v31.0
    ├── default/                 # Default (single-chain) setup
    │   ├── README.md            # Scenario-specific documentation
    │   ├── config.yaml          # Sequencer configuration
    │   ├── genesis.json         # Genesis configuration (symlink to parent genesis)
    │   ├── wallets.yaml         # Wallets configuration (symlink to multi_chain/wallets_6565.yaml)
    │   └── contracts.yaml       # Contracts configuration (symlink to multi_chain/contracts_6565.yaml)
    ├── multi_chain/             # Multi-chain scenario
    │   ├── README.md            # Scenario-specific documentation
    │   ├── genesis.json         # Genesis configuration (symlink to parent genesis)
    │   ├── chain_6565.yaml      # Configuration for chain with ID 6565
    │   ├── chain_6566.yaml      # Configuration for chain with ID 6566
    │   ├── wallets_6565.yaml    # Wallets for chain 6565
    │   ├── wallets_6566.yaml    # Wallets for chain 6566
    │   ├── contracts_6565.yaml  # Contracts for chain 6565
    │   └── contracts_6566.yaml  # Contracts for chain 6566
│   ├── l1-state.json.gz         # Shared L1 state for protocol v31.0
│   ├── genesis.json             # Genesis configuration for protocol v31.0
    └── versions.yaml            # Version metadata for protocol v31.0
```

## Configuration Files

### `l1-state.json.gz`

L1 state snapshot for Anvil. Contains the deployed L1 contracts state. It can be decompressed and then loaded with:

```bash
gzip -dfk ./local-chains/v30.2/l1-state.json.gz
anvil --load-state ./local-chains/v30.2/l1-state.json --port 8545 --block-time 0.25 --mixed-mining
```

### `config.yaml`

Node configuration file used to override the default values defined in the [config module](../node/bin/src/config).
Commonly modified values include:

- `genesis.chain_id` — Chain ID of the chain node operates on
- `genesis.bridgehub_address` — Address of the Bridgehub contract on L1
- `genesis.bytecode_supplier_address` — Address of the bytecode supplier contract
- `l1_sender.operator_commit_sk` — Private key for committing batches
- `l1_sender.operator_prove_sk` — Private key for proving batches
- `l1_sender.operator_execute_sk` — Private key for executing batches

### `genesis.json`

ZKsync OS genesis configuration with the following fields:

- `initial_contracts` -- Initial contracts to deploy in genesis. Storage entries that set the contracts as deployed and preimages will be derived from this field.
- `additional_storage` -- Additional (not related to contract deployments) storage entries to add in genesis state. Should be used in case of custom genesis state, e.g. if migrating some existing state to ZKsync OS.
- `execution_version` -- Execution version to set for genesis block.
- `genesis_root` -- Root hash of the genesis block, which is calculated as `blake_hash(root, index, number, prev hashes, timestamp)`. Please note, that after updating  `additional_storage` and `initial_contracts` this field should be recalculated. 

Default `genesis.json` has empty `additional_storage` and three contracts in `initial_contracts`: `L2ComplexUpgrader`, `L2GenesisUpgrade`, `L2WrappedBaseToken`.
If you are changing source code of any of the `initial_contracts` you should also update the `genesis.json` file with new bytecode 
(you can find it in the `deployedBytecode` field in `zksync-era/contracts/l1-contracts/out/<FILE_NAME>/<CONTRACT_NAME>.json`).

## Usage

### Using the `run_local.sh` Script

⚠️ This script is a temporary solution. Do not depend on it in production.

The `run_local.sh` script automates starting Anvil and chain node(s):

```bash
# Run a single chain
./run_local.sh ./local-chains/v30.2/default

# Run multiple chains
./run_local.sh ./local-chains/v30.2/multi_chain

# Run with logging to files
./run_local.sh ./local-chains/v30.2/multi_chain --logs-dir ./logs
```

#### How the Script Works

1. **Validates configuration directory** - Checks that the directory exists and `l1-state.json.gz` is in parent directory
2. Decompresses `l1-state.json.gz` into `l1-state.json` (in temporary directory)
3. **Builds ZKsync OS**
4. **Starts Anvil** - Loads the L1 state snapshot on port 8545
5. **Waits for Anvil readiness** - Polls the JSON-RPC endpoint until Anvil responds (up to 30 seconds)
6. **Detects chain mode**:
   - If `config.json` exists → Starts single chain
   - Otherwise → Starts all `chain_*.json` files found (e.g., `chain_6565.json`, `chain_6566.json`)
7. **Database cleanup prompt** (single chain mode only) - If the `db/` folder contains existing data, prompts whether to clean it up before starting
8. **Monitors processes** - If any process fails, all services are stopped
9. **Graceful shutdown** - Press `Ctrl+C` to stop all services

#### Script Output

- **Anvil logs**: Suppressed (or written to `anvil-<timestamp>.log` if `--logs-dir` is specified)
- **Chain logs**: Displayed in terminal (or written to `<config-name>-<timestamp>.log` if `--logs-dir` is specified)
- **Script messages**: Color-coded status updates

### Manual Setup

#### Running a Single Chain

Follow the instructions in the [v30.2/single_chain/README.md](./v30.2/default/README.md).

#### Running Multiple Chains

Follow the instructions in the [v30.2/multi_chain/README.md](./v30.2/multi_chain/README.md).

## Adding a new protocol version

1. Create a new directory (e.g., `v31.1/`)
2. Use [upgrade scripts](https://github.com/matter-labs/zksync-os-scripts) to regenerate single and multi-chain configurations
3. Optionally add new scenario-specific subfolders if required
4. Update [protocol upgrade tests](../integration-tests/src/upgrade) to support the update to the new version
5. When upgrade is fully finalized, make sure:
   * The new default config in [main.rs](../node/bin/src/main.rs) is updated to point to the new version
   * `genesis.json` path in the [Dockerfile](../Dockerfile) is updated to point to the new version
   * `CURRENT_PROTOCOL_VERSION` constant in [integration tests](../integration-tests/src/config.rs) is updated to the new version.
   * [`test-configs.sh`](../.github/scripts/test-configs.sh) script is updated to properly test the new version.

## Troubleshooting

### Anvil failed to start

- Check if port 8545 is already in use: `lsof -i :8545`
- Verify that decompressed `l1-state.json` exists and is valid JSON

### Chain fails to start

- Check for port conflicts between chains
- Verify all required config fields are present
- Check the terminal output for specific error messages

### Multiple chains: port conflicts

- Each chain config must specify unique ports. `rpc.address` - JSON-RPC port (e.g., 3050, 3051, 3052)
- Chains should be run in ephemeral mode or use unique directory paths for RocksDB and file storage to avoid interfering with one another.
