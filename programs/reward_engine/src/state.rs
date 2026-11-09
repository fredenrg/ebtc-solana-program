//! On-chain account state. See spec §2.2. Sizes derived via #[derive(InitSpace)].

use crate::constants::{DISPLAY_CODE_LEN, TIER_BUCKET_PAGE_SIZE, TIER_COUNT};
use anchor_lang::prelude::*;

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum DrawStatus {
    Pending,
    Settled,
    Cancelled,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum LiquidityMode {
    TreasuryPair,
    SwapAndPair,
    Paused,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum RouteStatus {
    Pending,
    Executed,
    Failed,
}

/// Where a draw's randomness comes from. Locked into the draw at request time from
/// `Config::randomness_mode` so a config flip mid-draw can't change the source.
///
/// Variants are append-only: existing on-chain `Config`/`DrawState` accounts encode the
/// discriminant by index, so new providers (e.g. ORAO) must be added at the END.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum RandomnessSource {
    /// Dev/local: 32 bytes frozen from a recent SlotHashes entry at fulfillment.
    SlotHash,
    /// Provider-agnostic: 32 bytes delivered by a trusted external fulfiller
    /// (`vrf_authority`). Used for deterministic tests and as the generic VRF seam.
    Vrf,
    /// Retired (Switchboard ended support 2026-09-25): On-Demand commit→reveal.
    /// `commit_draw_switchboard` binds the randomness account, then
    /// `fulfill_draw_switchboard` reads the revealed value.
    Switchboard,
    /// Production: ORAO VRF v2. `commit_draw_orao` requests randomness by CPI with a
    /// program-derived seed, then `fulfill_draw_orao` reads the quorum-fulfilled value.
    Orao,
    /// Production: MagicBlock VRF. `commit_draw_magicblock` queues a request by CPI; the
    /// MagicBlock program verifies the oracle's proof and calls `magicblock_callback`.
    MagicBlock,
    /// Hash-chain commit-reveal: the keeper reveals the preimage of `CommitRevealState::head`,
    /// mixed with the first slot hash at/after `request_slot + COMMIT_REVEAL_TARGET_OFFSET_SLOTS`.
    CommitReveal,
}

/// Head of the keeper's pre-committed hash chain (`head = H(tag || next_secret)`). Each reveal
/// must hash to the current head and then becomes the new head, so secrets are fixed in order
/// long before the slot hashes they are mixed with exist.
#[account]
#[derive(InitSpace)]
pub struct CommitRevealState {
    pub head: [u8; 32],
    pub reveals: u64,
    pub bump: u8,
}

/// Two-phase randomness lifecycle for a draw. Settlement requires `Fulfilled`.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, Debug, InitSpace)]
pub enum RandomnessStatus {
    /// Requested; the 32-byte result is not yet available.
    Pending,
    /// A fixed 32-byte result is stored in `DrawState::randomness`.
    Fulfilled,
}

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,
    pub pending_admin: Option<Pubkey>,
    pub ebtc_mint: Pubkey,
    pub collection: Pubkey,
    pub keeper: Pubkey,

    pub reward_vault_auth_bump: u8,
    pub liquidity_vault_auth_bump: u8,
    pub liquidity_sol_bump: u8,
    pub bump: u8,

    pub draw_interval_seconds: i64,
    pub settlement_delay_seconds: i64,
    pub emission_start_ts: i64,
    pub emission_end_ts: i64,
    pub max_reward_per_draw: u64,

    pub current_draw_id: u64,
    pub pending_draw_id: Option<u64>,
    pub last_draw_ts: i64,

    pub liquidity_mode: LiquidityMode,
    pub max_swap_to_ebtc_bps: u16,
    /// Worst accepted execution of a route's swap vs the pool TWAP (price impact + fee).
    pub max_slippage_bps: u16,
    pub min_liquidity_pct_of_sale_bps: u16,
    /// Largest accepted gap between the pool's spot price and its TWAP before a route executes.
    pub max_price_deviation_bps: u16,
    /// TWAP lookback. Capped at `MAX_TWAP_WINDOW_SECONDS`, the shortest span the Raydium
    /// observation ring is guaranteed to cover.
    pub twap_window_seconds: u32,

    /// Raydium CPMM program, pool and pool accounts. All default until `seed_pool`, which is
    /// the only writer, so the pool cannot be swapped for another after launch.
    pub pool_program: Pubkey,
    pub pool_address: Pubkey,
    pub pool_amm_config: Pubkey,
    pub pool_lp_mint: Pubkey,
    pub pool_observation: Pubkey,
    pub pool_seeded_ts: i64,
    /// Opening price (eBTC base units per lamport, Q32). Stands in for the TWAP before the
    /// first swap fills the observation ring.
    pub pool_seed_price_x32: u128,

    pub route_counter: u64,

    pub paused: bool,
    pub config_version: u64,

    /// Active randomness backend for NEW draws. `SlotHash` dev/local, `Vrf` trusted
    /// fulfiller, `Orao` / `MagicBlock` production VRF (`Switchboard` retired).
    pub randomness_mode: RandomnessSource,
    /// Trusted fulfiller in `Vrf` mode (e.g. oracle/relayer or VRF callback authority).
    /// Ignored in `SlotHash` mode. May be `Pubkey::default()` until VRF is wired.
    pub vrf_authority: Pubkey,
    /// Oracle queue for queue-based providers, set via `set_oracle_queue`.
    /// `MagicBlock`: the VRF queue requests are sent to (required; default = commits refused).
    /// `Switchboard` (retired): the queue a randomness account must belong to (when non-default).
    pub oracle_queue: Pubkey,

    /// Scoped authority allowed to call `migrate_mint` (legacy NFT migration) in addition to
    /// `admin`. Lets an automated migration minter run a hot key WITHOUT full admin power.
    /// Set via `set_migration_authority`. `Pubkey::default()` = only `admin` may migrate.
    pub migration_authority: Pubkey,
}

