//! Speculative state for blocks consensus has not yet finalized.
//!
//! The node's state backend is strictly linear: one diff per block number, written only
//! after finality. Consensus, however, needs to execute blocks *before* they are final —
//! a leader builds on a block that is still being voted on, a follower verifies a
//! proposal against its (possibly also unfinalized) parent, and short-lived competing
//! candidates for the same height can exist at once.
//!
//! [`PendingState`] bridges the two worlds: a small tree of per-block overlays (storage
//! writes + published preimages) keyed by consensus block digest, floating above the
//! committed state. Reading through a branch walks its overlays newest-first and falls
//! through to the backend's view at the committed head. Nothing here is persisted:
//! when a block is finalized its overlay is dropped and the durable backend takes over;
//! when a block is abandoned its overlay is pruned.

use commonware_cryptography::sha256::Digest;
use std::collections::HashMap;
use std::sync::Arc;
use zksync_os_storage_api::state_override_view::OverrideProvider;

/// One block's worth of state changes, layered over its parent.
pub struct Overlay {
    storage: HashMap<alloy::primitives::B256, alloy::primitives::B256>,
    preimages: HashMap<alloy::primitives::B256, Vec<u8>>,
}

impl Overlay {
    pub fn new(
        storage: impl IntoIterator<Item = (alloy::primitives::B256, alloy::primitives::B256)>,
        preimages: impl IntoIterator<Item = (alloy::primitives::B256, Vec<u8>)>,
    ) -> Self {
        Self {
            storage: storage.into_iter().collect(),
            preimages: preimages.into_iter().collect(),
        }
    }
}

/// The overlays of one branch, tip-first. Implements the state-override interface the
/// sequencer's view wrapper composes with any base view.
#[derive(Clone)]
pub struct BranchOverrides {
    chain: Vec<Arc<Overlay>>,
}

impl BranchOverrides {
    /// A branch with no overlays: reads go straight to the committed backend.
    pub fn empty() -> Self {
        Self { chain: Vec::new() }
    }
}

impl OverrideProvider for BranchOverrides {
    fn get_storage_override(
        &self,
        key: &alloy::primitives::B256,
    ) -> Option<alloy::primitives::B256> {
        self.chain
            .iter()
            .find_map(|overlay| overlay.storage.get(key).copied())
    }

    fn get_preimage_override(&self, hash: &alloy::primitives::B256) -> Option<Vec<u8>> {
        self.chain
            .iter()
            .find_map(|overlay| overlay.preimages.get(hash).cloned())
    }
}

struct PendingBlock {
    height: u64,
    parent: Digest,
    overlay: Arc<Overlay>,
}

/// Where the durable chain currently ends.
#[derive(Debug, Clone, Copy)]
pub struct CommittedHead {
    pub height: u64,
    /// Consensus digest of the committed tip. `None` only right after a restart, when
    /// the node knows its height from the write-ahead log but consensus digests are not
    /// persisted; parents at the committed height are then matched by height alone.
    pub digest: Option<Digest>,
}

pub struct PendingState {
    committed: CommittedHead,
    blocks: HashMap<Digest, PendingBlock>,
}

impl PendingState {
    pub fn new(committed: CommittedHead) -> Self {
        Self {
            committed,
            blocks: HashMap::new(),
        }
    }

    pub fn committed(&self) -> CommittedHead {
        self.committed
    }

    /// Supplies the committed tip's digest when it was unknown (after a restart, until
    /// the consensus archive hands the tip block back). Never changes a known digest.
    pub fn adopt_committed_digest(&mut self, height: u64, digest: Digest) {
        assert_eq!(
            height, self.committed.height,
            "adopted digest must describe the committed tip"
        );
        let previous = self.committed.digest.replace(digest);
        assert!(
            previous.is_none_or(|known| known == digest),
            "adopted digest contradicts the known committed digest"
        );
    }

    /// Resolves the branch a child of `parent` executes on: the overlays from the
    /// parent down to (excluding) the committed head, tip-first. `None` means this
    /// parent's state is not available (unknown digest, or already superseded).
    ///
    /// A parent at the committed height with a matching digest — or any digest, if the
    /// digest is unknown after a restart — resolves to the empty branch: children read
    /// straight from the committed backend.
    pub fn branch_for_parent(
        &self,
        parent_height: u64,
        parent_digest: Digest,
    ) -> Option<BranchOverrides> {
        if parent_height == self.committed.height {
            return match self.committed.digest {
                Some(digest) if digest != parent_digest => None,
                // Matching digest, or digest unknown after restart: consensus (which
                // validated the parent's ancestry before handing it to us) vouches for
                // linkage that we cannot re-derive locally.
                _ => Some(BranchOverrides { chain: Vec::new() }),
            };
        }

        let mut chain = Vec::new();
        let mut cursor = parent_digest;
        let mut cursor_height = parent_height;
        while cursor_height > self.committed.height {
            let block = self.blocks.get(&cursor)?;
            debug_assert_eq!(block.height, cursor_height, "pending tree height mismatch");
            chain.push(block.overlay.clone());
            cursor = block.parent;
            cursor_height -= 1;
        }
        // The walk must have landed exactly on the committed tip (when its digest is
        // known). A dangling branch means bookkeeping went wrong somewhere.
        if let Some(committed_digest) = self.committed.digest
            && cursor != committed_digest
        {
            return None;
        }
        Some(BranchOverrides { chain })
    }

