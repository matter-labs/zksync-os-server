//! Era-relative block heights.
//!
//! Consensus lives in *era-relative* coordinates: heights counted from the era's
//! anchor block (the anchor itself is era height 0 — a fresh chain's genesis, or
//! the cutover block of a migrated chain). Epochs, committee schedules, and the
//! engine all reason in this space. The node's own chain heights are *absolute*.
//!
//! The two coincide exactly when the anchor is 0 — every fresh devnet, most
//! tests — which is precisely how an absolute height slips into era math
//! unnoticed and only misbehaves on migrated chains. [`EraHeight`] exists to
//! make that mistake unrepresentable: era math accepts only this type, and the
//! only conversion from a chain height demands the anchor.

use std::num::NonZeroU64;

/// A block height in era-relative coordinates (blocks since the era anchor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct EraHeight(u64);

impl EraHeight {
    /// Converts an absolute chain height, given the era's anchor height.
    ///
    /// Consensus never reasons about pre-era blocks, so `chain_height` is
    /// expected to be at or past the anchor; a violation clamps to 0 (and
    /// asserts in debug builds) rather than wrapping.
    pub fn from_chain(chain_height: u64, era_anchor: u64) -> Self {
        debug_assert!(
            chain_height >= era_anchor,
            "chain height {chain_height} precedes the era anchor {era_anchor}"
        );
        Self(chain_height.saturating_sub(era_anchor))
    }

    /// The raw era-relative height.
    pub fn get(self) -> u64 {
        self.0
    }

    /// The era height of this block's child.
    pub fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    /// The epoch containing this height (epochs partition era heights into
    /// `epoch_length`-sized runs, starting at the anchor).
    pub fn epoch(self, epoch_length: NonZeroU64) -> u64 {
        self.0 / epoch_length.get()
    }
}

#[cfg(test)]
mod tests {
    use super::EraHeight;
    use std::num::NonZeroU64;

    #[test]
    fn era_coordinates_subtract_the_anchor() {
        let epoch_length = NonZeroU64::new(4).expect("nonzero");
        // Fresh chain: chain and era coordinates coincide.
        assert_eq!(EraHeight::from_chain(7, 0).epoch(epoch_length), 1);
        // Migrated chain: the same chain height is early in the era.
        let anchored = EraHeight::from_chain(21, 20);
        assert_eq!(anchored.get(), 1);
        assert_eq!(anchored.epoch(epoch_length), 0);
        assert_eq!(anchored.next().get(), 2);
    }
}
