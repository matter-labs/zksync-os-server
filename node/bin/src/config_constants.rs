//! This file contains constants that are dependent on local state.
//! Please keep it in the `const VAR: type = "val"` format only
//! as it is used to be automatically updated.
//! Please, use #[rustfmt::skip] if a constant is formatted to occupy two lines.

/// Default path to RocksDB storage.
pub const DEFAULT_ROCKS_DB_PATH: &str = "./db/node1";

/// L1 address of `Bridgehub` contract. This address and chain ID is an entrypoint into L1 discoverability so most
/// other contracts should be discoverable through it.
pub const BRIDGEHUB_ADDRESS: &str = "0x431880c76d6857f29e9283bb91918e2bed92516b";

/// L1 address of the `BytecodeSupplier` contract. This address right now cannot be discovered through `Bridgehub`,
/// so it has to be provided explicitly.
pub const BYTECODE_SUPPLIER_ADDRESS: &str = "0xdb9071a01138beb5ff40dabb4a5de842f17da1ec";

/// Chain ID of the chain node operates on.
pub const CHAIN_ID: u64 = 6565;

/// Private key to commit batches to L1
/// Must be consistent with the operator key set on the contract (permissioned!)
#[rustfmt::skip]
pub const OPERATOR_COMMIT_PK: &str = "0x316b48d6ba27471998eaf1daded180702fd38e3d7342af63337adbde043685a4";

/// Private key to use to submit proofs to L1
/// Can be arbitrary funded address - proof submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_PROVE_PK: &str = "0x56baafc1cdf46d2b244dcffdafeb42720e38af53a6586645b15a05e6386d7a7b";

/// Private key to use to execute batches on L1
/// Can be arbitrary funded address - execute submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_EXECUTE_PK: &str = "0x471e12193136a67720c1616b8db9b32e5ce3c1ff0ac9dbf69e4f3327ca17d87c";
