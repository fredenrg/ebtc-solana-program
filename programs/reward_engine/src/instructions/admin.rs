//! Admin / setup instructions. See spec §3.1.

use crate::constants::*;
use crate::errors::RewardError;
use crate::events::*;
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token::{Mint, Token, TokenAccount};

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitConfigArgs {
    pub draw_interval_seconds: i64,
    pub settlement_delay_seconds: i64,
    pub emission_start_ts: i64,
    pub emission_end_ts: i64,
    pub max_reward_per_draw: u64,
    pub keeper: Pubkey,
    pub liquidity_mode: LiquidityMode,
    pub max_swap_to_ebtc_bps: u16,
    pub max_slippage_bps: u16,
    pub min_liquidity_pct_of_sale_bps: u16,
    pub max_price_deviation_bps: u16,
    pub twap_window_seconds: u32,
    pub randomness_mode: RandomnessSource,
    pub vrf_authority: Pubkey,
}

fn validate_liquidity_params(
    max_swap_to_ebtc_bps: u16,
    max_slippage_bps: u16,
    min_liquidity_pct_of_sale_bps: u16,
    max_price_deviation_bps: u16,
    twap_window_seconds: u32,
) -> Result<()> {
    require!(
        max_swap_to_ebtc_bps <= MAX_SWAP_TO_EBTC_BPS_LIMIT,
        RewardError::SwapBpsTooHigh
    );
    require!(
        max_slippage_bps <= MAX_SLIPPAGE_BPS_LIMIT
            && max_price_deviation_bps <= MAX_PRICE_DEVIATION_BPS_LIMIT
            && min_liquidity_pct_of_sale_bps as u64 <= BPS_DENOMINATOR
            && twap_window_seconds > 0
            && twap_window_seconds <= MAX_TWAP_WINDOW_SECONDS,
        RewardError::InvalidLiquidityConfig
    );
    Ok(())
}

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(mut)]
    pub admin: Signer<'info>,

    #[account(
        init,
        payer = admin,
        space = 8 + Config::INIT_SPACE,
        seeds = [SEED_CONFIG],
        bump
    )]
    pub config: Box<Account<'info, Config>>,

    pub ebtc_mint: Box<Account<'info, Mint>>,

    /// CHECK: Metaplex Core collection address; stored, not deserialized here.
    pub collection: UncheckedAccount<'info>,

    /// CHECK: PDA authority for the reward vault ATA (signer only, no data).
    #[account(seeds = [SEED_REWARD_VAULT], bump)]
    pub reward_vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = ebtc_mint,
        associated_token::authority = reward_vault_authority,
    )]
    pub reward_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: PDA authority for the liquidity eBTC ATA.
    #[account(seeds = [SEED_LIQUIDITY_VAULT], bump)]
    pub liquidity_vault_authority: UncheckedAccount<'info>,

    #[account(
        init,
        payer = admin,
        associated_token::mint = ebtc_mint,
        associated_token::authority = liquidity_vault_authority,
    )]
    pub liquidity_ebtc_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: system-owned PDA that holds sale SOL. Not initialized; funded by transfers.
    #[account(seeds = [SEED_LIQUIDITY_SOL], bump)]
    pub liquidity_sol_vault: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
}

