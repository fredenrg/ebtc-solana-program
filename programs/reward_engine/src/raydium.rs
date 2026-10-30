//! Raydium CPMM (CP-Swap) integration: account parsing, CPI builders and pricing math.
//!
//! Vendored rather than depending on `raydium-cp-swap`, which pins Anchor 1.x and cannot be
//! linked into this Anchor 0.31 program. Layouts and instruction formats mirror
//! github.com/raydium-io/raydium-cp-swap `programs/cp-swap/src/{states,instructions}`.

use crate::errors::RewardError;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::{
    instruction::{AccountMeta, Instruction},
    program::invoke_signed,
    pubkey,
};

pub const CPMM_PROGRAM_ID: Pubkey = pubkey!("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");
pub const CPMM_DEVNET_PROGRAM_ID: Pubkey = pubkey!("DRaycpLY18LhpbydsBWbVJtxpNv9oXPgjRSfpF2bWpYb");
pub const WSOL_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const TOKEN_2022_PROGRAM_ID: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub const AUTH_SEED: &[u8] = b"vault_and_lp_mint_auth_seed";
pub const POOL_SEED: &[u8] = b"pool";
pub const POOL_LP_MINT_SEED: &[u8] = b"pool_lp_mint";
pub const POOL_VAULT_SEED: &[u8] = b"pool_vault";
pub const OBSERVATION_SEED: &[u8] = b"observation";

const IX_INITIALIZE: [u8; 8] = [175, 175, 109, 31, 13, 152, 155, 237];
const IX_DEPOSIT: [u8; 8] = [242, 35, 198, 137, 82, 225, 242, 182];
const IX_SWAP_BASE_INPUT: [u8; 8] = [143, 190, 90, 218, 196, 30, 51, 222];

const DISC_AMM_CONFIG: [u8; 8] = [218, 244, 33, 104, 203, 203, 43, 111];
const DISC_POOL_STATE: [u8; 8] = [247, 237, 227, 245, 215, 195, 222, 70];
const DISC_OBSERVATION: [u8; 8] = [122, 174, 197, 53, 129, 9, 165, 132];

pub const Q32: u128 = 1u128 << 32;
pub const FEE_RATE_DENOMINATOR: u64 = 1_000_000;
pub const OBSERVATION_NUM: usize = 100;

const POOL_STATE_LEN: usize = 637;
const AMM_CONFIG_LEN: usize = 236;
const OBSERVATION_LEN: usize = 4075;
const POOL_STATUS_DEPOSIT_DISABLED: u8 = 1 << 0;
const POOL_STATUS_SWAP_DISABLED: u8 = 1 << 2;

pub fn is_supported_program(program: &Pubkey) -> bool {
    *program == CPMM_PROGRAM_ID || *program == CPMM_DEVNET_PROGRAM_ID
}

fn read_pubkey(d: &[u8], o: usize) -> Pubkey {
    Pubkey::new_from_array(d[o..o + 32].try_into().unwrap())
}
fn read_u64(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}
fn read_u128(d: &[u8], o: usize) -> u128 {
    u128::from_le_bytes(d[o..o + 16].try_into().unwrap())
}

fn check_account(info: &AccountInfo, program: &Pubkey, disc: &[u8; 8], min_len: usize) -> Result<()> {
    require_keys_eq!(*info.owner, *program, RewardError::InvalidPoolAccount);
    let d = info.try_borrow_data()?;
    require!(d.len() >= min_len && d[..8] == disc[..], RewardError::InvalidPoolAccount);
    Ok(())
}

/// Trade fee in hundredths of a bip (1e-6). Only field we need from `AmmConfig`.
pub fn read_trade_fee_rate(amm_config: &AccountInfo, program: &Pubkey) -> Result<u64> {
    check_account(amm_config, program, &DISC_AMM_CONFIG, AMM_CONFIG_LEN)?;
    Ok(read_u64(&amm_config.try_borrow_data()?, 12))
}

