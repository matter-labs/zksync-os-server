

# Local Chains

This directory contains configuration files for running ZKsync OS nodes locally, organized by protocol version.

## Directory Structure

```
local-chains/
├── v30/                        # Current protocol version
│   ├── zkos-l1-state.json      # Base L1 state for this version
│   ├── genesis.json            # Base genesis configuration
│   ├── config.json             # Base node configuration
│   └── multiple-chains/        # Scenario-specific subfolder
│       ├── zkos-l1-state.json  # Shared L1 state for multiple chains
│       ├── genesis.json        # Shared genesis configuration
│       ├── chain1.json         # Configuration for chain #1
│       ├── chain2.json         # Configuration for chain #2
│       └── chain3.json         # Configuration for chain #3
└── v31/                        # Next protocol version
    ├── zkos-l1-state.json      # Base L1 state for this version
    ├── genesis.json            # Base genesis configuration
    └── config.json             # Base node configuration
```

## Configuration Files

### `zkos-l1-state.json`

L1 state snapshot for Anvil. Contains the deployed L1 contracts state that can be loaded with:

```bash
anvil --load-state ./local-chains/v30/zkos-l1-state.json --port 8545
```

### `config.json`

Node configuration file used to override the default values defined in `node/sequencer/config.rs`. Commonly modified values include:

- `genesis.chain_id` — Chain ID of the chain node operates on
- `genesis.bridgehub_address` — Address of the Bridgehub contract on L1
- `genesis.bytecode_supplier_address` — Address of the bytecode supplier contract
- `l1_sender.operator_commit_pk` — Private key for committing batches
- `l1_sender.operator_prove_pk` — Private key for proving batches
- `l1_sender.operator_execute_pk` — Private key for executing batches

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

### Running a Single Chain

1. Start Anvil with the L1 state:
   ```bash
   anvil --load-state ./local-chains/v30/zkos-l1-state.json --port 8545
   ```

2. Run the ZKsync OS server:
   ```bash
   cargo run --release
   ```

### Running Multiple Chains

The `multiple-chains/` subfolder contains configurations for running multiple chain instances against a shared L1 state.

1. Start Anvil with the shared L1 state:
   ```bash
   anvil --load-state ./local-chains/v30/multiple-chains/zkos-l1-state.json --port 8545
   ```

2. Run each chain instance in separate terminals:
   ```bash
   # Terminal 1
   cargo run -- --config ./local-chains/v30/multiple-chains/chain1.json
   
   # Terminal 2
   cargo run -- --config ./local-chains/v30/multiple-chains/chain2.json
   
   # Terminal 3
   cargo run -- --config ./local-chains/v30/multiple-chains/chain3.json
   ```

## Adding a New Protocol Version

When a new protocol version is released:

1. Create a new directory (e.g., `v31/`)
2. Generate new L1 state with updated contracts
3. Create appropriate `genesis.json` and `config.json` files
4. Optionally add scenario-specific subfolders (e.g., `multiple-chains/`)
5. Add a README.md with general information and the era-contracts branch used. Feel free to check existing files for the template.
