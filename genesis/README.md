# Genesis

ZKsync OS genesis is configurable through the `genesis.json` file. 
JSON has fields:
- `initial_contracts` -- Initial contracts to deploy in genesis. Storage entries that set the contracts as deployed and preimages will be derived from this field.
- `additional_storage` -- Additional (not related to contract deployments) storage entries to add in genesis state. Should be used in case of custom genesis state, e.g. if migrating some existing state to ZKsync OS.
- `execution_version` -- Execution version to set for genesis block.
- `genesis_root` -- Root hash of the genesis block, which is calculated as `blake_hash(root, index, number, prev hashes, timestamp)`. Please note, that after updating  `additional_storage` and `initial_contracts` this field should be recalculated. 

Default `genesis.json` has empty `additional_storage` and three contracts in `initial_contracts`: `L2ComplexUpgrader`, `L2GenesisUpgrade`, `L2WrappedBaseToken`.
If you are changing source code of any of the `initial_contracts` you should also update the `genesis.json` file with new bytecode 
(you can find it in the `deployedBytecode` field in `zksync-era/contracts/l1-contracts/out/<FILE_NAME>/<CONTRACT_NAME>.json`).
