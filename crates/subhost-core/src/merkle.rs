//! Domain-separated binary Merkle tree.
//!
//! The tree is always a perfect binary tree: leaves are padded to the next power
//! of two with a fixed empty-leaf digest. Padding with a distinct, domain
//! separated leaf instead of duplicating the last leaf avoids the second-preimage
//! ambiguity that lets `[a, b, c]` and `[a, b, c, c]` share a root.
//!
//! Leaves are hashed by the caller through [`leaf_hash`] (or built directly with
//! [`merkle_root`]); internal nodes use a different domain tag, so a leaf digest
//! can never be reinterpreted as an internal node.

use crate::{encode_canonical, Hash};
use serde::{Deserialize, Serialize};

/// Domain tag for a real leaf.
const LEAF_DOMAIN: &[u8] = b"subhost-merkle-leaf-v1";
/// Domain tag for an internal node.
const NODE_DOMAIN: &[u8] = b"subhost-merkle-node-v1";
/// Domain tag for the padding leaf.
const EMPTY_DOMAIN: &[u8] = b"subhost-merkle-empty-v1";

/// Hash one leaf payload with leaf domain separation.
pub fn leaf_hash(data: &[u8]) -> Hash {
    let mut payload = Vec::with_capacity(LEAF_DOMAIN.len() + 8 + data.len());
    payload.extend_from_slice(LEAF_DOMAIN);
    payload.extend_from_slice(&(data.len() as u64).to_le_bytes());
    payload.extend_from_slice(data);
    Hash::from_data(&payload)
}

/// The digest used to pad an incomplete level.
pub fn empty_leaf() -> Hash {
    Hash::from_data(EMPTY_DOMAIN)
}

fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut payload = Vec::with_capacity(NODE_DOMAIN.len() + 2 * crate::HASH_SIZE);
    payload.extend_from_slice(NODE_DOMAIN);
    payload.extend_from_slice(left.as_bytes());
    payload.extend_from_slice(right.as_bytes());
    Hash::from_data(&payload)
}

/// Root of a tree over already-hashed leaves.
///
/// An empty input commits to the empty-leaf digest rather than to a special
/// "no tree" value that could collide with a one-leaf tree.
pub fn merkle_root(leaves: &[Hash]) -> Hash {
    if leaves.is_empty() {
        return empty_leaf();
    }
    let width = leaves.len().next_power_of_two();
    let mut level = Vec::with_capacity(width);
    level.extend_from_slice(leaves);
    level.resize(width, empty_leaf());

    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len() / 2);
        for pair in level.chunks_exact(2) {
            next.push(node_hash(&pair[0], &pair[1]));
        }
        level = next;
    }
    level[0]
}

/// An inclusion proof for one leaf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MerkleProof {
    pub leaf: Hash,
    pub index: u64,
    /// Bottom-up sibling digests; `siblings.len()` is the tree depth.
    pub siblings: Vec<Hash>,
}

impl MerkleProof {
    /// Verify that this proof commits `leaf` at `index` under `root`.
    pub fn verify(&self, root: &Hash) -> bool {
        verify_merkle_proof(root, &self.leaf, self.index, &self.siblings)
    }
}

/// Build an inclusion proof for `leaves[index]`.
///
/// Returns `None` when `index` is outside the leaf set.
pub fn merkle_proof(leaves: &[Hash], index: usize) -> Option<MerkleProof> {
    if index >= leaves.len() {
        return None;
    }
    let width = leaves.len().next_power_of_two();
    let mut level = Vec::with_capacity(width);
    level.extend_from_slice(leaves);
    level.resize(width, empty_leaf());

    let leaf = leaves[index];
    let mut cursor = index;
    let mut siblings = Vec::new();
    while level.len() > 1 {
        let sibling = cursor ^ 1;
        siblings.push(level[sibling]);
        let mut next = Vec::with_capacity(level.len() / 2);
        for pair in level.chunks_exact(2) {
            next.push(node_hash(&pair[0], &pair[1]));
        }
        level = next;
        cursor /= 2;
    }

    Some(MerkleProof { leaf, index: index as u64, siblings })
}

/// Verify a proof without rebuilding the tree.
pub fn verify_merkle_proof(root: &Hash, leaf: &Hash, index: u64, siblings: &[Hash]) -> bool {
    let mut node = *leaf;
    let mut cursor = index;
    for sibling in siblings {
        node = if cursor & 1 == 0 { node_hash(&node, sibling) } else { node_hash(sibling, &node) };
        cursor >>= 1;
    }
    // An index outside the padded width would leave high bits set; the fold would
    // still produce a digest, so reject it explicitly.
    cursor == 0 && node == *root
}

/// Convenience helper for key/value commitments such as IBC packet paths.
pub fn key_value_leaf(key: &[u8], value: &[u8]) -> Hash {
    leaf_hash(&encode_canonical(&(key, value)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaves(count: usize) -> Vec<Hash> {
        (0..count).map(|index| leaf_hash(&(index as u64).to_le_bytes())).collect()
    }

    #[test]
    fn empty_tree_has_a_distinct_root() {
        assert_eq!(merkle_root(&[]), empty_leaf());
        assert_ne!(merkle_root(&[]), leaf_hash(b""));
        assert!(merkle_proof(&[], 0).is_none());
    }

    #[test]
    fn every_leaf_proves_against_the_root() {
        for count in 1..=17 {
            let leaves = leaves(count);
            let root = merkle_root(&leaves);
            for index in 0..count {
                let proof = merkle_proof(&leaves, index).unwrap();
                assert!(proof.verify(&root), "leaf {index} of {count} must verify");
                assert_eq!(proof.leaf, leaves[index]);
                assert_eq!(proof.index, index as u64);
            }
            assert!(merkle_proof(&leaves, count).is_none());
        }
    }

    #[test]
    fn a_proof_cannot_be_replayed_at_another_index_or_against_another_root() {
        let leaves = leaves(5);
        let root = merkle_root(&leaves);
        let mut proof = merkle_proof(&leaves, 3).unwrap();

        proof.index = 2;
        assert!(!proof.verify(&root), "a valid path at the wrong index must fail");

        let mut other = leaves.clone();
        other[0] = leaf_hash(b"different");
        assert!(!proof.verify(&merkle_root(&other)));
    }

    #[test]
    fn padding_is_not_ambiguous_with_a_duplicated_last_leaf() {
        let four = leaves(4);
        let three = leaves(3);
        let mut duplicated = three.clone();
        duplicated.push(*three.last().unwrap());
        assert_ne!(merkle_root(&three), merkle_root(&duplicated));
        assert_ne!(merkle_root(&three), merkle_root(&four));
    }

    #[test]
    fn leaf_and_internal_domains_do_not_collide() {
        let left = leaf_hash(b"left");
        let right = leaf_hash(b"right");
        let root = merkle_root(&[left, right]);
        assert_ne!(root, leaf_hash(left.as_bytes()));
        assert_ne!(root, leaf_hash(right.as_bytes()));
    }

    #[test]
    fn proof_shape_is_deterministic() {
        let leaves = leaves(9);
        let first = merkle_proof(&leaves, 4).unwrap();
        let second = merkle_proof(&leaves, 4).unwrap();
        assert_eq!(first, second);
        assert_eq!(first.siblings.len(), 4, "nine leaves pad to a depth of four");
    }
}
