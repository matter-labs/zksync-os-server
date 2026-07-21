//! Registry storage layout version 1.
//!
//! Everything specific to this layout lives here — the slot map, the reader,
//! the test-side state builder, and the pinned contract bytecode. The crate
//! root dispatches into this module on the layout version it reads from the
//! chain; a future layout is a sibling `v2` module with its own copies of all
//! of this, mapping into the same version-independent output types. Layouts
//! never patch each other.

pub(crate) mod layout;
pub(crate) mod reader;

// The layout's writer-side surface, used by tests (and genesis seeding) to
// manufacture registry state, and by callers that assemble governance
// transactions. Explicitly versioned at every use site: `v1::pack_ingress`
// says which packing it is.
pub use layout::{
    RawIdentity, RegistryStateBuilder, SLOT_ACTIVATION_MARGIN, SLOT_EPOCH_LENGTH, SLOT_ERA_ANCHOR,
    SLOT_OWNER, pack_egress, pack_ingress, unpack_egress, unpack_ingress,
};

/// The pinned v1 contract bytecode, checked in like the wire goldens: the
/// deployed artifact must never drift from what this module's layout mirrors.
/// An integration test recompiles `contracts/` and fails on any difference;
/// regenerating the pin is a deliberate, reviewed act. The runtime bytecode is
/// deployment-independent (the contract holds no immutables), so it also
/// serves as-is for genesis seeding.
pub const PINNED_RUNTIME_BYTECODE_HEX: &str =
    include_str!("../../pinned/validator-registry-v1.runtime.hex");
/// The deploy (init) bytecode matching the pin; constructor arguments are
/// appended ABI-encoded by the caller.
pub const PINNED_DEPLOY_BYTECODE_HEX: &str =
    include_str!("../../pinned/validator-registry-v1.deploy.hex");
