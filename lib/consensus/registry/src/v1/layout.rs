//! The registry contract's storage layout, mirrored constant-for-constant.
//!
//! `contracts/src/ValidatorRegistry.sol` hand-assigns every field to an
//! explicit storage slot precisely so that this module can read it back
//! byte-for-byte without an ABI, a VM, or a compiler in the loop. The layout
//! is versioned: slot 0 holds the layout version, and readers must dispatch on
//! it before trusting anything else (see the crate root). This module is the
//! v1 layout; future layouts append slots and get their own modules.
//!
//! Also here: the packed socket-address codec shared by writers and readers,
//! and [`RegistryStateBuilder`], which produces the raw `(flat key, value)`
//! pairs of a populated registry — how simulator tests seed a registry into
//! genesis state without deploying anything.

use crate::flat_key;
use alloy::primitives::{Address, B256, U256, keccak256};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Scalar slots. The layout-version slot itself is `crate::SLOT_LAYOUT_VERSION`
/// (slot 0), the one location every layout shares.
pub const SLOT_OWNER: u64 = 1;
pub const SLOT_IDENTITY_COUNT: u64 = 2;
pub const SLOT_SCHEDULE_COUNT: u64 = 3;
pub const SLOT_ERA_ANCHOR: u64 = 5;
pub const SLOT_EPOCH_LENGTH: u64 = 6;
pub const SLOT_ACTIVATION_MARGIN: u64 = 7;

/// Table prefixes (must match the contract's string constants exactly).
const IDENTITY_PREFIX: &[u8] = b"zksync-os.registry.v1.identity";
const SCHEDULE_PREFIX: &[u8] = b"zksync-os.registry.v1.schedule";
const BLS_KEY_PREFIX: &[u8] = b"zksync-os.registry.v1.bls-key";
const NETWORK_KEY_PREFIX: &[u8] = b"zksync-os.registry.v1.network-key";

/// Field offsets within one identity's slot block.
pub const IDENTITY_OWNER: u64 = 0;
pub const IDENTITY_BLS_HIGH: u64 = 1;
pub const IDENTITY_BLS_LOW: u64 = 2;
pub const IDENTITY_NETWORK_KEY: u64 = 3;
pub const IDENTITY_POP_A: u64 = 4;
pub const IDENTITY_POP_B: u64 = 5;
pub const IDENTITY_POP_C: u64 = 6;
pub const IDENTITY_INGRESS: u64 = 7;
pub const IDENTITY_EGRESS: u64 = 8;

/// Field offsets within one schedule entry's slot block.
pub const ENTRY_ACTIVATION_EPOCH: u64 = 0;
pub const ENTRY_MEMBER_COUNT: u64 = 1;
pub const ENTRY_FIRST_MEMBER: u64 = 2;

/// The slot (as a 32-byte key) where identity `index`'s block begins.
pub fn identity_base(index: u64) -> U256 {
    table_base(IDENTITY_PREFIX, index)
}

/// The slot where schedule entry `index`'s block begins.
pub fn schedule_base(index: u64) -> U256 {
    table_base(SCHEDULE_PREFIX, index)
}

/// The key-reservation slot for a BLS public key (48 bytes).
pub fn bls_key_slot(key: &[u8; 48]) -> U256 {
    // The contract hashes the two storage words of the key: the first 32
    // bytes, then the remaining 16 left-aligned in a zero-padded word.
    let mut low = [0u8; 32];
    low[..16].copy_from_slice(&key[32..]);
    let mut preimage = Vec::with_capacity(BLS_KEY_PREFIX.len() + 64);
    preimage.extend_from_slice(BLS_KEY_PREFIX);
    preimage.extend_from_slice(&key[..32]);
    preimage.extend_from_slice(&low);
    U256::from_be_bytes(keccak256(&preimage).0)
}