pub struct PoolView {
    pub amm_config: Pubkey,
    pub token_0_vault: Pubkey,
    pub token_1_vault: Pubkey,
    pub lp_mint: Pubkey,
    pub token_0_mint: Pubkey,
    pub token_1_mint: Pubkey,
    pub observation_key: Pubkey,
    pub status: u8,
    pub lp_supply: u64,
    /// protocol + fund + creator fees owed out of each vault; not part of the curve.
    pub fees_0: u64,
    pub fees_1: u64,
}

impl PoolView {
    pub fn deposit_enabled(&self) -> bool {
        self.status & POOL_STATUS_DEPOSIT_DISABLED == 0
    }
    pub fn swap_enabled(&self) -> bool {
        self.status & POOL_STATUS_SWAP_DISABLED == 0
    }
    /// Curve reserves (vault balances minus fees owed), as Raydium prices them.
    pub fn reserves(&self, vault_0: u64, vault_1: u64) -> Result<(u64, u64)> {
        Ok((
            vault_0.checked_sub(self.fees_0).ok_or(RewardError::MathOverflow)?,
            vault_1.checked_sub(self.fees_1).ok_or(RewardError::MathOverflow)?,
        ))
    }
}

/// `PoolState` is `#[repr(C, packed)]` zero-copy; offsets include the 8-byte discriminator.
pub fn read_pool(pool: &AccountInfo, program: &Pubkey) -> Result<PoolView> {
    check_account(pool, program, &DISC_POOL_STATE, POOL_STATE_LEN)?;
    let d = pool.try_borrow_data()?;
    let fees = |protocol: usize, fund: usize, creator: usize| -> Result<u64> {
        read_u64(&d, protocol)
            .checked_add(read_u64(&d, fund))
            .and_then(|x| x.checked_add(read_u64(&d, creator)))
            .ok_or(error!(RewardError::MathOverflow))
    };
    Ok(PoolView {
        amm_config: read_pubkey(&d, 8),
        token_0_vault: read_pubkey(&d, 72),
        token_1_vault: read_pubkey(&d, 104),
        lp_mint: read_pubkey(&d, 136),
        token_0_mint: read_pubkey(&d, 168),
        token_1_mint: read_pubkey(&d, 200),
        observation_key: read_pubkey(&d, 296),
        status: d[329],
        lp_supply: read_u64(&d, 333),
        fees_0: fees(341, 357, 397)?,
        fees_1: fees(349, 365, 405)?,
    })
}

#[derive(Clone, Copy, Default)]
pub struct Observation {
    pub ts: u64,
    pub cumulative: u128,
}

pub struct ObservationView {
    pub initialized: bool,
    pub index: usize,
    pub last_update_ts: u64,
    /// Heap-allocated: 100 slots are 3.2KB, most of the 4KB SBF stack frame.
    pub observations: Vec<Observation>,
}

/// `token_0` selects which cumulative to read: Raydium accumulates token_0's price in token_1
/// and token_1's price in token_0, so pass whichever side is SOL to get eBTC per lamport.
pub fn read_observation(obs: &AccountInfo, program: &Pubkey, token_0: bool) -> Result<ObservationView> {
    check_account(obs, program, &DISC_OBSERVATION, OBSERVATION_LEN)?;
    let d = obs.try_borrow_data()?;
    let index = u16::from_le_bytes([d[9], d[10]]) as usize;
    require!(index < OBSERVATION_NUM, RewardError::InvalidPoolAccount);
    let observations = (0..OBSERVATION_NUM)
        .map(|i| {
            let base = 43 + i * 40;
            Observation {
                ts: read_u64(&d, base),
                cumulative: read_u128(&d, if token_0 { base + 8 } else { base + 24 }),
            }
        })
        .collect();
    Ok(ObservationView {
        initialized: d[8] != 0,
        index,
        last_update_ts: read_u64(&d, 43 + OBSERVATION_NUM * 40),
        observations,
    })
}

/// Price of `base` in `quote` units, Q32, the same way Raydium feeds its oracle.
pub fn price_x32(base_reserve: u64, quote_reserve: u64) -> Result<u128> {
    require!(base_reserve > 0, RewardError::InvalidPoolAccount);
    (quote_reserve as u128)
        .checked_mul(Q32)
        .map(|x| x / base_reserve as u128)
        .ok_or(error!(RewardError::MathOverflow))
}