#[account]
#[derive(InitSpace)]
pub struct CollectionState {
    pub total_minted: u32,
    pub total_active: u32,
    pub total_active_weight: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct TierConfig {
    pub tier_id: u8,
    pub max_supply: u32,
    pub minted: u32,
    pub active_count: u32,
    pub chance_weight: u32,
    pub reward_multiplier_bps: u32,
    pub price_lamports: u64,
    pub min_token_id: u32,
    pub migration_max_id: u32,
    pub next_serial: u32,
    pub active: bool,
    pub bump: u8,
    /// Merkle root over this tier's public-mint metadata leaves (see `merkle.rs`).
    /// All-zero until set; the tier cannot be activated without it.
    pub metadata_root: [u8; 32],
}

#[account]
#[derive(InitSpace)]
pub struct DrawState {
    pub draw_id: u64,
    pub request_ts: i64,
    pub request_slot: u64,
    pub settlement_slot: u64,
    pub total_weight: u64,
    pub eligible_supply: u32,
    /// Per-tier active_count frozen at request time. settle_draw uses ONLY these
    /// counts for weight + selection range, so mints/deactivations after request
    /// cannot change this draw's outcome. Indexed by tier_id (0..TIER_COUNT).
    pub snapshot_tier_counts: [u32; TIER_COUNT],
    pub bonus_token_id: Option<u32>,
    pub bonus_asset: Option<Pubkey>,
    pub bonus_tier_id: u8,
    pub bonus_weight: u32,
    pub bonus_caller: Option<Pubkey>,
    pub config_version: u64,

    /// Randomness backend locked at request time (copied from Config::randomness_mode).
    pub randomness_source: RandomnessSource,
    /// Lifecycle of this draw's randomness. settle_draw requires `Fulfilled`.
    pub randomness_status: RandomnessStatus,
    /// The fixed 32-byte randomness result used to derive the winner. Zero until fulfilled.
    pub randomness: [u8; 32],
    /// Slot at which randomness was fulfilled (informational / audit).
    pub randomness_slot: u64,
    /// Switchboard / Orao: the randomness (ORAO: request) account bound at the commit step.
    /// The fulfill step must read this exact account. MagicBlock: the oracle queue used.
    /// None for other sources.
    pub randomness_account: Option<Pubkey>,
    /// Switchboard: the account's `seed_slot` recorded at commit, verified unchanged at
    /// reveal. Orao: the slot the request was made (informational). MagicBlock: the slot of
    /// the latest request, echoed back by the callback so stale requests are ignored.
    pub randomness_commit_slot: u64,

    pub winner_token_id: Option<u32>,
    pub winner_owner: Option<Pubkey>,
    pub reward_amount: u64,
    pub status: DrawStatus,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct TokenRegistry {
    pub token_id: u32,
    pub asset: Pubkey,
    pub tier_id: u8,
    pub serial: u32,
    pub display_code: [u8; DISPLAY_CODE_LEN],
    pub active: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct ClaimableReward {
    pub owner: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
    pub updated_ts: i64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct LiquidityRoute {
    pub route_id: u64,
    pub buyer: Pubkey,
    pub sale_lamports: u64,
    pub mode: LiquidityMode,
    pub target_pool: Pubkey,
    pub status: RouteStatus,
    pub created_ts: i64,
    pub executed_ts: Option<i64>,
    pub failure_code: u16,
    pub retry_count: u8,
    pub bump: u8,
    /// Filled by `execute_route`. Deposits draw on the PDA's whole wSOL/eBTC balances, so
    /// `sol_deposited` can include dust left over from earlier routes.
    pub swapped_lamports: u64,
    pub ebtc_bought: u64,
    pub sol_deposited: u64,
    pub ebtc_deposited: u64,
    pub lp_minted: u64,
}

/// Paged active-token bucket for weighted selection without full-collection scans.
#[account]
#[derive(InitSpace)]
pub struct TierBucket {
    pub tier_id: u8,
    pub page: u16,
    pub len: u16,
    #[max_len(TIER_BUCKET_PAGE_SIZE)]
    pub token_ids: Vec<u32>,
    pub bump: u8,
}