pub fn initialize_config(ctx: Context<InitializeConfig>, args: InitConfigArgs) -> Result<()> {
    require!(
        args.emission_end_ts > args.emission_start_ts,
        RewardError::InvalidEmissionWindow
    );
    require!(args.draw_interval_seconds > 0, RewardError::InvalidInterval);
    require!(
        args.settlement_delay_seconds > 0
            && args.settlement_delay_seconds < args.draw_interval_seconds,
        RewardError::InvalidInterval
    );
    validate_liquidity_params(
        args.max_swap_to_ebtc_bps,
        args.max_slippage_bps,
        args.min_liquidity_pct_of_sale_bps,
        args.max_price_deviation_bps,
        args.twap_window_seconds,
    )?;

    let config = &mut ctx.accounts.config;
    config.admin = ctx.accounts.admin.key();
    config.pending_admin = None;
    config.ebtc_mint = ctx.accounts.ebtc_mint.key();
    config.collection = ctx.accounts.collection.key();
    config.keeper = args.keeper;

    config.reward_vault_auth_bump = ctx.bumps.reward_vault_authority;
    config.liquidity_vault_auth_bump = ctx.bumps.liquidity_vault_authority;
    config.liquidity_sol_bump = ctx.bumps.liquidity_sol_vault;
    config.bump = ctx.bumps.config;

    config.draw_interval_seconds = args.draw_interval_seconds;
    config.settlement_delay_seconds = args.settlement_delay_seconds;
    config.emission_start_ts = args.emission_start_ts;
    config.emission_end_ts = args.emission_end_ts;
    config.max_reward_per_draw = args.max_reward_per_draw;

    config.current_draw_id = 0;
    config.pending_draw_id = None;
    config.last_draw_ts = 0;

    config.liquidity_mode = args.liquidity_mode;
    config.max_swap_to_ebtc_bps = args.max_swap_to_ebtc_bps;
    config.max_slippage_bps = args.max_slippage_bps;
    config.min_liquidity_pct_of_sale_bps = args.min_liquidity_pct_of_sale_bps;
    config.max_price_deviation_bps = args.max_price_deviation_bps;
    config.twap_window_seconds = args.twap_window_seconds;
    // The pool is recorded by `seed_pool`, its only writer.
    config.pool_program = Pubkey::default();
    config.pool_address = Pubkey::default();
    config.pool_amm_config = Pubkey::default();
    config.pool_lp_mint = Pubkey::default();
    config.pool_observation = Pubkey::default();
    config.pool_seeded_ts = 0;
    config.pool_seed_price_x32 = 0;

    config.route_counter = 0;
    config.paused = false;
    config.config_version = 1;

    config.randomness_mode = args.randomness_mode;
    config.vrf_authority = args.vrf_authority;
    // The oracle queue is set later (only needed by queue-based providers) via
    // `set_oracle_queue`, so InitConfigArgs stays unchanged.
    config.oracle_queue = Pubkey::default();
    // Migration authority is set later (only needed during the migration window) via
    // `set_migration_authority`, so InitConfigArgs stays unchanged.
    config.migration_authority = Pubkey::default();

    emit!(ConfigInitialized {
        admin: config.admin,
        ebtc_mint: config.ebtc_mint,
        collection: config.collection,
        config_version: config.config_version,
    });
    Ok(())
}

#[derive(Accounts)]
pub struct InitializeCollectionState<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        init,
        payer = admin,
        space = 8 + CollectionState::INIT_SPACE,
        seeds = [SEED_COLLECTION],
        bump
    )]
    pub collection_state: Account<'info, CollectionState>,

    pub system_program: Program<'info, System>,
}

pub fn initialize_collection_state(ctx: Context<InitializeCollectionState>) -> Result<()> {
    let cs = &mut ctx.accounts.collection_state;
    cs.total_minted = 0;
    cs.total_active = 0;
    cs.total_active_weight = 0;
    cs.bump = ctx.bumps.collection_state;
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct InitTierArgs {
    pub tier_id: u8,
    pub max_supply: u32,
    pub chance_weight: u32,
    pub reward_multiplier_bps: u32,
    pub price_lamports: u64,
    pub min_token_id: u32,
    pub migration_max_id: u32,
}

#[derive(Accounts)]
#[instruction(args: InitTierArgs)]
pub struct InitializeTier<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        init,
        payer = admin,
        space = 8 + TierConfig::INIT_SPACE,
        seeds = [SEED_TIER, &[args.tier_id]],
        bump
    )]
    pub tier_config: Account<'info, TierConfig>,

    pub system_program: Program<'info, System>,
}

pub fn initialize_tier(ctx: Context<InitializeTier>, args: InitTierArgs) -> Result<()> {
    require!((args.tier_id as usize) < TIER_COUNT, RewardError::InvalidTierId);
    require!(args.chance_weight > 0, RewardError::ZeroTotalWeight);
    require!(args.reward_multiplier_bps > 0, RewardError::InvalidTierId);

    let t = &mut ctx.accounts.tier_config;
    t.tier_id = args.tier_id;
    t.max_supply = args.max_supply;
    t.minted = 0;
    t.active_count = 0;
    t.chance_weight = args.chance_weight;
    t.reward_multiplier_bps = args.reward_multiplier_bps;
    t.price_lamports = args.price_lamports;
    t.min_token_id = args.min_token_id;
    t.migration_max_id = args.migration_max_id;
    // new public mints start after the migration range
    t.next_serial = args.migration_max_id + 1;
    // Public mint stays closed until the admin commits a metadata root and opens the tier.
    t.active = false;
    t.metadata_root = [0u8; 32];
    t.bump = ctx.bumps.tier_config;

    emit!(TierInitialized {
        tier_id: t.tier_id,
        max_supply: t.max_supply,
        chance_weight: t.chance_weight,
        reward_multiplier_bps: t.reward_multiplier_bps,
        price_lamports: t.price_lamports,
    });
    Ok(())
}

