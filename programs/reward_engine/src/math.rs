//! Deterministic, overflow-checked emission math. See spec §5.

use crate::constants::BPS_DENOMINATOR;
use crate::errors::RewardError;
use anchor_lang::prelude::*;

/// Number of remaining draws in the emission window (floored, min 1).
pub fn remaining_draws(
    now: i64,
    emission_end_ts: i64,
    draw_interval_seconds: i64,
) -> Result<u64> {
    require!(draw_interval_seconds > 0, RewardError::InvalidInterval);
    let remaining_time = emission_end_ts.saturating_sub(now).max(0);
    let draws = (remaining_time / draw_interval_seconds) as u64;
    // After emission end (or in the final interval) clamp to 1 so the formula
    // drains the remaining vault gracefully instead of dividing by zero.
    Ok(draws.max(1))
}

/// base_reward = vault_balance / remaining_draws  (integer division)
pub fn base_reward(vault_balance: u64, remaining_draws: u64) -> Result<u64> {
    require!(remaining_draws > 0, RewardError::MathOverflow);
    Ok(vault_balance / remaining_draws)
}

/// final = min( base * multiplier_bps / 10_000 , cap , vault_balance )
/// Multiplication is done in u128 to avoid overflow on large balances.
pub fn final_reward(
    base_reward: u64,
    multiplier_bps: u32,
    max_reward_per_draw: u64,
    vault_balance: u64,
) -> Result<u64> {
    let scaled = (base_reward as u128)
        .checked_mul(multiplier_bps as u128)
        .ok_or(RewardError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR as u128)
        .ok_or(RewardError::MathOverflow)?;

    let tier_reward = u64::try_from(scaled).map_err(|_| RewardError::MathOverflow)?;

    Ok(tier_reward.min(max_reward_per_draw).min(vault_balance))
}

/// Full pipeline used at settlement.
pub fn compute_reward(
    now: i64,
    emission_end_ts: i64,
    draw_interval_seconds: i64,
    vault_balance: u64,
    multiplier_bps: u32,
    max_reward_per_draw: u64,
) -> Result<u64> {
    let rd = remaining_draws(now, emission_end_ts, draw_interval_seconds)?;
    let base = base_reward(vault_balance, rd)?;
    final_reward(base, multiplier_bps, max_reward_per_draw, vault_balance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_reward_matches_decision() {
        // 16.8M eBTC @ 6 decimals over 367_920 draws (integer floor)
        let vault = 16_800_000u64 * 1_000_000;
        let base = base_reward(vault, 367_920).unwrap();
        assert_eq!(base, 45_662_100); // ≈ 45.662100 eBTC
    }

    #[test]
    fn tier_multipliers() {
        let base = 45_662_100u64;
        let cap = 200u64 * 1_000_000;
        let bal = u64::MAX;
        assert_eq!(final_reward(base, 10_000, cap, bal).unwrap(), 45_662_100);
        assert_eq!(final_reward(base, 12_500, cap, bal).unwrap(), 57_077_625);
        assert_eq!(final_reward(base, 30_000, cap, bal).unwrap(), 136_986_300);
    }

    #[test]
    fn cap_is_enforced() {
        let base = 1_000u64 * 1_000_000; // huge base
        let cap = 200u64 * 1_000_000;
        assert_eq!(final_reward(base, 30_000, cap, u64::MAX).unwrap(), cap);
    }

    #[test]
    fn never_overdraws_vault() {
        let base = 100u64 * 1_000_000;
        let bal = 10u64 * 1_000_000;
        assert_eq!(final_reward(base, 30_000, u64::MAX, bal).unwrap(), bal);
    }

    #[test]
    fn remaining_draws_clamped_after_end() {
        assert_eq!(remaining_draws(1_000, 500, 600).unwrap(), 1);
    }
}
