use alloy::primitives::{B256, b256, keccak256};

/// Height of the chain batch root tree; every leaf sits exactly this many hops below the root.
/// Mirrors `ChainBatchRootTree.TREE_DEPTH` (era-contracts) and the zksync-os bootloader's
/// `compute_chain_batch_root`.
pub const CHAIN_BATCH_ROOT_TREE_DEPTH: usize = 3;

/// Leaf index of the batch's local L2->L1 logs tree root.
pub const LOGS_ROOT_LEAF_INDEX: u64 = 0;
/// Leaf index of the chain's own multichain (aggregated MessageRoot) root.
pub const MULTICHAIN_ROOT_LEAF_INDEX: u64 = 1;
/// Leaf index of the interop commitment tree (IMT) root at batch begin.
pub const IMT_BEGIN_ROOT_LEAF_INDEX: u64 = 2;
/// Leaf index of the interop commitment tree (IMT) root at batch end.
pub const IMT_END_ROOT_LEAF_INDEX: u64 = 3;

/// Root of the reserved (all-zero) right subtree — a height-2 tree over the four zero leaves
/// 4..7: `z2` where `z1 = keccak256(0 || 0)`, `z2 = keccak256(z1 || z1)`. Locked against the
/// recomputation by a unit test; mirrors `ChainBatchRootTree.RESERVED_SUBTREE_NODE`.
pub const RESERVED_SUBTREE_NODE: B256 =
    b256!("0xb4c11951957c6f8f642c4af61cd6b24640fec6dc7fc607ee8206a99e92410d30");

fn node(left: B256, right: B256) -> B256 {
    keccak256([left.0, right.0].concat())
}

/// The chain batch root — the value a ZKsync OS chain commits as its batch's `l2LogsTreeRoot` —
/// as a fixed height-3 (8-leaf) keccak256 Merkle tree, bit-for-bit identical to the zksync-os
/// bootloader's `compute_chain_batch_root` and era-contracts' `ChainBatchRootTree.compute`.
///
/// Leaf layout: 0 = L2 logs root, 1 = multichain root, 2 = IMT root at batch begin,
/// 3 = IMT root at batch end, 4..7 reserved (zero).
pub fn compute_chain_batch_root(
    logs_root: B256,
    multichain_root: B256,
    imt_root_begin: B256,
    imt_root_end: B256,
) -> B256 {
    let live_subtree_node = node(
        node(logs_root, multichain_root),
        node(imt_root_begin, imt_root_end),
    );
    node(live_subtree_node, RESERVED_SUBTREE_NODE)
}

/// The three sibling hops that authenticate a leaf of the chain batch root tree, ordered leaf
/// level up, for the given leaf index. Used by proof builders: the L2->L1 log proof extends the
/// logs-tree path with the leaf-0 siblings, and the atomic-interop settlement proof authenticates
/// an IMT boundary root at leaf 2 (begin) or 3 (end).
pub fn chain_batch_root_leaf_siblings(
    leaf_index: u64,
    logs_root: B256,
    multichain_root: B256,
    imt_root_begin: B256,
    imt_root_end: B256,
) -> [B256; CHAIN_BATCH_ROOT_TREE_DEPTH] {
    match leaf_index {
        LOGS_ROOT_LEAF_INDEX => [
            multichain_root,
            node(imt_root_begin, imt_root_end),
            RESERVED_SUBTREE_NODE,
        ],
        MULTICHAIN_ROOT_LEAF_INDEX => [
            logs_root,
            node(imt_root_begin, imt_root_end),
            RESERVED_SUBTREE_NODE,
        ],
        IMT_BEGIN_ROOT_LEAF_INDEX => [
            imt_root_end,
            node(logs_root, multichain_root),
            RESERVED_SUBTREE_NODE,
        ],
        IMT_END_ROOT_LEAF_INDEX => [
            imt_root_begin,
            node(logs_root, multichain_root),
            RESERVED_SUBTREE_NODE,
        ],
        _ => panic!("chain batch root has no live leaf at index {leaf_index}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks `RESERVED_SUBTREE_NODE` against its definition.
    #[test]
    fn reserved_subtree_node_matches_recomputation() {
        let z1 = node(B256::ZERO, B256::ZERO);
        let z2 = node(z1, z1);
        assert_eq!(RESERVED_SUBTREE_NODE, z2);
    }

    /// Mirrors the zksync-os `chain_batch_root_is_height3_merkle` test: independent naive
    /// recomputation of the full 8-leaf tree.
    #[test]
    fn compute_matches_naive_height3_merkle() {
        let a = B256::repeat_byte(1);
        let b = B256::repeat_byte(2);
        let c = B256::repeat_byte(3);
        let d = B256::repeat_byte(4);
        let z = B256::ZERO;

        let l1 = [node(a, b), node(c, d), node(z, z), node(z, z)];
        let l2 = [node(l1[0], l1[1]), node(l1[2], l1[3])];
        let expected = node(l2[0], l2[1]);

        assert_eq!(compute_chain_batch_root(a, b, c, d), expected);
    }

    /// Every live leaf's sibling path folds back to the root at its index.
    #[test]
    fn leaf_siblings_reconstruct_root() {
        let leaves = [
            B256::repeat_byte(0xa),
            B256::repeat_byte(0xb),
            B256::repeat_byte(0xc),
            B256::repeat_byte(0xd),
        ];
        let root = compute_chain_batch_root(leaves[0], leaves[1], leaves[2], leaves[3]);

        for leaf_index in 0..4u64 {
            let siblings = chain_batch_root_leaf_siblings(
                leaf_index, leaves[0], leaves[1], leaves[2], leaves[3],
            );
            let mut current = leaves[leaf_index as usize];
            let mut index = leaf_index;
            for sibling in siblings {
                current = if index % 2 == 0 {
                    node(current, sibling)
                } else {
                    node(sibling, current)
                };
                index /= 2;
            }
            assert_eq!(
                current, root,
                "leaf {leaf_index} path must fold to the root"
            );
        }
    }
}