/// The key-reservation slot for an ed25519 public key.
pub fn network_key_slot(key: &[u8; 32]) -> U256 {
    let mut preimage = Vec::with_capacity(NETWORK_KEY_PREFIX.len() + 32);
    preimage.extend_from_slice(NETWORK_KEY_PREFIX);
    preimage.extend_from_slice(key);
    U256::from_be_bytes(keccak256(&preimage).0)
}

fn table_base(prefix: &[u8], index: u64) -> U256 {
    // abi.encodePacked(<prefix string>, uint256(index))
    let mut preimage = Vec::with_capacity(prefix.len() + 32);
    preimage.extend_from_slice(prefix);
    preimage.extend_from_slice(&U256::from(index).to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(&preimage).0)
}

// ------------------------------------------------------------- socket codec

/// Packs an ingress socket address: byte 0 the IP version (4 or 6), bytes
/// 1..17 the IP (IPv4 in the first four bytes), bytes 17..19 the TCP port,
/// the rest zero. The contract checks this structure on write; the reader
/// rejects anything that does not round-trip.
pub fn pack_ingress(address: SocketAddr) -> B256 {
    let mut packed = [0u8; 32];
    write_ip(&mut packed, address.ip());
    packed[17..19].copy_from_slice(&address.port().to_be_bytes());
    B256::from(packed)
}

/// Packs an egress address: like an ingress without a port.
pub fn pack_egress(ip: IpAddr) -> B256 {
    let mut packed = [0u8; 32];
    write_ip(&mut packed, ip);
    B256::from(packed)
}

pub fn unpack_ingress(packed: B256) -> Option<SocketAddr> {
    let ip = read_ip(&packed.0)?;
    let port = u16::from_be_bytes([packed.0[17], packed.0[18]]);
    if port == 0 || packed.0[19..].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(SocketAddr::new(ip, port))
}

pub fn unpack_egress(packed: B256) -> Option<IpAddr> {
    let ip = read_ip(&packed.0)?;
    if packed.0[17..].iter().any(|byte| *byte != 0) {
        return None;
    }
    Some(ip)
}

fn write_ip(packed: &mut [u8; 32], ip: IpAddr) {
    match ip {
        IpAddr::V4(v4) => {
            packed[0] = 4;
            packed[1..5].copy_from_slice(&v4.octets());
        }
        IpAddr::V6(v6) => {
            packed[0] = 6;
            packed[1..17].copy_from_slice(&v6.octets());
        }
    }
}

fn read_ip(packed: &[u8; 32]) -> Option<IpAddr> {
    match packed[0] {
        4 => {
            if packed[5..17].iter().any(|byte| *byte != 0) {
                return None;
            }
            let mut octets = [0u8; 4];
            octets.copy_from_slice(&packed[1..5]);
            Some(IpAddr::V4(Ipv4Addr::from(octets)))
        }
        6 => {
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&packed[1..17]);
            Some(IpAddr::V6(Ipv6Addr::from(octets)))
        }
        _ => None,
    }
}

// ----------------------------------------------------------- state builder

/// One identity's raw material for [`RegistryStateBuilder`].
#[derive(Clone)]
pub struct RawIdentity {
    pub owner: Address,
    pub bls_key: [u8; 48],
    pub network_key: [u8; 32],
    pub pop: [u8; 96],
    pub ingress: SocketAddr,
    pub egress: IpAddr,
}

/// Builds the storage image of a populated registry: exactly the slots the
/// contract would have written, as `(flat key, value)` pairs ready to merge
/// into genesis state. Tests use this to stand up a registry without running
/// any contract code; the layout golden test pins it against the real
/// contract's writes so the two can never drift apart silently.
pub struct RegistryStateBuilder {
    address: Address,
    identities: Vec<RawIdentity>,
    entries: Vec<(u64, Vec<u64>)>,
    layout_version: u64,
}

impl RegistryStateBuilder {
    pub fn new(address: Address) -> Self {
        Self {
            address,
            identities: Vec::new(),
            entries: Vec::new(),
            layout_version: 1,
        }
    }

