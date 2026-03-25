# Genesis v30.1

Genesis configuration for protocol version `v30.1`.

## Historical Note

Although the latest `v30` patch release is `v30.2`, for historical reasons we still register chains with the [`v30.1` genesis](./genesis.json).

*This means that new `v30` chain registration should continue using [`genesis.json`](./genesis.json) from this directory.*

## Diff vs `v30.2`

Compared to [`../v30.2/genesis.json`](../v30.2/genesis.json), the [`v30.1` genesis](./genesis.json) differs in the following way:

- Five entries in `initial_contracts` have different bytecode blobs:
  - `0x000000000000000000000000000000000000800f`
  - `0x0000000000000000000000000000000000010001`
  - `0x0000000000000000000000000000000000010007`
  - `0x000000000000000000000000000000000001000c`
  - `0xd704e29df32c189b8613f79fcc043b2dc01d5f53`
- In all five cases, the executable bytecode is identical between `v30.1` and `v30.2`.
- The only bytecode difference is the Solidity metadata trailer, i.e. metadata or IPFS hash changes rather than logic changes.
- `additional_storage` is identical.
- `additional_storage_raw` is identical.
- `genesis_root` differs because the full bytecode blobs differ:
  - `v30.1`: `0x423c107626aff95d3d086eabd92132dc9485e021ae3cb4c7735d5e963578e3d0`
  - `v30.2`: `0xa317d183fd2ed701814a993cfa76e458401bfa81c5da85e309dab4f92ebcba2e`

## Files

- [`genesis.json`](./genesis.json): genesis used for `v30` chain registration.

