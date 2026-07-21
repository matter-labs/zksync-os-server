//! The layout-v1 reader: storage words in, a validated [`RegistryView`] out.
//!
//! Decoding and validation are deliberately separate passes, so the main flow
//! reads as the sequence of claims it establishes: `read_*` functions turn
//! slots into typed values and refuse malformed bytes; `ensure_*` functions
//! check relationships *between* decoded values. Any refusal anywhere means
//! the same thing to the caller: no committee, do not rotate.

use crate::v1::layout;
use crate::{
    MAX_ENTRY_SIZE, MAX_IDENTITIES, MAX_SCHEDULE_ENTRIES, RegistryIdentity, RegistryRefusal,
    RegistryScheduleEntry, RegistryView,
};
use alloy::primitives::{Address, B256, U256};
use commonware_codec::DecodeExt as _;
use commonware_cryptography::bls12381::primitives::variant::{MinPk, Variant};
use commonware_cryptography::ed25519;
use std::collections::HashMap;
use zksync_os_interface::traits::ReadStorage;

pub(crate) fn read(
    state: impl ReadStorage,
    address: Address,
    chain_id: u64,
) -> Result<RegistryView, RegistryRefusal> {
    let mut slots = Slots { state, address };

    let identity_count = read_identity_count(&mut slots)?;
    let entry_count = read_schedule_entry_count(&mut slots)?;

    let mut identities = Vec::with_capacity(identity_count as usize);
    for index in 0..identity_count {
        identities.push(read_identity(&mut slots, index)?);
    }
    ensure_keys_never_reused(&identities)?;
    ensure_proofs_of_possession(&identities, chain_id, address)?;

    let mut schedule = Vec::with_capacity(entry_count as usize);
    for index in 0..entry_count {
        schedule.push(read_schedule_entry(&mut slots, index, identity_count)?);
    }
    ensure_activations_increase(&schedule)?;
    ensure_entry_ingresses_distinct(&schedule, &identities)?;

    Ok(RegistryView {
        identities: identities
            .into_iter()
            .map(|decoded| decoded.identity)
            .collect(),
        schedule,
    })
}

/// Storage access scoped to the registry's address: hands out one slot's word
/// at a time, absent slots reading as zero (exactly the EVM's semantics).
struct Slots<S> {
    state: S,
    address: Address,
}

impl<S: ReadStorage> Slots<S> {
    fn word(&mut self, slot: U256) -> B256 {
        self.state
            .read(crate::flat_key(self.address, slot))
            .unwrap_or_default()
    }

    fn number(&mut self, slot: U256) -> U256 {
        U256::from_be_bytes(self.word(slot).0)
    }
}

/// An identity as decoded from storage: the public view plus the raw material
/// the cross-checks need (key bytes for reuse detection, the proof for
/// verification).
struct DecodedIdentity {
    identity: RegistryIdentity,
    bls_bytes: [u8; 48],
    network_bytes: [u8; 32],
    pop: <MinPk as Variant>::Signature,
}

fn read_identity_count(slots: &mut Slots<impl ReadStorage>) -> Result<u64, RegistryRefusal> {
    let count = slots.number(U256::from(layout::SLOT_IDENTITY_COUNT));
    if count > U256::from(MAX_IDENTITIES) {
        return Err(RegistryRefusal::TooManyIdentities { found: count });
    }
    Ok(count.to::<u64>())
}

fn read_schedule_entry_count(slots: &mut Slots<impl ReadStorage>) -> Result<u64, RegistryRefusal> {
    let count = slots.number(U256::from(layout::SLOT_SCHEDULE_COUNT));
    if count > U256::from(MAX_SCHEDULE_ENTRIES) {
        return Err(RegistryRefusal::TooManyScheduleEntries { found: count });
    }
    Ok(count.to::<u64>())
}

fn read_identity(
    slots: &mut Slots<impl ReadStorage>,
    index: u64,
) -> Result<DecodedIdentity, RegistryRefusal> {
    let base = layout::identity_base(index);
    let malformed = |field| RegistryRefusal::MalformedIdentity { index, field };

    let owner_word = slots.word(base + U256::from(layout::IDENTITY_OWNER));
    if owner_word.0[..12].iter().any(|byte| *byte != 0) {
        return Err(malformed("owner"));
    }
    let owner = Address::from_slice(&owner_word.0[12..]);
    if owner == Address::ZERO {
        return Err(malformed("owner"));
    }

    let mut bls_bytes = [0u8; 48];
    let high = slots.word(base + U256::from(layout::IDENTITY_BLS_HIGH));
    let low = slots.word(base + U256::from(layout::IDENTITY_BLS_LOW));
    bls_bytes[..32].copy_from_slice(&high.0);
    bls_bytes[32..].copy_from_slice(&low.0[..16]);
    if low.0[16..].iter().any(|byte| *byte != 0) {
        return Err(malformed("bls key"));
    }
    let bls_key = <MinPk as Variant>::Public::decode(bls_bytes.as_slice())
        .map_err(|_| malformed("bls key"))?;

    let network_word = slots.word(base + U256::from(layout::IDENTITY_NETWORK_KEY));
    let network_key = ed25519::PublicKey::decode(network_word.0.as_slice())
        .map_err(|_| malformed("network key"))?;

    let mut pop_bytes = [0u8; 96];
    pop_bytes[..32].copy_from_slice(&slots.word(base + U256::from(layout::IDENTITY_POP_A)).0);
    pop_bytes[32..64].copy_from_slice(&slots.word(base + U256::from(layout::IDENTITY_POP_B)).0);
    pop_bytes[64..].copy_from_slice(&slots.word(base + U256::from(layout::IDENTITY_POP_C)).0);
    let pop = <MinPk as Variant>::Signature::decode(pop_bytes.as_slice())
        .map_err(|_| malformed("proof of possession"))?;

    let ingress = layout::unpack_ingress(slots.word(base + U256::from(layout::IDENTITY_INGRESS)))
        .ok_or(malformed("ingress"))?;
    let egress = layout::unpack_egress(slots.word(base + U256::from(layout::IDENTITY_EGRESS)))
        .ok_or(malformed("egress"))?;

    Ok(DecodedIdentity {
        identity: RegistryIdentity {
            owner,
            bls_key,
            network_key,
            ingress,
            egress,
        },
        bls_bytes,
        network_bytes: network_word.0,
        pop,
    })
}

