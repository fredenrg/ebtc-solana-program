//! Tier-bucket weighted winner selection. See spec §7.
//!
//! Randomness derivation lives in `randomness.rs`; this module turns a final seed into a
//! concrete winner. Selection is deterministic given the seed, the per-tier snapshot
//! counts, and the optional bonus entry — so on-chain settle and off-chain keeper agree.

use crate::constants::{TIER_BUCKET_PAGE_SIZE, TIER_COUNT};
use crate::errors::RewardError;
use anchor_lang::prelude::*;

/// Per-tier weight contribution input for selection.
#[derive(Clone, Copy, Debug)]
pub struct TierWeight {
    pub active_count: u32,
    pub chance_weight: u32,
}

/// Optional bonus entry (one NFT, one extra block of `weight` units in its tier).
#[derive(Clone, Copy, Debug)]
pub struct BonusEntry {
    pub tier_id: u8,
    pub weight: u32,
}

/// Outcome of weighted selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Normal winner: the `index`-th active token within `tier_id`.
    Normal { tier_id: u8, index_in_tier: u32 },
    /// The bonus token in `tier_id` won.
    Bonus { tier_id: u8 },
}

/// Reduce a seed to a u64 in `[0, total_weight)`.
pub fn random_in_range(seed: &[u8; 32], total_weight: u64) -> Result<u64> {
    require!(total_weight > 0, RewardError::ZeroTotalWeight);
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&seed[0..8]);
    let r = u64::from_le_bytes(buf);
    Ok(r % total_weight)
}

/// Compute total weight from tiers + bonus. Returns error on overflow.
pub fn total_weight(tiers: &[TierWeight; TIER_COUNT], bonus: Option<BonusEntry>) -> Result<u64> {
    let mut sum: u64 = 0;
    for t in tiers.iter() {
        let tw = (t.active_count as u64)
            .checked_mul(t.chance_weight as u64)
            .ok_or(RewardError::MathOverflow)?;
        sum = sum.checked_add(tw).ok_or(RewardError::MathOverflow)?;
    }
    if let Some(b) = bonus {
        sum = sum
            .checked_add(b.weight as u64)
            .ok_or(RewardError::MathOverflow)?;
    }
    Ok(sum)
}

/// Tier-bucket weighted selection (spec §7).
///
/// Each active token in tier `t` occupies `chance_weight[t]` contiguous units.
/// The bonus (if present) appends one extra block of `bonus.weight` units to the
/// END of its tier's range, representing the bonus token's extra entry.
pub fn select_winner(
    r: u64,
    tiers: &[TierWeight; TIER_COUNT],
    bonus: Option<BonusEntry>,
) -> Result<Selection> {
    let mut cumulative: u64 = 0;
    for (tid, t) in tiers.iter().enumerate() {
        let normal_block = (t.active_count as u64)
            .checked_mul(t.chance_weight as u64)
            .ok_or(RewardError::MathOverflow)?;

        let bonus_block = match bonus {
            Some(b) if b.tier_id as usize == tid => b.weight as u64,
            _ => 0,
        };

        let tier_total = normal_block
            .checked_add(bonus_block)
            .ok_or(RewardError::MathOverflow)?;

        let next = cumulative
            .checked_add(tier_total)
            .ok_or(RewardError::MathOverflow)?;

        if r < next {
            let offset = r - cumulative;
            if offset < normal_block {
                // normal token: each token occupies `chance_weight` units
                let index_in_tier = (offset / t.chance_weight as u64) as u32;
                return Ok(Selection::Normal {
                    tier_id: tid as u8,
                    index_in_tier,
                });
            } else {
                // offset landed in the bonus block
                return Ok(Selection::Bonus { tier_id: tid as u8 });
            }
        }
        cumulative = next;
    }
    // r was validated < total_weight, so we must have returned above.
    Err(RewardError::WinnerVerificationFailed.into())
}

/// Map a within-tier active index to (page, slot) for TierBucket lookup.
pub fn index_to_page_slot(index_in_tier: u32) -> (u16, u16) {
    let page = (index_in_tier as usize / TIER_BUCKET_PAGE_SIZE) as u16;
    let slot = (index_in_tier as usize % TIER_BUCKET_PAGE_SIZE) as u16;
    (page, slot)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers(counts: [u32; 5]) -> [TierWeight; 5] {
        let weights = [10u32, 25, 36, 60, 78];
        let mut out = [TierWeight {
            active_count: 0,
            chance_weight: 0,
        }; 5];
        for i in 0..5 {
            out[i] = TierWeight {
                active_count: counts[i],
                chance_weight: weights[i],
            };
        }
        out
    }

    #[test]
    fn selects_first_token_at_zero() {
        let t = tiers([1, 1, 1, 1, 1]);
        let sel = select_winner(0, &t, None).unwrap();
        assert_eq!(sel, Selection::Normal { tier_id: 0, index_in_tier: 0 });
    }

    #[test]
    fn selects_last_unit() {
        let t = tiers([1, 0, 0, 0, 0]); // total weight 10
        let sel = select_winner(9, &t, None).unwrap();
        assert_eq!(sel, Selection::Normal { tier_id: 0, index_in_tier: 0 });
    }

    #[test]
    fn crosses_into_second_tier() {
        let t = tiers([1, 1, 0, 0, 0]); // tier0 = 0..10, tier1 = 10..35
        let sel = select_winner(10, &t, None).unwrap();
        assert_eq!(sel, Selection::Normal { tier_id: 1, index_in_tier: 0 });
    }

    #[test]
    fn bonus_block_wins_at_tail() {
        // tier4 has 1 active token (78 units) + bonus block (78 units) = 156 units total
        let t = tiers([0, 0, 0, 0, 1]);
        let bonus = Some(BonusEntry { tier_id: 4, weight: 78 });
        let total = total_weight(&t, bonus).unwrap();
        assert_eq!(total, 156);
        // r in [78,156) lands in bonus block
        let sel = select_winner(120, &t, bonus).unwrap();
        assert_eq!(sel, Selection::Bonus { tier_id: 4 });
        // r < 78 is the normal token
        let sel2 = select_winner(10, &t, bonus).unwrap();
        assert_eq!(sel2, Selection::Normal { tier_id: 4, index_in_tier: 0 });
    }

    #[test]
    fn index_within_tier() {
        let t = tiers([5, 0, 0, 0, 0]); // 5 tokens * 10 = 50 units
        // offset 23 -> index 2
        let sel = select_winner(23, &t, None).unwrap();
        assert_eq!(sel, Selection::Normal { tier_id: 0, index_in_tier: 2 });
    }

    #[test]
    fn page_slot_mapping() {
        assert_eq!(index_to_page_slot(0), (0, 0));
        assert_eq!(index_to_page_slot(255), (0, 255));
        assert_eq!(index_to_page_slot(256), (1, 0));
        assert_eq!(index_to_page_slot(513), (2, 1));
    }
}