// ---- Simple admin setters ----

#[derive(Accounts)]
pub struct AdminConfig<'info> {
    #[account(address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,
    #[account(mut, seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
}

pub fn set_paused(ctx: Context<AdminConfig>, paused: bool) -> Result<()> {
    ctx.accounts.config.paused = paused;
    emit!(PausedChanged { paused });
    Ok(())
}

fn bump_version(config: &mut Config, field_code: u16) {
    config.config_version = config.config_version.saturating_add(1);
    emit!(ConfigChanged {
        config_version: config.config_version,
        field_code,
    });
}

pub fn set_draw_interval(ctx: Context<AdminConfig>, seconds: i64) -> Result<()> {
    require!(seconds > 0, RewardError::InvalidInterval);
    let c = &mut ctx.accounts.config;
    require!(
        c.settlement_delay_seconds < seconds,
        RewardError::InvalidInterval
    );
    c.draw_interval_seconds = seconds;
    bump_version(c, 1);
    Ok(())
}

pub fn set_settlement_delay(ctx: Context<AdminConfig>, seconds: i64) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(
        seconds > 0 && seconds < c.draw_interval_seconds,
        RewardError::InvalidInterval
    );
    c.settlement_delay_seconds = seconds;
    bump_version(c, 2);
    Ok(())
}

pub fn set_max_reward_per_draw(ctx: Context<AdminConfig>, amount: u64) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.max_reward_per_draw = amount;
    bump_version(c, 3);
    Ok(())
}

pub fn set_keeper(ctx: Context<AdminConfig>, new_keeper: Pubkey) -> Result<()> {
    ctx.accounts.config.keeper = new_keeper;
    emit!(ConfigChanged {
        config_version: ctx.accounts.config.config_version,
        field_code: 4,
    });
    Ok(())
}

/// Switch randomness backend and/or rotate the VRF authority. Bumps config_version so any
/// in-flight pending draw (which locked the old version) becomes cancellable.
pub fn set_randomness_config(
    ctx: Context<AdminConfig>,
    mode: RandomnessSource,
    vrf_authority: Pubkey,
) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.randomness_mode = mode;
    c.vrf_authority = vrf_authority;
    bump_version(c, 7);
    Ok(())
}

/// Set the oracle queue for queue-based providers (`MagicBlock` requests go to it; retired
/// `Switchboard` randomness accounts had to belong to it).
/// Bumps config_version so any in-flight pending draw becomes cancellable.
pub fn set_oracle_queue(ctx: Context<AdminConfig>, queue: Pubkey) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.oracle_queue = queue;
    bump_version(c, 8);
    Ok(())
}

/// Set the scoped authority allowed to call `migrate_mint` (in addition to `admin`).
/// Lets an automated migration minter run a hot key without full admin power.
/// `Pubkey::default()` disables the extra authority (only `admin` may migrate).
pub fn set_migration_authority(ctx: Context<AdminConfig>, authority: Pubkey) -> Result<()> {
    let c = &mut ctx.accounts.config;
    c.migration_authority = authority;
    bump_version(c, 9);
    Ok(())
}

/// Move the emission window so it opens at `start_ts` (≥ now), keeping its duration. Only
/// before the first draw, so draws begin after the migration window and the emission
/// period is counted from activation rather than from `initialize_config`.
pub fn set_emission_start(ctx: Context<AdminConfig>, start_ts: i64) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(
        c.current_draw_id == 0 && c.pending_draw_id.is_none(),
        RewardError::EmissionAlreadyStarted
    );
    let now = Clock::get()?.unix_timestamp;
    require!(start_ts >= now, RewardError::InvalidEmissionWindow);
    let duration = c.emission_end_ts - c.emission_start_ts;
    c.emission_start_ts = start_ts;
    c.emission_end_ts = start_ts
        .checked_add(duration)
        .ok_or(RewardError::InvalidEmissionWindow)?;
    bump_version(c, 10);
    Ok(())
}

/// Extend the emission window to `end_ts`. The end can only move later, so a running schedule
/// can be stretched but never shortened. Leaves config_version alone so an in-flight draw still
/// settles (its reward is computed at settlement from the new end).
#[derive(Accounts)]
pub struct SetCommitRevealHead<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,
    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        init_if_needed,
        payer = admin,
        space = 8 + CommitRevealState::INIT_SPACE,
        seeds = [SEED_COMMIT_REVEAL],
        bump,
    )]
    pub commit_reveal: Account<'info, CommitRevealState>,
    pub system_program: Program<'info, System>,
}