/// Keys are reserved forever: no key may appear under two identities, so
/// fault evidence and historical certificates always attribute unambiguously.
/// The contract enforces this at write time; the reader re-derives it instead
/// of trusting it.
fn ensure_keys_never_reused(identities: &[DecodedIdentity]) -> Result<(), RegistryRefusal> {
    let mut seen_bls: HashMap<[u8; 48], u64> = HashMap::new();
    let mut seen_network: HashMap<[u8; 32], u64> = HashMap::new();
    for (index, decoded) in identities.iter().enumerate() {
        let index = index as u64;
        if let Some(&previous) = seen_bls.get(&decoded.bls_bytes) {
            return Err(RegistryRefusal::KeyReused { index, previous });
        }
        if let Some(&previous) = seen_network.get(&decoded.network_bytes) {
            return Err(RegistryRefusal::KeyReused { index, previous });
        }
        seen_bls.insert(decoded.bls_bytes, index);
        seen_network.insert(decoded.network_bytes, index);
    }
    Ok(())
}

fn ensure_proofs_of_possession(
    identities: &[DecodedIdentity],
    chain_id: u64,
    registry: Address,
) -> Result<(), RegistryRefusal> {
    for (index, decoded) in identities.iter().enumerate() {
        crate::verify_proof_of_possession(
            &decoded.identity.bls_key,
            decoded.identity.owner,
            chain_id,
            registry,
            &decoded.pop,
        )
        .map_err(|_| RegistryRefusal::InvalidProofOfPossession {
            index: index as u64,
        })?;
    }
    Ok(())
}

fn read_schedule_entry(
    slots: &mut Slots<impl ReadStorage>,
    index: u64,
    identity_count: u64,
) -> Result<RegistryScheduleEntry, RegistryRefusal> {
    let base = layout::schedule_base(index);

    let activation = slots.number(base + U256::from(layout::ENTRY_ACTIVATION_EPOCH));
    if activation > U256::from(u64::MAX) {
        return Err(RegistryRefusal::MalformedEntry {
            index,
            field: "activation epoch",
        });
    }

    let member_count = slots.number(base + U256::from(layout::ENTRY_MEMBER_COUNT));
    if member_count == U256::ZERO {
        return Err(RegistryRefusal::EmptyEntry { index });
    }
    if member_count > U256::from(MAX_ENTRY_SIZE) {
        return Err(RegistryRefusal::EntryTooLarge {
            index,
            found: member_count,
        });
    }

    let mut members = Vec::with_capacity(member_count.to::<u64>() as usize);
    for position in 0..member_count.to::<u64>() {
        let member = slots.number(base + U256::from(layout::ENTRY_FIRST_MEMBER + position));
        if member >= U256::from(identity_count) {
            return Err(RegistryRefusal::UnknownMember { index, member });
        }
        let member = member.to::<u64>();
        if members.contains(&member) {
            return Err(RegistryRefusal::DuplicateMember { index, member });
        }
        members.push(member);
    }

    Ok(RegistryScheduleEntry {
        activation_epoch: activation.to::<u64>(),
        members,
    })
}

fn ensure_activations_increase(schedule: &[RegistryScheduleEntry]) -> Result<(), RegistryRefusal> {
    for (index, window) in schedule.windows(2).enumerate() {
        if window[1].activation_epoch <= window[0].activation_epoch {
            return Err(RegistryRefusal::NonMonotonicSchedule {
                index: index as u64 + 1,
                found: window[1].activation_epoch,
                previous: window[0].activation_epoch,
            });
        }
    }
    Ok(())
}

/// Two committee members listening on the same address cannot both be dialed;
/// an entry containing such a pair is a misconfiguration, not a committee.
fn ensure_entry_ingresses_distinct(
    schedule: &[RegistryScheduleEntry],
    identities: &[DecodedIdentity],
) -> Result<(), RegistryRefusal> {
    for (index, entry) in schedule.iter().enumerate() {
        for (a_position, &a) in entry.members.iter().enumerate() {
            for &b in &entry.members[a_position + 1..] {
                if identities[a as usize].identity.ingress
                    == identities[b as usize].identity.ingress
                {
                    return Err(RegistryRefusal::IngressCollision {
                        index: index as u64,
                        a,
                        b,
                    });
                }
            }
        }
    }
    Ok(())
}