/// Time-weighted price over `[max(now - window, seed_ts), now]`.
///
/// Before the first swap Raydium leaves the ring uninitialised and the price can only have
/// moved through proportional deposits, so the seed price stands in. Slot `k`'s cumulative is
/// stamped with `ts_k` but may include up to 15s of later accrual (Raydium folds sub-15s
/// updates into the current slot), which biases the result by at most 15s of price weight.
pub fn twap_x32(
    obs: &ObservationView,
    spot_x32: u128,
    seed_price_x32: u128,
    seed_ts: u64,
    now: u64,
    window: u64,
) -> Option<u128> {
    let start = now.saturating_sub(window).max(seed_ts);
    if now <= start {
        return Some(spot_x32);
    }
    if !obs.initialized {
        return Some(seed_price_x32);
    }

    let latest = obs.observations[obs.index];
    let accrued_to = if obs.last_update_ts == 0 { latest.ts } else { obs.last_update_ts };
    let cum_now = latest
        .cumulative
        .wrapping_add(spot_x32.checked_mul(now.saturating_sub(accrued_to) as u128)?);

    let wrapped = obs.observations[(obs.index + 1) % OBSERVATION_NUM].ts != 0;
    let point = |i: usize| -> Observation {
        let o = obs.observations[i];
        // The first slot is created at the first swap with a zero cumulative.
        if !wrapped && i == 0 {
            Observation { ts: o.ts, cumulative: 0 }
        } else {
            o
        }
    };

    let mut best: Option<Observation> = None;
    let mut oldest: Option<Observation> = None;
    for i in 0..OBSERVATION_NUM {
        let o = point(i);
        if o.ts == 0 || o.ts > now {
            continue;
        }
        if o.ts <= start && best.map_or(true, |b| o.ts > b.ts) {
            best = Some(o);
        }
        if oldest.map_or(true, |x| o.ts < x.ts) {
            oldest = Some(o);
        }
    }

    if let Some(b) = best {
        let dt = now - b.ts;
        return if dt == 0 { Some(spot_x32) } else { Some(cum_now.wrapping_sub(b.cumulative) / dt as u128) };
    }

    let first = oldest?;
    if !wrapped {
        // Ring starts after `start`: the seed price held from `start` until the first swap.
        let pre = first.ts.saturating_sub(start) as u128;
        let total = cum_now
            .wrapping_sub(first.cumulative)
            .checked_add(seed_price_x32.checked_mul(pre)?)?;
        return Some(total / (now - start) as u128);
    }
    let dt = now - first.ts;
    if dt == 0 {
        Some(spot_x32)
    } else {
        Some(cum_now.wrapping_sub(first.cumulative) / dt as u128)
    }
}

/// `|spot - twap| <= twap * max_bps / 10_000`.
pub fn within_deviation(spot_x32: u128, twap_x32: u128, max_bps: u16) -> bool {
    let diff = spot_x32.abs_diff(twap_x32);
    match (diff.checked_mul(10_000), twap_x32.checked_mul(max_bps as u128)) {
        (Some(l), Some(r)) => l <= r,
        _ => false,
    }
}