/// Publish the head of a fresh hash chain. Refused while a draw is pending, so a head can
/// never be swapped after that draw's target slot hash is known.
pub fn set_commit_reveal_head(ctx: Context<SetCommitRevealHead>, head: [u8; 32]) -> Result<()> {
    require!(ctx.accounts.config.pending_draw_id.is_none(), RewardError::PendingDrawExists);
    let cr = &mut ctx.accounts.commit_reveal;
    cr.head = head;
    cr.bump = ctx.bumps.commit_reveal;
    emit!(ConfigChanged {
        config_version: ctx.accounts.config.config_version,
        field_code: 12,
    });
    Ok(())
}

pub fn set_emission_end(ctx: Context<AdminConfig>, end_ts: i64) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(end_ts > c.emission_end_ts, RewardError::InvalidEmissionWindow);
    c.emission_end_ts = end_ts;
    emit!(ConfigChanged {
        config_version: c.config_version,
        field_code: 11,
    });
    Ok(())
}

/// Routing caps only. The pool itself is fixed by `seed_pool` and cannot be changed here.
#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct LiquidityConfigArgs {
    pub max_swap_to_ebtc_bps: u16,
    pub max_slippage_bps: u16,
    pub min_liquidity_pct_of_sale_bps: u16,
    pub max_price_deviation_bps: u16,
    pub twap_window_seconds: u32,
}

pub fn set_liquidity_mode(ctx: Context<AdminConfig>, mode: LiquidityMode) -> Result<()> {
    ctx.accounts.config.liquidity_mode = mode;
    bump_version(&mut ctx.accounts.config, 5);
    Ok(())
}

pub fn set_liquidity_config(ctx: Context<AdminConfig>, args: LiquidityConfigArgs) -> Result<()> {
    validate_liquidity_params(
        args.max_swap_to_ebtc_bps,
        args.max_slippage_bps,
        args.min_liquidity_pct_of_sale_bps,
        args.max_price_deviation_bps,
        args.twap_window_seconds,
    )?;
    let c = &mut ctx.accounts.config;
    c.max_swap_to_ebtc_bps = args.max_swap_to_ebtc_bps;
    c.max_slippage_bps = args.max_slippage_bps;
    c.min_liquidity_pct_of_sale_bps = args.min_liquidity_pct_of_sale_bps;
    c.max_price_deviation_bps = args.max_price_deviation_bps;
    c.twap_window_seconds = args.twap_window_seconds;
    bump_version(c, 6);
    Ok(())
}

// ---- Tier setters ----

#[derive(Accounts)]
#[instruction(tier_id: u8)]
pub struct AdminTier<'info> {
    #[account(address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,
    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [SEED_TIER, &[tier_id]], bump = tier_config.bump)]
    pub tier_config: Account<'info, TierConfig>,
}

pub fn set_tier_price(ctx: Context<AdminTier>, _tier_id: u8, price_lamports: u64) -> Result<()> {
    ctx.accounts.tier_config.price_lamports = price_lamports;
    Ok(())
}

pub fn set_tier_active(ctx: Context<AdminTier>, _tier_id: u8, active: bool) -> Result<()> {
    let t = &mut ctx.accounts.tier_config;
    if active {
        require!(t.metadata_root != [0u8; 32], RewardError::MetadataRootNotSet);
    }
    t.active = active;
    Ok(())
}

pub fn set_tier_metadata_root(
    ctx: Context<AdminTier>,
    _tier_id: u8,
    metadata_root: [u8; 32],
) -> Result<()> {
    require!(metadata_root != [0u8; 32], RewardError::MetadataRootNotSet);
    ctx.accounts.tier_config.metadata_root = metadata_root;
    Ok(())
}

// ---- Two-step admin transfer ----

pub fn propose_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
    ctx.accounts.config.pending_admin = Some(new_admin);
    Ok(())
}

#[derive(Accounts)]
pub struct AcceptAdmin<'info> {
    pub pending_admin: Signer<'info>,
    #[account(mut, seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
}

pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
    let c = &mut ctx.accounts.config;
    require!(
        c.pending_admin == Some(ctx.accounts.pending_admin.key()),
        RewardError::Unauthorized
    );
    c.admin = ctx.accounts.pending_admin.key();
    c.pending_admin = None;
    Ok(())
}