    /// Registers the overlay of a block that was just built or verified.
    pub fn insert(&mut self, digest: Digest, height: u64, parent: Digest, overlay: Overlay) {
        self.blocks.insert(
            digest,
            PendingBlock {
                height,
                parent,
                overlay: Arc::new(overlay),
            },
        );
    }

    pub fn contains(&self, digest: &Digest) -> bool {
        self.blocks.contains_key(digest)
    }

    /// Advances the committed head to a finalized block and prunes everything that can
    /// no longer be a parent: the finalized block's own overlay (the durable backend
    /// takes over) and every abandoned candidate at or below its height.
    pub fn advance_committed(&mut self, height: u64, digest: Digest) {
        assert_eq!(
            height,
            self.committed.height + 1,
            "committed head must advance one block at a time"
        );
        self.committed = CommittedHead {
            height,
            digest: Some(digest),
        };
        self.blocks.retain(|_, block| block.height > height);
    }

    /// Number of pending overlays (test/metrics probe).
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::B256;
    use commonware_cryptography::{Hasher, Sha256};

    fn digest(tag: u8) -> Digest {
        Sha256::hash(&[tag])
    }

    fn key(tag: u8) -> B256 {
        B256::repeat_byte(tag)
    }

    fn overlay_writing(slot: u8, value: u8) -> Overlay {
        Overlay::new([(key(slot), key(value))], [])
    }

    fn read(branch: &BranchOverrides, slot: u8) -> Option<B256> {
        branch.get_storage_override(&key(slot))
    }

    #[test]
    fn branches_are_isolated_and_read_tip_first() {
        let mut pending = PendingState::new(CommittedHead {
            height: 10,
            digest: Some(digest(0)),
        });

        // Two competing children of the committed tip writing different values to the
        // same slot, plus a grandchild on the first branch overwriting it again.
        pending.insert(digest(1), 11, digest(0), overlay_writing(7, 1));
        pending.insert(digest(2), 11, digest(0), overlay_writing(7, 2));
        pending.insert(digest(3), 12, digest(1), overlay_writing(7, 3));

        let branch_a = pending.branch_for_parent(11, digest(1)).unwrap();
        let branch_b = pending.branch_for_parent(11, digest(2)).unwrap();
        let branch_a_child = pending.branch_for_parent(12, digest(3)).unwrap();

        assert_eq!(read(&branch_a, 7), Some(key(1)));
        assert_eq!(read(&branch_b, 7), Some(key(2)));
        // Tip-first: the grandchild's write shadows its parent's.
        assert_eq!(read(&branch_a_child, 7), Some(key(3)));
        // Slots nobody wrote fall through to the base (no override).
        assert_eq!(read(&branch_a, 9), None);
    }

    #[test]
    fn committed_parent_resolves_to_empty_branch() {
        let pending = PendingState::new(CommittedHead {
            height: 10,
            digest: Some(digest(0)),
        });
        let branch = pending.branch_for_parent(10, digest(0)).unwrap();
        assert_eq!(read(&branch, 7), None);

        // A different digest at the committed height is not our chain.
        assert!(pending.branch_for_parent(10, digest(9)).is_none());
    }

    #[test]
    fn unknown_committed_digest_accepts_parent_by_height() {
        // Right after a restart the committed digest is unknown; the parent handed to
        // us by consensus (which validated ancestry) is accepted by height.
        let pending = PendingState::new(CommittedHead {
            height: 10,
            digest: None,
        });
        assert!(pending.branch_for_parent(10, digest(9)).is_some());
        // But an unknown *pending* parent above the head is still unresolvable.
        assert!(pending.branch_for_parent(11, digest(1)).is_none());
    }

    #[test]
    fn advancing_prunes_the_losing_branch() {
        let mut pending = PendingState::new(CommittedHead {
            height: 10,
            digest: Some(digest(0)),
        });
        pending.insert(digest(1), 11, digest(0), overlay_writing(7, 1));
        pending.insert(digest(2), 11, digest(0), overlay_writing(7, 2));
        pending.insert(digest(3), 12, digest(1), overlay_writing(7, 3));

        // Block 1 wins height 11: its own overlay retires (the durable backend now has
        // it), its sibling is abandoned, its child lives on.
        pending.advance_committed(11, digest(1));

        assert!(!pending.contains(&digest(1)));
        assert!(!pending.contains(&digest(2)));
        assert!(pending.contains(&digest(3)));
        assert_eq!(pending.len(), 1);

        // The surviving child now reaches the committed head directly.
        let branch = pending.branch_for_parent(12, digest(3)).unwrap();
        assert_eq!(read(&branch, 7), Some(key(3)));

        // The abandoned sibling can no longer be a parent.
        assert!(pending.branch_for_parent(11, digest(2)).is_none());
    }

    #[test]
    fn dangling_branch_is_unresolvable() {
        let mut pending = PendingState::new(CommittedHead {
            height: 10,
            digest: Some(digest(0)),
        });
        // A pending block whose parent chain does not reach the committed tip.
        pending.insert(digest(5), 11, digest(4), overlay_writing(7, 5));
        assert!(pending.branch_for_parent(11, digest(5)).is_none());
    }
}
