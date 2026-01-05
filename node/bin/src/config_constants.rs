//! This file contains constants that are dependent on local state.
//! Please keep it in the `const VAR: type = "val"` format only
//! as it is used to be automatically updated.
//! Please, use #[rustfmt::skip] if a constant is formatted to occupy two lines.

/// Default path to RocksDB storage.
pub const DEFAULT_ROCKS_DB_PATH: &str = "./db/node1";

/// L1 address of `Bridgehub` contract. This address and chain ID is an entrypoint into L1 discoverability so most
/// other contracts should be discoverable through it.
pub const BRIDGEHUB_ADDRESS: &str = "0x94b710b3de416f419959dc26710eebe675d4743e";

/// L1 address of the `BytecodeSupplier` contract. This address right now cannot be discovered through `Bridgehub`,
/// so it has to be provided explicitly.
pub const BYTECODE_SUPPLIER_ADDRESS: &str = "0xba7ace7c29944e15a5926176314a1f4dfa9c8267";

/// Chain ID of the chain node operates on.
pub const CHAIN_ID: u64 = 6565;

/// Private key to commit batches to L1
/// Must be consistent with the operator key set on the contract (permissioned!)
#[rustfmt::skip]
pub const OPERATOR_COMMIT_PK: &str = "0x09b8d48d8de31688789a8e44ace1bda0418a1041cf6d00ca384ce7d6a6b7f3fd";

/// Private key to use to submit proofs to L1
/// Can be arbitrary funded address - proof submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_PROVE_PK: &str = "0xa4e1be0303d6b5d2d6d31bb4afc09f6cb66f8d4211375df7c7d52aee888b04a6";

/// Private key to use to execute batches on L1
/// Can be arbitrary funded address - execute submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_EXECUTE_PK: &str = "0xdc31ab85e1e406aa3cc1ed705a16abdd1f230699c472a47e15697bccfe38e7dd";