    /// Overrides the layout version — how tests manufacture the
    /// unknown-version refusal.
    pub fn with_layout_version(mut self, version: u64) -> Self {
        self.layout_version = version;
        self
    }

    pub fn identity(mut self, identity: RawIdentity) -> Self {
        self.identities.push(identity);
        self
    }

    /// Appends a schedule entry of identity indices activating at `epoch`.
    pub fn schedule_entry(mut self, epoch: u64, members: Vec<u64>) -> Self {
        self.entries.push((epoch, members));
        self
    }

    /// The storage image as `(flat key, value)` pairs — the form node state
    /// (and sim genesis) stores slots in.
    pub fn build(self) -> Vec<(B256, B256)> {
        let address = self.address;
        self.build_slots()
            .into_iter()
            .map(|(slot, value)| (flat_key(address, slot), value))
            .collect()
    }

    /// The storage image as raw `(slot, value)` pairs — the form
    /// `eth_getStorageAt` exposes, which is how the layout golden test
    /// compares this builder against the deployed contract's actual writes.
    pub fn build_slots(&self) -> Vec<(U256, B256)> {
        let mut slots: Vec<(U256, B256)> = vec![
            (
                U256::from(crate::SLOT_LAYOUT_VERSION),
                B256::from(U256::from(self.layout_version).to_be_bytes::<32>()),
            ),
            (
                U256::from(SLOT_IDENTITY_COUNT),
                B256::from(U256::from(self.identities.len()).to_be_bytes::<32>()),
            ),
            (
                U256::from(SLOT_SCHEDULE_COUNT),
                B256::from(U256::from(self.entries.len()).to_be_bytes::<32>()),
            ),
        ];
        for (index, identity) in self.identities.iter().enumerate() {
            let base = identity_base(index as u64);
            let mut owner_word = [0u8; 32];
            owner_word[12..].copy_from_slice(identity.owner.as_slice());
            let mut bls_low = [0u8; 32];
            bls_low[..16].copy_from_slice(&identity.bls_key[32..]);
            slots.push((base, B256::from(owner_word)));
            slots.push((
                base + U256::from(IDENTITY_BLS_HIGH),
                B256::from_slice(&identity.bls_key[..32]),
            ));
            slots.push((base + U256::from(IDENTITY_BLS_LOW), B256::from(bls_low)));
            slots.push((
                base + U256::from(IDENTITY_NETWORK_KEY),
                B256::from(identity.network_key),
            ));
            slots.push((
                base + U256::from(IDENTITY_POP_A),
                B256::from_slice(&identity.pop[..32]),
            ));
            slots.push((
                base + U256::from(IDENTITY_POP_B),
                B256::from_slice(&identity.pop[32..64]),
            ));
            slots.push((
                base + U256::from(IDENTITY_POP_C),
                B256::from_slice(&identity.pop[64..]),
            ));
            slots.push((
                base + U256::from(IDENTITY_INGRESS),
                pack_ingress(identity.ingress),
            ));
            slots.push((
                base + U256::from(IDENTITY_EGRESS),
                pack_egress(identity.egress),
            ));
            let reservation = B256::from(U256::from(index as u64 + 1).to_be_bytes::<32>());
            slots.push((bls_key_slot(&identity.bls_key), reservation));
            slots.push((network_key_slot(&identity.network_key), reservation));
        }
        for (index, (epoch, members)) in self.entries.iter().enumerate() {
            let base = schedule_base(index as u64);
            slots.push((base, B256::from(U256::from(*epoch).to_be_bytes::<32>())));
            slots.push((
                base + U256::from(ENTRY_MEMBER_COUNT),
                B256::from(U256::from(members.len()).to_be_bytes::<32>()),
            ));
            for (position, member) in members.iter().enumerate() {
                slots.push((
                    base + U256::from(ENTRY_FIRST_MEMBER + position as u64),
                    B256::from(U256::from(*member).to_be_bytes::<32>()),
                ));
            }
        }
        slots
    }
}
