//! This file contains constants that are dependent on local state.
//! Please keep it in the `const VAR: type = "val"` format only
//! as it is used to be automatically updated.
//! Please, use #[rustfmt::skip] if a constant is formatted to occupy two lines.

/// L1 address of `Bridgehub` contract. This address and chain ID is an entrypoint into L1 discoverability so most
/// other contracts should be discoverable through it.
pub const BRIDGEHUB_ADDRESS: &str = "0x396e757c7b8cb2158b90b2d2d11eed4bd818e58c";

/// L1 address of the `BytecodeSupplier` contract. This address right now cannot be discovered through `Bridgehub`,
/// so it has to be provided explicitly.
pub const BYTECODE_SUPPLIER_ADDRESS: &str = "0xef0b6c2c85f321d876a6fd87e138bae974196623";

/// Chain ID of the chain node operates on.
pub const CHAIN_ID: u64 = 270;

/// Private key to commit batches to L1
/// Must be consistent with the operator key set on the contract (permissioned!)
#[rustfmt::skip]
pub const OPERATOR_COMMIT_PK: &str = "0xef2bd6efd849426877d76c24341bc3452d6b2157869442786f6c19d044e04942";

/// Private key to use to submit proofs to L1
/// Can be arbitrary funded address - proof submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_PROVE_PK: &str = "0x02ae689a20546e6c8a6b16e6c5c62ba67cbf654cd3d0c3ca49b0a64e61c4896a";

/// Private key to use to execute batches on L1
/// Can be arbitrary funded address - execute submission is permissionless.
#[rustfmt::skip]
pub const OPERATOR_EXECUTE_PK: &str = "0xce3307d58233b61e70dbf8300f73816a9541f138259b381860853898dd911aa2";
