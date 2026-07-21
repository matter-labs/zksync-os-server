//! Proofs of possession.
//!
//! BLS aggregation is exposed to rogue-key attacks, so a registered BLS key is
//! only trusted alongside a proof that someone actually holds its private
//! half. The registry contract stores the proof; nodes verify it here when
//! deriving a committee.
//!
//! The signed message binds the key to its owner address, the chain, and the
//! registry address, so a proof can be neither replayed onto another chain or
//! registry nor front-run into an entry under someone else's owner address.
//! The message format is a custody statement, independent of both the
//! consensus protocol version and the registry's storage layout version — it
//! must stay verifiable across upgrades of either.

use alloy::primitives::Address;
use commonware_codec::Encode as _;
use commonware_cryptography::bls12381::primitives::group;
use commonware_cryptography::bls12381::primitives::ops;
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant};

/// Domain tag; the `v1` names this message format, which versions on its own
/// terms (not with the storage layout).
const POP_NAMESPACE: &[u8] = b"zksync-os.registry.v1.pop";

/// The message a proof of possession signs.
pub fn proof_of_possession_message(
    bls_key: &<MinPk as Variant>::Public,
    owner: Address,
    chain_id: u64,
    registry: Address,
) -> Vec<u8> {
    let mut message = Vec::with_capacity(48 + 20 + 8 + 20);
    message.extend_from_slice(&bls_key.encode());
    message.extend_from_slice(owner.as_slice());
    message.extend_from_slice(&chain_id.to_be_bytes());
    message.extend_from_slice(registry.as_slice());
    message
}

/// Produces the proof of possession an operator registers alongside their
/// keys (see `tools/consensus-keygen`).
pub fn sign_proof_of_possession(
    bls_private: &group::Private,
    owner: Address,
    chain_id: u64,
    registry: Address,
) -> <MinPk as Variant>::Signature {
    let public = ops::compute_public::<MinPk>(bls_private);
    let message = proof_of_possession_message(&public, owner, chain_id, registry);
    ops::sign_message::<MinPk>(bls_private, POP_NAMESPACE, &message)
}

pub fn verify_proof_of_possession(
    bls_key: &<MinPk as Variant>::Public,
    owner: Address,
    chain_id: u64,
    registry: Address,
    pop: &<MinPk as Variant>::Signature,
) -> Result<(), commonware_cryptography::bls12381::primitives::Error> {
    let message = proof_of_possession_message(bls_key, owner, chain_id, registry);
    ops::verify_message::<MinPk>(bls_key, POP_NAMESPACE, &message, pop)
}
