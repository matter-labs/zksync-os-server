//! This file contains constants that are dependent on local state.
//! Please keep it in the `const VAR: type = "val"` format only
//! as it is used to be automatically updated.
//! Please, use #[rustfmt::skip] if a constant is formatted to occupy two lines.

/// Default path to RocksDB storage.
pub const DEFAULT_ROCKS_DB_PATH: &str = "./db/node1";

/// L1 address of `Bridgehub` contract. This address and chain ID is an entrypoint into L1 discoverability so most
/// other contracts should be discoverable through it.
pub const BRIDGEHUB_ADDRESS: &str = "0xd7e1f660ed86e1e9add0000ad860c5b1b1916be2";

/// L1 address of the `BytecodeSupplier` contract. This address right now cannot be discovered through `Bridgehub`,
/// so it has to be provided explicitly.
pub const BYTECODE_SUPPLIER_ADDRESS: &str = "0x7e6217aaa2b800d648fe6337b8dfe3c41d0ca12d";

/// Chain ID of the chain node operates on.
pub const CHAIN_ID: u64 = 6565;

/// Private key to commit batches to L1
/// Must be consistent with the operator key set on the contract (permissioned!)
#[rustfmt::skip]
pub const OPERATOR_COMMIT_PK: &str = "0x25c9b0b2560a79e646e8a3fb2eb997b5128d033943100b4b8a7dead752b34b79";

/// Private key to use to submit proofs to L1
/// Can be arbitrary funded address - proof submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_PROVE_PK: &str = "0x6380e89a46923b6d62877f374981f47635ca36ccc9706580db99d5f29440f9b4";

/// Private key to use to execute batches on L1
/// Can be arbitrary funded address - execute submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_EXECUTE_PK: &str = "0x854107184439c97d4a1f079aef4f34ecc764179b1159466dec9d29cffefca9cf";
