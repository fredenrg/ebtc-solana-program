//! Per-tier metadata commitment for public mints.
//!
//! The admin commits one Merkle root per tier over every public-mint token's
//! `(token_id, display_code, name, uri)`. `mint_nft` must present a proof, so buyers cannot
//! choose their own metadata. MUST match `keeper/src/nft-merkle.ts`:
//!
//!   leaf = sha256(0x00 || token_id_le32 || len16(display_code) || display_code
//!                      || len16(name) || name || len16(uri) || uri)
//!   node = sha256(0x01 || min(a, b) || max(a, b))
//!
//! Domain-separation bytes stop a node from being passed off as a leaf; sorted pairs mean
//! the proof needs no left/right flags.

use anchor_lang::solana_program::hash::hashv;

const LEAF_PREFIX: &[u8] = &[0x00];
const NODE_PREFIX: &[u8] = &[0x01];

pub fn leaf_hash(token_id: u32, display_code: &str, name: &str, uri: &str) -> [u8; 32] {
    let d = display_code.as_bytes();
    let n = name.as_bytes();
    let u = uri.as_bytes();
    hashv(&[
        LEAF_PREFIX,
        &token_id.to_le_bytes(),
        &(d.len() as u16).to_le_bytes(),
        d,
        &(n.len() as u16).to_le_bytes(),
        n,
        &(u.len() as u16).to_le_bytes(),
        u,
    ])
    .to_bytes()
}

pub fn verify(proof: &[[u8; 32]], root: &[u8; 32], leaf: [u8; 32]) -> bool {
    let mut acc = leaf;
    for sib in proof {
        acc = if acc <= *sib {
            hashv(&[NODE_PREFIX, &acc, sib]).to_bytes()
        } else {
            hashv(&[NODE_PREFIX, sib, &acc]).to_bytes()
        };
    }
    acc == *root
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_leaf_tree_round_trips() {
        let a = leaf_hash(100_033, "FX100033", "FX100 #100033", "https://x/a");
        let b = leaf_hash(100_034, "FX100034", "FX100 #100034", "https://x/b");
        let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
        let root = hashv(&[NODE_PREFIX, &lo, &hi]).to_bytes();
        assert!(verify(&[b], &root, a));
        assert!(verify(&[a], &root, b));
        let forged = leaf_hash(100_033, "FX100033", "FX100 #100033", "https://evil");
        assert!(!verify(&[b], &root, forged));
    }

    #[test]
    fn single_leaf_tree_has_empty_proof() {
        let a = leaf_hash(1, "A", "A", "u");
        assert!(verify(&[], &a, a));
    }
}