fn isqrt(n: u128) -> u128 {
    if n < 2 {
        return n;
    }
    let mut x = 1u128 << ((128 - n.leading_zeros() + 1) / 2);
    loop {
        let y = (x + n / x) >> 1;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// How much of `amount` to swap into the other side of a `reserve_in` pool so the remainder
/// pairs with the output at the post-swap ratio (single-sided zap):
///   x = (sqrt(R²(F+g)² + 4·g·F·A·R) − R(F+g)) / 2g,  with g = F·(1 − fee).
/// Uses bps precision (fee rounded up) to keep the square inside u128; overestimating the fee
/// only under-swaps slightly, leaving dust for the next route.
pub fn zap_swap_amount(amount: u64, reserve_in: u64, trade_fee_rate: u64) -> u64 {
    const F: u128 = 10_000;
    let fee_bps = (trade_fee_rate as u128).div_ceil(100).min(F - 1);
    let g = F - fee_bps;
    let r = reserve_in as u128;
    let a = amount as u128;
    let calc = || -> Option<u128> {
        let b = r.checked_mul(F + g)?;
        let disc = b.checked_mul(b)?.checked_add(
            4u128.checked_mul(g)?.checked_mul(F)?.checked_mul(a)?.checked_mul(r)?,
        )?;
        Some(isqrt(disc).saturating_sub(b) / (2 * g))
    };
    calc().map_or(amount / 2, |x| x.min(a) as u64)
}

/// Largest LP amount Raydium can mint for these balances. Raydium charges `ceil(lp·R/supply)`
/// per side, which stays within each balance when `lp = floor(balance·supply/R)`.
pub fn lp_for_balances(bal_0: u64, bal_1: u64, reserve_0: u64, reserve_1: u64, lp_supply: u64) -> u64 {
    if reserve_0 == 0 || reserve_1 == 0 {
        return 0;
    }
    let s = lp_supply as u128;
    let lp0 = (bal_0 as u128) * s / reserve_0 as u128;
    let lp1 = (bal_1 as u128) * s / reserve_1 as u128;
    lp0.min(lp1).min(u64::MAX as u128) as u64
}

fn meta(info: &AccountInfo, writable: bool, signer: bool) -> AccountMeta {
    if writable {
        AccountMeta::new(*info.key, signer)
    } else {
        AccountMeta::new_readonly(*info.key, signer)
    }
}

fn invoke<'info>(
    program: &AccountInfo<'info>,
    data: Vec<u8>,
    accounts: &[(&AccountInfo<'info>, bool, bool)],
    seeds: &[&[&[u8]]],
) -> Result<()> {
    let ix = Instruction {
        program_id: *program.key,
        accounts: accounts.iter().map(|(a, w, s)| meta(a, *w, *s)).collect(),
        data,
    };
    let mut infos: Vec<AccountInfo> = accounts.iter().map(|(a, _, _)| (*a).clone()).collect();
    infos.push(program.clone());
    invoke_signed(&ix, &infos, seeds).map_err(Into::into)
}

fn ix_data(disc: [u8; 8], args: &[u64]) -> Vec<u8> {
    let mut data = Vec::with_capacity(8 + args.len() * 8);
    data.extend_from_slice(&disc);
    for a in args {
        data.extend_from_slice(&a.to_le_bytes());
    }
    data
}

pub struct InitializeAccounts<'a, 'info> {
    pub program: &'a AccountInfo<'info>,
    pub creator: &'a AccountInfo<'info>,
    pub amm_config: &'a AccountInfo<'info>,
    pub authority: &'a AccountInfo<'info>,
    pub pool_state: &'a AccountInfo<'info>,
    pub token_0_mint: &'a AccountInfo<'info>,
    pub token_1_mint: &'a AccountInfo<'info>,
    pub lp_mint: &'a AccountInfo<'info>,
    pub creator_token_0: &'a AccountInfo<'info>,
    pub creator_token_1: &'a AccountInfo<'info>,
    pub creator_lp_token: &'a AccountInfo<'info>,
    pub token_0_vault: &'a AccountInfo<'info>,
    pub token_1_vault: &'a AccountInfo<'info>,
    pub create_pool_fee: &'a AccountInfo<'info>,
    pub observation_state: &'a AccountInfo<'info>,
    pub token_program: &'a AccountInfo<'info>,
    pub associated_token_program: &'a AccountInfo<'info>,
    pub system_program: &'a AccountInfo<'info>,
    pub rent: &'a AccountInfo<'info>,
}

/// `initialize(init_amount_0, init_amount_1, open_time)`. `creator` pays the pool rent and the
/// create-pool fee and receives the LP, so it must be a lamport-holding system-owned signer.
pub fn initialize(a: InitializeAccounts, amount_0: u64, amount_1: u64, open_time: u64, seeds: &[&[&[u8]]]) -> Result<()> {
    invoke(
        a.program,
        ix_data(IX_INITIALIZE, &[amount_0, amount_1, open_time]),
        &[
            (a.creator, true, true),
            (a.amm_config, false, false),
            (a.authority, false, false),
            (a.pool_state, true, false),
            (a.token_0_mint, false, false),
            (a.token_1_mint, false, false),
            (a.lp_mint, true, false),
            (a.creator_token_0, true, false),
            (a.creator_token_1, true, false),
            (a.creator_lp_token, true, false),
            (a.token_0_vault, true, false),
            (a.token_1_vault, true, false),
            (a.create_pool_fee, true, false),
            (a.observation_state, true, false),
            (a.token_program, false, false),
            (a.token_program, false, false),
            (a.token_program, false, false),
            (a.associated_token_program, false, false),
            (a.system_program, false, false),
            (a.rent, false, false),
        ],
        seeds,
    )
}

pub struct SwapAccounts<'a, 'info> {
    pub program: &'a AccountInfo<'info>,
    pub payer: &'a AccountInfo<'info>,
    pub authority: &'a AccountInfo<'info>,
    pub amm_config: &'a AccountInfo<'info>,
    pub pool_state: &'a AccountInfo<'info>,
    pub input_token_account: &'a AccountInfo<'info>,
    pub output_token_account: &'a AccountInfo<'info>,
    pub input_vault: &'a AccountInfo<'info>,
    pub output_vault: &'a AccountInfo<'info>,
    pub token_program: &'a AccountInfo<'info>,
    pub input_mint: &'a AccountInfo<'info>,
    pub output_mint: &'a AccountInfo<'info>,
    pub observation_state: &'a AccountInfo<'info>,
}

pub fn swap_base_input(a: SwapAccounts, amount_in: u64, minimum_out: u64, seeds: &[&[&[u8]]]) -> Result<()> {
    invoke(
        a.program,
        ix_data(IX_SWAP_BASE_INPUT, &[amount_in, minimum_out]),
        &[
            (a.payer, false, true),
            (a.authority, false, false),
            (a.amm_config, false, false),
            (a.pool_state, true, false),
            (a.input_token_account, true, false),
            (a.output_token_account, true, false),
            (a.input_vault, true, false),
            (a.output_vault, true, false),
            (a.token_program, false, false),
            (a.token_program, false, false),
            (a.input_mint, false, false),
            (a.output_mint, false, false),
            (a.observation_state, true, false),
        ],
        seeds,
    )
}

pub struct DepositAccounts<'a, 'info> {
    pub program: &'a AccountInfo<'info>,
    pub owner: &'a AccountInfo<'info>,
    pub authority: &'a AccountInfo<'info>,
    pub pool_state: &'a AccountInfo<'info>,
    pub owner_lp_token: &'a AccountInfo<'info>,
    pub token_0_account: &'a AccountInfo<'info>,
    pub token_1_account: &'a AccountInfo<'info>,
    pub token_0_vault: &'a AccountInfo<'info>,
    pub token_1_vault: &'a AccountInfo<'info>,
    pub token_program: &'a AccountInfo<'info>,
    pub token_program_2022: &'a AccountInfo<'info>,
    pub vault_0_mint: &'a AccountInfo<'info>,
    pub vault_1_mint: &'a AccountInfo<'info>,
    pub lp_mint: &'a AccountInfo<'info>,
}

pub fn deposit(a: DepositAccounts, lp_amount: u64, max_0: u64, max_1: u64, seeds: &[&[&[u8]]]) -> Result<()> {
    invoke(
        a.program,
        ix_data(IX_DEPOSIT, &[lp_amount, max_0, max_1]),
        &[
            (a.owner, false, true),
            (a.authority, false, false),
            (a.pool_state, true, false),
            (a.owner_lp_token, true, false),
            (a.token_0_account, true, false),
            (a.token_1_account, true, false),
            (a.token_0_vault, true, false),
            (a.token_1_vault, true, false),
            (a.token_program, false, false),
            (a.token_program_2022, false, false),
            (a.vault_0_mint, false, false),
            (a.vault_1_mint, false, false),
            (a.lp_mint, true, false),
        ],
        seeds,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs_view(entries: &[(u64, u128)], index: usize, last_update_ts: u64) -> ObservationView {
        let mut observations = vec![Observation::default(); OBSERVATION_NUM];
        for (i, (ts, c)) in entries.iter().enumerate() {
            observations[i] = Observation { ts: *ts, cumulative: *c };
        }
        ObservationView { initialized: true, index, last_update_ts, observations }
    }

    #[test]
    fn isqrt_is_floor() {
        for n in [0u128, 1, 2, 3, 4, 15, 16, 17, 1 << 64, u128::MAX] {
            let r = isqrt(n);
            assert!(r * r <= n);
            assert!((r + 1).checked_mul(r + 1).map_or(true, |x| x > n));
        }
    }

    #[test]
    fn zap_pairs_the_remainder() {
        // 45.87 SOL pool, 0.25% fee, 0.808 SOL sale.
        let r_sol = 45_870_000_000u64;
        let r_ebtc = 2_700_000_000_000u64;
        let a = 808_000_000u64;
        let x = zap_swap_amount(a, r_sol, 2_500);
        assert!(x > a * 49 / 100 && x < a * 51 / 100, "x={x}");
        let out = (x as u128 * 9_975 / 10_000) * r_ebtc as u128 / (r_sol as u128 + x as u128 * 9_975 / 10_000);
        let rem = (a - x) as u128;
        // remaining SOL / new SOL reserve ~= eBTC out / new eBTC reserve
        let lhs = rem * (r_ebtc as u128 - out);
        let rhs = out * (r_sol as u128 + x as u128);
        assert!(lhs.abs_diff(rhs) * 1_000 < rhs, "lhs={lhs} rhs={rhs}");
    }

    #[test]
    fn zap_falls_back_on_overflow() {
        assert_eq!(zap_swap_amount(1_000, u64::MAX, 2_500), 500);
    }

    #[test]
    fn lp_never_overdraws() {
        let (r0, r1, s) = (45_870_000_123u64, 2_700_000_000_777u64, 351_928_000_000u64);
        let (b0, b1) = (400_000_001u64, 23_544_000_009u64);
        let lp = lp_for_balances(b0, b1, r0, r1, s);
        let need = |b: u64, r: u64| ((lp as u128 * r as u128) + s as u128 - 1) / s as u128 <= b as u128;
        assert!(need(b0, r0) && need(b1, r1));
    }

    #[test]
    fn twap_uninitialised_uses_seed_price() {
        let mut v = obs_view(&[], 0, 0);
        v.initialized = false;
        assert_eq!(twap_x32(&v, 999, 100, 1_000, 2_000, 900), Some(100));
    }

    #[test]
    fn twap_constant_price() {
        // swaps at 1000 and 1100, price 50 throughout
        let v = obs_view(&[(1_000, 0), (1_100, 50 * 100)], 1, 1_100);
        assert_eq!(twap_x32(&v, 50, 50, 900, 2_000, 600), Some(50));
    }

    #[test]
    fn twap_blends_seed_price_before_first_swap() {
        // seeded at 0 with price 100; first swap at 500; price 200 since. Window [400, 1000].
        let v = obs_view(&[(500, 0)], 0, 500);
        let t = twap_x32(&v, 200, 100, 0, 1_000, 600).unwrap();
        // 100 for 100s + 200 for 500s over 600s
        assert_eq!(t, (100 * 100 + 200 * 500) / 600);
    }

    #[test]
    fn twap_resists_a_fresh_spike() {
        // price 100 for a long time, spiked to 1000 one second ago
        let v = obs_view(&[(1_000, 0), (1_015, 100 * 15), (1_999, 100 * 999)], 2, 1_999);
        let t = twap_x32(&v, 1_000, 100, 0, 2_000, 900).unwrap();
        assert!(t < 105, "t={t}");
        assert!(!within_deviation(1_000, t, 500));
        assert!(within_deviation(t, t, 0));
    }
}
