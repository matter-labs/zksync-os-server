# Genesis

ZKsync OS genesis is configurable through the `genesis.json` file. 
JSON has two fields:
- `initial_contracts` -- Initial contracts to deploy in genesis. Storage entries that set the contracts as deployed and preimages will be derived from this field.
- `additional_storage` -- Additional (not related to contract deployments) storage entries to add in genesis state. Should be used in case of custom genesis state, e.g. if migrating some existing state to ZKsync OS.

Default `genesis.json` has empty `additional_storage` and one contract in `initial_contracts` which is `L2ComplexUpgrader`.
If you are changing source code of any of the `initial_contracts` you should also update the `genesis.json` file with new bytecode 
(you can find it in the `deployedBytecode` field in `zksync-era/contracts/l1-contracts/out/<FILE_NAME>/<CONTRACT_NAME>.json`).
