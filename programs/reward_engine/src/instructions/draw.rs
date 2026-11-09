//! Draw lifecycle: request / request_with_bonus / settle / cancel / claim. See spec §3.4, §4–§7.

use crate::constants::*;
use crate::errors::RewardError;
use crate::events::*;
use crate::math::compute_reward;
use crate::mpl::{assert_in_collection, read_asset_owner, verify_asset};
use crate::randomness::{commit_reveal, derive_seed, recent_slot_hash, slot_hash_randomness};
use crate::selection::{
    index_to_page_slot, random_in_range, select_winner, total_weight, BonusEntry, Selection,
    TierWeight,
};
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::sysvar::slot_hashes;
use anchor_spl::token::{
    transfer as spl_transfer, Mint, Token, TokenAccount, Transfer as SplTransfer,
};

// ---------------- request_draw ----------------

#[derive(Accounts)]
#[instruction(draw_id: u64)]
pub struct RequestDraw<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_CONFIG],
        bump = config.bump,
        has_one = ebtc_mint @ RewardError::AccountMismatch,
        constraint = !config.paused @ RewardError::Paused,
    )]
    pub config: Account<'info, Config>,

    /// Read to enforce that the eBTC mint authority is revoked before any draw.
    pub ebtc_mint: Box<Account<'info, Mint>>,

    #[account(seeds = [SEED_COLLECTION], bump = collection_state.bump)]
    pub collection_state: Account<'info, CollectionState>,

    #[account(
        init,
        payer = payer,
        space = 8 + DrawState::INIT_SPACE,
        seeds = [SEED_DRAW, &draw_id.to_le_bytes()],
        bump
    )]
    pub draw_state: Account<'info, DrawState>,

    #[account(constraint = reward_vault.mint == config.ebtc_mint @ RewardError::AccountMismatch)]
    pub reward_vault: Box<Account<'info, TokenAccount>>,

    // all five tiers (id order 0..4) — snapshotted into the draw at request time
    #[account(seeds = [SEED_TIER, &[0u8]], bump = tier0.bump)]
    pub tier0: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[1u8]], bump = tier1.bump)]
    pub tier1: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[2u8]], bump = tier2.bump)]
    pub tier2: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[3u8]], bump = tier3.bump)]
    pub tier3: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[4u8]], bump = tier4.bump)]
    pub tier4: Account<'info, TierConfig>,

    pub system_program: Program<'info, System>,
}

/// Build the [TierWeight; 5] array from the five tier accounts (chance weights are
/// immutable post-init; only active_count varies and is what we snapshot).
fn tiers_from_accounts(
    t0: &TierConfig,
    t1: &TierConfig,
    t2: &TierConfig,
    t3: &TierConfig,
    t4: &TierConfig,
) -> [TierWeight; 5] {
    [
        TierWeight { active_count: t0.active_count, chance_weight: t0.chance_weight },
        TierWeight { active_count: t1.active_count, chance_weight: t1.chance_weight },
        TierWeight { active_count: t2.active_count, chance_weight: t2.chance_weight },
        TierWeight { active_count: t3.active_count, chance_weight: t3.chance_weight },
        TierWeight { active_count: t4.active_count, chance_weight: t4.chance_weight },
    ]
}

#[allow(clippy::too_many_arguments)]
fn open_draw(
    config: &mut Config,
    ebtc_mint: &Mint,
    collection_state: &CollectionState,
    tiers: &[TierWeight; 5],
    reward_vault_amount: u64,
    draw_id: u64,
    draw: &mut DrawState,
    draw_bump: u8,
    now: i64,
    slot: u64,
) -> Result<()> {
    require!(ebtc_mint.mint_authority.is_none(), RewardError::MintAuthorityNotRevoked);
    require!(now >= config.emission_start_ts, RewardError::EmissionNotStarted);
    require!(config.pending_draw_id.is_none(), RewardError::PendingDrawExists);
    require!(
        now >= config.last_draw_ts + config.draw_interval_seconds,
        RewardError::DrawIntervalNotElapsed
    );
    require!(collection_state.total_active > 0, RewardError::NoActiveTokens);
    require!(reward_vault_amount > 0, RewardError::InsufficientRewardVault);
    require!(draw_id == config.current_draw_id + 1, RewardError::AccountMismatch);

    // base (no-bonus) weight from the SNAPSHOT counts
    let base_weight = total_weight(tiers, None)?;
    require!(base_weight > 0, RewardError::ZeroTotalWeight);

    config.current_draw_id = draw_id;
    config.pending_draw_id = Some(draw_id);
    config.last_draw_ts = now;

    draw.draw_id = draw_id;
    draw.request_ts = now;
    draw.request_slot = slot;
    draw.settlement_slot = 0;
    draw.total_weight = base_weight;
    draw.eligible_supply = collection_state.total_active;
    draw.snapshot_tier_counts = [
        tiers[0].active_count,
        tiers[1].active_count,
        tiers[2].active_count,
        tiers[3].active_count,
        tiers[4].active_count,
    ];
    draw.bonus_token_id = None;
    draw.bonus_asset = None;
    draw.bonus_tier_id = 0;
    draw.bonus_weight = 0;
    draw.bonus_caller = None;
    draw.config_version = config.config_version;

    // randomness lifecycle: source locked from config; result pending until fulfilled
    draw.randomness_source = config.randomness_mode;
    draw.randomness_status = RandomnessStatus::Pending;
    draw.randomness = [0u8; 32];
    draw.randomness_slot = 0;
    draw.randomness_account = None;
    draw.randomness_commit_slot = 0;

    draw.winner_token_id = None;
    draw.winner_owner = None;
    draw.reward_amount = 0;
    draw.status = DrawStatus::Pending;
    draw.bump = draw_bump;
    Ok(())
}

pub fn request_draw(ctx: Context<RequestDraw>, draw_id: u64) -> Result<()> {
    let clock = Clock::get()?;
    let tiers = tiers_from_accounts(
        &ctx.accounts.tier0,
        &ctx.accounts.tier1,
        &ctx.accounts.tier2,
        &ctx.accounts.tier3,
        &ctx.accounts.tier4,
    );
    open_draw(
        &mut ctx.accounts.config,
        &ctx.accounts.ebtc_mint,
        &ctx.accounts.collection_state,
        &tiers,
        ctx.accounts.reward_vault.amount,
        draw_id,
        &mut ctx.accounts.draw_state,
        ctx.bumps.draw_state,
        clock.unix_timestamp,
        clock.slot,
    )?;

    let d = &ctx.accounts.draw_state;
    emit!(DrawRequested {
        draw_id: d.draw_id,
        request_ts: d.request_ts,
        total_weight: d.total_weight,
        eligible_supply: d.eligible_supply,
    });
    Ok(())
}

// ---------------- request_draw_with_bonus ----------------

#[derive(Accounts)]
#[instruction(draw_id: u64)]
pub struct RequestDrawWithBonus<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_CONFIG],
        bump = config.bump,
        has_one = ebtc_mint @ RewardError::AccountMismatch,
        constraint = !config.paused @ RewardError::Paused,
    )]
    pub config: Account<'info, Config>,

    /// Read to enforce that the eBTC mint authority is revoked before any draw.
    pub ebtc_mint: Box<Account<'info, Mint>>,

    #[account(seeds = [SEED_COLLECTION], bump = collection_state.bump)]
    pub collection_state: Account<'info, CollectionState>,

    #[account(
        init,
        payer = payer,
        space = 8 + DrawState::INIT_SPACE,
        seeds = [SEED_DRAW, &draw_id.to_le_bytes()],
        bump
    )]
    pub draw_state: Account<'info, DrawState>,

    #[account(constraint = reward_vault.mint == config.ebtc_mint @ RewardError::AccountMismatch)]
    pub reward_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        seeds = [SEED_TOKEN, &token_registry.token_id.to_le_bytes()],
        bump = token_registry.bump,
        constraint = token_registry.active @ RewardError::TokenInactive,
    )]
    pub token_registry: Account<'info, TokenRegistry>,

    /// CHECK: Metaplex Core asset, must be owned by payer.
    #[account(constraint = asset.key() == token_registry.asset @ RewardError::AccountMismatch)]
    pub asset: UncheckedAccount<'info>,

    // all five tiers (id order 0..4) — snapshotted into the draw at request time
    #[account(seeds = [SEED_TIER, &[0u8]], bump = tier0.bump)]
    pub tier0: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[1u8]], bump = tier1.bump)]
    pub tier1: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[2u8]], bump = tier2.bump)]
    pub tier2: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[3u8]], bump = tier3.bump)]
    pub tier3: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[4u8]], bump = tier4.bump)]
    pub tier4: Account<'info, TierConfig>,

    pub system_program: Program<'info, System>,
}

pub fn request_draw_with_bonus(
    ctx: Context<RequestDrawWithBonus>,
    draw_id: u64,
    _token_id: u32,
) -> Result<()> {
    // caller must currently own the NFT, and the asset must be in the project collection
    verify_asset(
        &ctx.accounts.asset.to_account_info(),
        &ctx.accounts.payer.key(),
        &ctx.accounts.config.collection,
    )?;

    let tiers = tiers_from_accounts(
        &ctx.accounts.tier0,
        &ctx.accounts.tier1,
        &ctx.accounts.tier2,
        &ctx.accounts.tier3,
        &ctx.accounts.tier4,
    );

    // bonus weight comes from the bonus token's tier (chance weight is immutable)
    let bonus_tier_id = ctx.accounts.token_registry.tier_id as usize;
    require!(bonus_tier_id < tiers.len(), RewardError::InvalidTierId);
    let bonus_weight = tiers[bonus_tier_id].chance_weight;

    let clock = Clock::get()?;
    open_draw(
        &mut ctx.accounts.config,
        &ctx.accounts.ebtc_mint,
        &ctx.accounts.collection_state,
        &tiers,
        ctx.accounts.reward_vault.amount,
        draw_id,
        &mut ctx.accounts.draw_state,
        ctx.bumps.draw_state,
        clock.unix_timestamp,
        clock.slot,
    )?;

    let d = &mut ctx.accounts.draw_state;
    d.bonus_token_id = Some(ctx.accounts.token_registry.token_id);
    d.bonus_asset = Some(ctx.accounts.token_registry.asset);
    d.bonus_tier_id = ctx.accounts.token_registry.tier_id;
    d.bonus_weight = bonus_weight;
    d.bonus_caller = Some(ctx.accounts.payer.key());
    d.total_weight = d
        .total_weight
        .checked_add(bonus_weight as u64)
        .ok_or(RewardError::MathOverflow)?;

    emit!(DrawRequestedWithBonus {
        draw_id: d.draw_id,
        bonus_token_id: ctx.accounts.token_registry.token_id,
        bonus_caller: ctx.accounts.payer.key(),
        total_weight: d.total_weight,
    });
    Ok(())
}

// ---------------- fulfill_draw_slot_hash (dev/local randomness) ----------------

/// Freeze slot-hash randomness for a pending draw (dev/local mode). Permissionless: the
/// entropy is the recent slot hash, not the caller. Enforces the settlement delay so the
/// slot hash used is not knowable at request time.
#[derive(Accounts)]
pub struct FulfillDrawSlotHash<'info> {
    pub caller: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    /// CHECK: SlotHashes sysvar, parsed manually for randomness.
    #[account(address = slot_hashes::ID)]
    pub slot_hashes: UncheckedAccount<'info>,
}

pub fn fulfill_draw_slot_hash(ctx: Context<FulfillDrawSlotHash>) -> Result<()> {
    require!(
        ctx.accounts.config.randomness_mode == RandomnessSource::SlotHash,
        RewardError::RandomnessModeMismatch
    );

    let d = &ctx.accounts.draw_state;
    require!(
        d.randomness_status == RandomnessStatus::Pending,
        RewardError::RandomnessAlreadyFulfilled
    );
    require!(
        d.config_version == ctx.accounts.config.config_version,
        RewardError::ConfigVersionMismatch
    );

    let clock = Clock::get()?;
    require!(
        clock.unix_timestamp >= d.request_ts + ctx.accounts.config.settlement_delay_seconds,
        RewardError::RandomnessNotReady
    );

    let slot_hash = recent_slot_hash(&ctx.accounts.slot_hashes.to_account_info())?;
    let randomness = slot_hash_randomness(&slot_hash, d.draw_id, d.request_slot, clock.slot);

    let d = &mut ctx.accounts.draw_state;
    d.randomness = randomness;
    d.randomness_slot = clock.slot;
    d.randomness_status = RandomnessStatus::Fulfilled;

    emit!(RandomnessFulfilled {
        draw_id: d.draw_id,
        source_code: 0,
        slot: clock.slot,
    });
    Ok(())
}

// ---------------- fulfill_draw_vrf (production randomness) ----------------

/// Deliver an external VRF result for a pending draw. v1 trusts `config.vrf_authority`
/// (oracle/relayer/VRF callback authority) to provide true VRF output. The forward path
/// to fully on-chain VRF is to replace the authority signature with VRF-account proof
/// verification here and copy that account's randomness into the draw.
#[derive(Accounts)]
pub struct FulfillDrawVrf<'info> {
    /// Must equal `config.vrf_authority`.
    pub vrf_authority: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,
}

pub fn fulfill_draw_vrf(ctx: Context<FulfillDrawVrf>, randomness: [u8; 32]) -> Result<()> {
    require!(
        ctx.accounts.config.randomness_mode == RandomnessSource::Vrf,
        RewardError::RandomnessModeMismatch
    );
    require_keys_eq!(
        ctx.accounts.vrf_authority.key(),
        ctx.accounts.config.vrf_authority,
        RewardError::Unauthorized
    );

    let d = &ctx.accounts.draw_state;
    require!(
        d.randomness_status == RandomnessStatus::Pending,
        RewardError::RandomnessAlreadyFulfilled
    );
    require!(
        d.config_version == ctx.accounts.config.config_version,
        RewardError::ConfigVersionMismatch
    );

    let clock = Clock::get()?;
    let d = &mut ctx.accounts.draw_state;
    d.randomness = randomness;
    d.randomness_slot = clock.slot;
    d.randomness_status = RandomnessStatus::Fulfilled;

    emit!(RandomnessFulfilled {
        draw_id: d.draw_id,
        source_code: 1,
        slot: clock.slot,
    });
    Ok(())
}

// ---------------- fulfill_draw_commit_reveal (hash-chain commit-reveal) ----------------

/// Reveal the next hash-chain secret for a pending commit-reveal draw. Permissionless: only
/// the holder of the chain can produce a matching preimage.
#[derive(Accounts)]
pub struct FulfillDrawCommitReveal<'info> {
    pub caller: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [SEED_COMMIT_REVEAL], bump = commit_reveal.bump)]
    pub commit_reveal: Account<'info, CommitRevealState>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    /// CHECK: address-constrained to the SlotHashes sysvar.
    #[account(address = slot_hashes::ID)]
    pub slot_hashes: UncheckedAccount<'info>,
}

pub fn fulfill_draw_commit_reveal(ctx: Context<FulfillDrawCommitReveal>, secret: [u8; 32]) -> Result<()> {
    let d = &ctx.accounts.draw_state;
    require!(
        d.randomness_source == RandomnessSource::CommitReveal,
        RewardError::RandomnessModeMismatch
    );
    require!(
        d.randomness_status == RandomnessStatus::Pending,
        RewardError::RandomnessAlreadyFulfilled
    );
    require!(
        d.config_version == ctx.accounts.config.config_version,
        RewardError::ConfigVersionMismatch
    );
    require!(
        commit_reveal::chain_hash(&secret) == ctx.accounts.commit_reveal.head,
        RewardError::InvalidReveal
    );

    let target = d.request_slot + COMMIT_REVEAL_TARGET_OFFSET_SLOTS;
    let (target_slot, target_hash) =
        commit_reveal::target_slot_hash(&ctx.accounts.slot_hashes.to_account_info(), target)?;
    let value = commit_reveal::draw_randomness(&secret, &target_hash, d.draw_id, d.request_slot);

    let cr = &mut ctx.accounts.commit_reveal;
    cr.head = secret;
    cr.reveals += 1;

    let d = &mut ctx.accounts.draw_state;
    d.randomness = value;
    d.randomness_slot = target_slot;
    d.randomness_status = RandomnessStatus::Fulfilled;

    emit!(RandomnessFulfilled {
        draw_id: d.draw_id,
        source_code: 5,
        slot: target_slot,
    });
    Ok(())
}

// ---------------- Switchboard On-Demand (commit -> reveal) ----------------

/// Bind a Switchboard On-Demand randomness account to a pending draw (commit phase).
/// Permissionless: anyone may run the crank, but the account is validated to be a real,
/// freshly-committed (unrevealed) Switchboard randomness account whose `seed_slot` is at or
/// after the draw's request slot. The bound account is the ONLY one reveal will accept.
#[derive(Accounts)]
pub struct CommitDrawSwitchboard<'info> {
    pub caller: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    /// CHECK: Switchboard On-Demand randomness account, parsed + validated in the handler.
    pub randomness_account: UncheckedAccount<'info>,
}

pub fn commit_draw_switchboard(ctx: Context<CommitDrawSwitchboard>) -> Result<()> {
    require!(
        ctx.accounts.config.randomness_mode == RandomnessSource::Switchboard,
        RewardError::RandomnessModeMismatch
    );
    {
        let d = &ctx.accounts.draw_state;
        require!(
            d.randomness_source == RandomnessSource::Switchboard,
            RewardError::RandomnessModeMismatch
        );
        require!(
            d.randomness_status == RandomnessStatus::Pending,
            RewardError::RandomnessAlreadyFulfilled
        );
        require!(d.randomness_account.is_none(), RewardError::RandomnessAlreadyCommitted);
        require!(
            d.config_version == ctx.accounts.config.config_version,
            RewardError::ConfigVersionMismatch
        );
    }

    let clock = Clock::get()?;
    let info = crate::randomness::switchboard::validate_commit(
        &ctx.accounts.randomness_account.to_account_info(),
        &ctx.accounts.config.oracle_queue,
        ctx.accounts.draw_state.request_slot,
        clock.slot,
    )?;

    let key = ctx.accounts.randomness_account.key();
    let d = &mut ctx.accounts.draw_state;
    d.randomness_account = Some(key);
    d.randomness_commit_slot = info.seed_slot;

    emit!(RandomnessCommitted {
        draw_id: d.draw_id,
        randomness_account: key,
        commit_slot: info.seed_slot,
    });
    Ok(())
}

/// Read the revealed Switchboard randomness for a committed draw (reveal phase) and freeze
/// it into the draw. Must reference the exact account bound at commit; settlement then
/// proceeds deterministically from the stored 32-byte value (same as every other backend).
#[derive(Accounts)]
pub struct FulfillDrawSwitchboard<'info> {
    pub caller: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    /// CHECK: Switchboard randomness account; must equal the one bound at commit.
    pub randomness_account: UncheckedAccount<'info>,
}

pub fn fulfill_draw_switchboard(ctx: Context<FulfillDrawSwitchboard>) -> Result<()> {
    require!(
        ctx.accounts.config.randomness_mode == RandomnessSource::Switchboard,
        RewardError::RandomnessModeMismatch
    );
    let (commit_slot, bound) = {
        let d = &ctx.accounts.draw_state;
        require!(
            d.randomness_source == RandomnessSource::Switchboard,
            RewardError::RandomnessModeMismatch
        );
        require!(
            d.randomness_status == RandomnessStatus::Pending,
            RewardError::RandomnessAlreadyFulfilled
        );
        require!(
            d.config_version == ctx.accounts.config.config_version,
            RewardError::ConfigVersionMismatch
        );
        let bound = d.randomness_account.ok_or(error!(RewardError::RandomnessNotCommitted))?;
        (d.randomness_commit_slot, bound)
    };
    require_keys_eq!(
        bound,
        ctx.accounts.randomness_account.key(),
        RewardError::RandomnessAccountMismatch
    );

    let clock = Clock::get()?;
    let value = crate::randomness::switchboard::read_revealed(
        &ctx.accounts.randomness_account.to_account_info(),
        commit_slot,
        clock.slot,
    )?;

    let d = &mut ctx.accounts.draw_state;
    d.randomness = value;
    d.randomness_slot = clock.slot;
    d.randomness_status = RandomnessStatus::Fulfilled;

    emit!(RandomnessFulfilled {
        draw_id: d.draw_id,
        source_code: 2,
        slot: clock.slot,
    });
    Ok(())
}

// ---------------- ORAO VRF v2 (request by CPI -> quorum fulfillment) ----------------

/// Request ORAO randomness for a pending draw. Permissionless; the caller pays ORAO's fee
/// plus the request account's rent. The seed is derived on-chain from the draw, so the
/// caller cannot choose it, and the request PDA it creates is bound as the only account
/// `fulfill_draw_orao` will accept.
#[derive(Accounts)]
pub struct CommitDrawOrao<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    /// CHECK: ORAO VRF program, pinned by address.
    #[account(address = crate::randomness::orao::PROGRAM_ID @ RewardError::InvalidOraoAccount)]
    pub orao_program: UncheckedAccount<'info>,

    /// CHECK: ORAO network state PDA, pinned by address; owner + layout checked in handler.
    #[account(mut, address = crate::randomness::orao::NETWORK_STATE @ RewardError::InvalidOraoAccount)]
    pub network_state: UncheckedAccount<'info>,

    /// CHECK: ORAO fee treasury; must equal `network_state.config.treasury`.
    #[account(mut)]
    pub treasury: UncheckedAccount<'info>,

    /// CHECK: ORAO request PDA for this draw's seed; created by the CPI.
    #[account(mut)]
    pub request: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn commit_draw_orao(ctx: Context<CommitDrawOrao>) -> Result<()> {
    use crate::randomness::orao;
    require!(
        ctx.accounts.config.randomness_mode == RandomnessSource::Orao,
        RewardError::RandomnessModeMismatch
    );
    let seed = {
        let d = &ctx.accounts.draw_state;
        require!(d.randomness_source == RandomnessSource::Orao, RewardError::RandomnessModeMismatch);
        require!(
            d.randomness_status == RandomnessStatus::Pending,
            RewardError::RandomnessAlreadyFulfilled
        );
        require!(d.randomness_account.is_none(), RewardError::RandomnessAlreadyCommitted);
        require!(
            d.config_version == ctx.accounts.config.config_version,
            RewardError::ConfigVersionMismatch
        );
        orao::request_seed(&crate::ID, d.draw_id, d.request_slot)
    };
    let request_key = orao::request_address(&seed);
    require_keys_eq!(ctx.accounts.request.key(), request_key, RewardError::RandomnessAccountMismatch);
    let treasury = orao::read_treasury(&ctx.accounts.network_state.to_account_info())?;
    require_keys_eq!(ctx.accounts.treasury.key(), treasury, RewardError::InvalidOraoAccount);

    orao::request_v2(
        &ctx.accounts.orao_program.to_account_info(),
        &ctx.accounts.caller.to_account_info(),
        &ctx.accounts.network_state.to_account_info(),
        &ctx.accounts.treasury.to_account_info(),
        &ctx.accounts.request.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        seed,
    )?;

    let slot = Clock::get()?.slot;
    let d = &mut ctx.accounts.draw_state;
    d.randomness_account = Some(request_key);
    d.randomness_commit_slot = slot;

    emit!(RandomnessCommitted {
        draw_id: d.draw_id,
        randomness_account: request_key,
        commit_slot: slot,
    });
    Ok(())
}

/// Freeze the ORAO-fulfilled randomness into a committed draw. Permissionless; reads only
/// the request account bound at commit, which must be ORAO-owned, fulfilled, and carry
/// this draw's seed.
#[derive(Accounts)]
pub struct FulfillDrawOrao<'info> {
    pub caller: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    /// CHECK: ORAO request account; must equal the one bound at commit.
    pub request: UncheckedAccount<'info>,
}

pub fn fulfill_draw_orao(ctx: Context<FulfillDrawOrao>) -> Result<()> {
    use crate::randomness::orao;
    require!(
        ctx.accounts.config.randomness_mode == RandomnessSource::Orao,
        RewardError::RandomnessModeMismatch
    );
    let seed = {
        let d = &ctx.accounts.draw_state;
        require!(d.randomness_source == RandomnessSource::Orao, RewardError::RandomnessModeMismatch);
        require!(
            d.randomness_status == RandomnessStatus::Pending,
            RewardError::RandomnessAlreadyFulfilled
        );
        require!(
            d.config_version == ctx.accounts.config.config_version,
            RewardError::ConfigVersionMismatch
        );
        let bound = d.randomness_account.ok_or(error!(RewardError::RandomnessNotCommitted))?;
        require_keys_eq!(bound, ctx.accounts.request.key(), RewardError::RandomnessAccountMismatch);
        orao::request_seed(&crate::ID, d.draw_id, d.request_slot)
    };
    let value = orao::read_fulfilled(&ctx.accounts.request.to_account_info(), &seed)?;

    let clock = Clock::get()?;
    let d = &mut ctx.accounts.draw_state;
    d.randomness = orao::draw_randomness(&value);
    d.randomness_slot = clock.slot;
    d.randomness_status = RandomnessStatus::Fulfilled;

    emit!(RandomnessFulfilled {
        draw_id: d.draw_id,
        source_code: 3,
        slot: clock.slot,
    });
    Ok(())
}

// ---------------- MagicBlock VRF (queued request -> proof-verified callback) ----------------

/// Queue a MagicBlock VRF request for a pending draw. Permissionless; the caller pays
/// MagicBlock's 0.0005 SOL fee. May be repeated `RECOMMIT_AFTER_SLOTS` after the previous
/// request if no callback arrived (MagicBlock expires unserved requests); the callback only
/// accepts the latest request.
#[derive(Accounts)]
pub struct CommitDrawMagicBlock<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(
        seeds = [SEED_CONFIG],
        bump = config.bump,
        constraint = !config.paused @ RewardError::Paused,
        constraint = config.oracle_queue != Pubkey::default() @ RewardError::OracleQueueNotSet,
    )]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    /// CHECK: this program's requester identity PDA; signs the request CPI.
    #[account(seeds = [crate::randomness::magicblock::IDENTITY_SEED], bump)]
    pub program_identity: UncheckedAccount<'info>,

    /// CHECK: MagicBlock oracle queue, pinned by `config.oracle_queue`; MagicBlock checks owner.
    #[account(mut, address = config.oracle_queue @ RewardError::InvalidMagicBlockAccount)]
    pub oracle_queue: UncheckedAccount<'info>,

    /// CHECK: MagicBlock VRF program, pinned by address.
    #[account(address = crate::randomness::magicblock::PROGRAM_ID @ RewardError::InvalidMagicBlockAccount)]
    pub vrf_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,

    /// CHECK: SlotHashes sysvar (MagicBlock mixes a slot hash into the request id).
    #[account(address = slot_hashes::ID @ RewardError::AccountMismatch)]
    pub slot_hashes: UncheckedAccount<'info>,
}

pub fn commit_draw_magicblock(ctx: Context<CommitDrawMagicBlock>) -> Result<()> {
    use crate::randomness::magicblock;
    use anchor_lang::Discriminator;
    require!(
        ctx.accounts.config.randomness_mode == RandomnessSource::MagicBlock,
        RewardError::RandomnessModeMismatch
    );
    let slot = Clock::get()?.slot;
    let (draw_id, request_slot) = {
        let d = &ctx.accounts.draw_state;
        require!(
            d.randomness_source == RandomnessSource::MagicBlock,
            RewardError::RandomnessModeMismatch
        );
        require!(
            d.randomness_status == RandomnessStatus::Pending,
            RewardError::RandomnessAlreadyFulfilled
        );
        require!(
            d.config_version == ctx.accounts.config.config_version,
            RewardError::ConfigVersionMismatch
        );
        if d.randomness_account.is_some() {
            require!(
                slot >= d.randomness_commit_slot.saturating_add(magicblock::RECOMMIT_AFTER_SLOTS),
                RewardError::RandomnessAlreadyCommitted
            );
        }
        (d.draw_id, d.request_slot)
    };

    let data = magicblock::request_data(
        &magicblock::caller_seed(&crate::ID, draw_id, request_slot, slot),
        &crate::ID,
        crate::instruction::MagicblockCallback::DISCRIMINATOR,
        &[
            (ctx.accounts.draw_state.key(), true),
            (ctx.accounts.config.key(), false),
        ],
        &slot.to_le_bytes(),
    );
    magicblock::request(
        &ctx.accounts.vrf_program.to_account_info(),
        &ctx.accounts.caller.to_account_info(),
        &ctx.accounts.program_identity.to_account_info(),
        ctx.bumps.program_identity,
        &ctx.accounts.oracle_queue.to_account_info(),
        &ctx.accounts.system_program.to_account_info(),
        &ctx.accounts.slot_hashes.to_account_info(),
        data,
    )?;

    let queue = ctx.accounts.oracle_queue.key();
    let d = &mut ctx.accounts.draw_state;
    d.randomness_account = Some(queue);
    d.randomness_commit_slot = slot;
    emit!(RandomnessCommitted {
        draw_id,
        randomness_account: queue,
        commit_slot: slot,
    });
    Ok(())
}

/// Invoked by the MagicBlock VRF program after it verified the oracle's proof. No pause or
/// status constraints on the accounts: a failing callback would fail the oracle's tx and
/// strand the request, so anything that is not the draw's latest request is a logged no-op.
#[derive(Accounts)]
pub struct MagicBlockCallback<'info> {
    /// MagicBlock's scoped identity PDA for this program; only MagicBlock can sign it.
    #[account(address = crate::randomness::magicblock::callback_identity(&crate::ID) @ RewardError::InvalidMagicBlockAccount)]
    pub vrf_identity: Signer<'info>,

    #[account(mut, seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()], bump = draw_state.bump)]
    pub draw_state: Account<'info, DrawState>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
}

pub fn magicblock_callback(ctx: Context<MagicBlockCallback>, randomness: [u8; 32], commit_slot: u64) -> Result<()> {
    use crate::randomness::magicblock;
    let d = &mut ctx.accounts.draw_state;
    let current = magicblock::is_current(
        d.status == DrawStatus::Pending,
        d.randomness_source == RandomnessSource::MagicBlock,
        d.randomness_status == RandomnessStatus::Pending,
        d.config_version == ctx.accounts.config.config_version,
        d.randomness_account.map(|_| d.randomness_commit_slot),
        commit_slot,
    );
    if !current {
        msg!("ignoring MagicBlock callback for draw {} (commit slot {})", d.draw_id, commit_slot);
        return Ok(());
    }
    let slot = Clock::get()?.slot;
    d.randomness = magicblock::draw_randomness(&randomness);
    d.randomness_slot = slot;
    d.randomness_status = RandomnessStatus::Fulfilled;
    emit!(RandomnessFulfilled {
        draw_id: d.draw_id,
        source_code: 4,
        slot,
    });
    Ok(())
}

// ---------------- settle_draw ----------------

#[derive(Accounts)]
pub struct SettleDraw<'info> {
    #[account(mut)]
    pub settler: Signer<'info>,

    #[account(mut, seeds = [SEED_CONFIG], bump = config.bump, constraint = !config.paused @ RewardError::Paused)]
    pub config: Box<Account<'info, Config>>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,

    pub collection_state: Account<'info, CollectionState>,

    // all five tiers in id order 0..4
    #[account(seeds = [SEED_TIER, &[0u8]], bump = tier0.bump)]
    pub tier0: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[1u8]], bump = tier1.bump)]
    pub tier1: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[2u8]], bump = tier2.bump)]
    pub tier2: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[3u8]], bump = tier3.bump)]
    pub tier3: Account<'info, TierConfig>,
    #[account(seeds = [SEED_TIER, &[4u8]], bump = tier4.bump)]
    pub tier4: Account<'info, TierConfig>,

    /// Bucket page that (per off-chain computation) contains the winning index.
    /// Verified on-chain against the derived selection.
    pub winner_bucket: Account<'info, TierBucket>,

    #[account(
        seeds = [SEED_TOKEN, &winner_token_registry.token_id.to_le_bytes()],
        bump = winner_token_registry.bump,
    )]
    pub winner_token_registry: Account<'info, TokenRegistry>,

    /// CHECK: winning Metaplex Core asset; current owner is read at settlement.
    #[account(constraint = winner_asset.key() == winner_token_registry.asset @ RewardError::AccountMismatch)]
    pub winner_asset: UncheckedAccount<'info>,

    #[account(
        mut,
        seeds = [SEED_REWARD_VAULT],
        bump = config.reward_vault_auth_bump,
    )]
    /// CHECK: reward vault PDA authority (signer for transfers).
    pub reward_vault_authority: UncheckedAccount<'info>,

    #[account(mut, constraint = reward_vault.mint == config.ebtc_mint @ RewardError::AccountMismatch)]
    pub reward_vault: Account<'info, TokenAccount>,

    /// Winner's eBTC token account (must belong to the current asset owner).
    #[account(mut, constraint = winner_token_account.mint == config.ebtc_mint @ RewardError::AccountMismatch)]
    pub winner_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn settle_draw(ctx: Context<SettleDraw>) -> Result<()> {
    let clock = Clock::get()?;
    let now = clock.unix_timestamp;

    {
        let d = &ctx.accounts.draw_state;
        // Randomness must be fixed before we can settle — no execution-slot dependence.
        require!(
            d.randomness_status == RandomnessStatus::Fulfilled,
            RewardError::RandomnessNotFulfilled
        );
        require!(
            d.config_version == ctx.accounts.config.config_version,
            RewardError::ConfigVersionMismatch
        );
    }

    // Build tier weight inputs from the SNAPSHOT counts frozen at request time.
    // chance_weight is immutable post-init, so reading it live from the tier accounts
    // is safe; only active_count can change, and we deliberately ignore the live value.
    let snap = ctx.accounts.draw_state.snapshot_tier_counts;
    let tiers = [
        TierWeight { active_count: snap[0], chance_weight: ctx.accounts.tier0.chance_weight },
        TierWeight { active_count: snap[1], chance_weight: ctx.accounts.tier1.chance_weight },
        TierWeight { active_count: snap[2], chance_weight: ctx.accounts.tier2.chance_weight },
        TierWeight { active_count: snap[3], chance_weight: ctx.accounts.tier3.chance_weight },
        TierWeight { active_count: snap[4], chance_weight: ctx.accounts.tier4.chance_weight },
    ];

    let bonus = if ctx.accounts.draw_state.bonus_token_id.is_some() {
        Some(BonusEntry {
            tier_id: ctx.accounts.draw_state.bonus_tier_id,
            weight: ctx.accounts.draw_state.bonus_weight,
        })
    } else {
        None
    };

    let tw = total_weight(&tiers, bonus)?;
    // Consistency guard: recomputed snapshot weight must equal the stored draw weight.
    require!(
        tw == ctx.accounts.draw_state.total_weight,
        RewardError::SnapshotWeightMismatch
    );

    // ---- randomness ----
    // Deterministic: derive purely from the FIXED randomness frozen at fulfillment.
    let settlement_slot = clock.slot;
    let seed = derive_seed(
        ctx.accounts.draw_state.draw_id,
        ctx.accounts.draw_state.request_slot,
        &ctx.accounts.draw_state.randomness,
        ctx.accounts.draw_state.config_version,
    );
    let r = random_in_range(&seed, tw)?;

    // ---- selection ----
    let selection = select_winner(r, &tiers, bonus)?;

    // resolve to a concrete token_id and verify the provided winner accounts match
    let (winner_tier_id, expected_token_id) = match selection {
        Selection::Normal { tier_id, index_in_tier } => {
            let (page, slot) = index_to_page_slot(index_in_tier);
            require!(ctx.accounts.winner_bucket.tier_id == tier_id, RewardError::InvalidBucketPage);
            require!(ctx.accounts.winner_bucket.page == page, RewardError::InvalidBucketPage);
            let tid = *ctx
                .accounts
                .winner_bucket
                .token_ids
                .get(slot as usize)
                .ok_or(RewardError::InvalidBucketPage)?;
            (tier_id, tid)
        }
        Selection::Bonus { tier_id } => {
            let tid = ctx
                .accounts
                .draw_state
                .bonus_token_id
                .ok_or(RewardError::WinnerVerificationFailed)?;
            (tier_id, tid)
        }
    };

    require!(
        ctx.accounts.winner_token_registry.token_id == expected_token_id,
        RewardError::WinnerVerificationFailed
    );
    require!(
        ctx.accounts.winner_token_registry.tier_id == winner_tier_id,
        RewardError::WinnerVerificationFailed
    );
    require!(ctx.accounts.winner_token_registry.active, RewardError::TokenInactive);

    // verify the winning asset is in the project collection
    assert_in_collection(
        &ctx.accounts.winner_asset.to_account_info(),
        &ctx.accounts.config.collection,
    )?;

    // verify the current owner and that the destination ATA belongs to them
    let current_owner = read_asset_owner(&ctx.accounts.winner_asset.to_account_info())?;
    require_keys_eq!(
        ctx.accounts.winner_token_account.owner,
        current_owner,
        RewardError::WinnerVerificationFailed
    );

    // ---- reward ----
    let multiplier_bps = match winner_tier_id {
        0 => ctx.accounts.tier0.reward_multiplier_bps,
        1 => ctx.accounts.tier1.reward_multiplier_bps,
        2 => ctx.accounts.tier2.reward_multiplier_bps,
        3 => ctx.accounts.tier3.reward_multiplier_bps,
        4 => ctx.accounts.tier4.reward_multiplier_bps,
        _ => return err!(RewardError::InvalidTierId),
    };

    let reward = compute_reward(
        now,
        ctx.accounts.config.emission_end_ts,
        ctx.accounts.config.draw_interval_seconds,
        ctx.accounts.reward_vault.amount,
        multiplier_bps,
        ctx.accounts.config.max_reward_per_draw,
    )?;
    require!(reward <= ctx.accounts.config.max_reward_per_draw, RewardError::RewardCapExceeded);
    require!(reward <= ctx.accounts.reward_vault.amount, RewardError::InsufficientRewardVault);

    // ---- payout (PDA-signed) ----
    let bump = ctx.accounts.config.reward_vault_auth_bump;
    let signer_seeds: &[&[&[u8]]] = &[&[SEED_REWARD_VAULT, &[bump]]];
    let cpi = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        SplTransfer {
            from: ctx.accounts.reward_vault.to_account_info(),
            to: ctx.accounts.winner_token_account.to_account_info(),
            authority: ctx.accounts.reward_vault_authority.to_account_info(),
        },
        signer_seeds,
    );
    spl_transfer(cpi, reward)?;

    // ---- finalize ----
    let d = &mut ctx.accounts.draw_state;
    d.settlement_slot = settlement_slot;
    d.winner_token_id = Some(expected_token_id);
    d.winner_owner = Some(current_owner);
    d.reward_amount = reward;
    d.status = DrawStatus::Settled;

    ctx.accounts.config.pending_draw_id = None;

    emit!(DrawSettled {
        draw_id: d.draw_id,
        winner_token_id: expected_token_id,
        winner_owner: current_owner,
        reward_amount: reward,
        settlement_slot,
        pushed: true,
    });
    Ok(())
}

// ---------------- cancel_stale_draw ----------------

#[derive(Accounts)]
pub struct CancelStaleDraw<'info> {
    pub caller: Signer<'info>,

    #[account(mut, seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status == DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,
}

pub fn cancel_stale_draw(ctx: Context<CancelStaleDraw>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let is_admin = ctx.accounts.caller.key() == ctx.accounts.config.admin;
    let version_changed =
        ctx.accounts.draw_state.config_version != ctx.accounts.config.config_version;
    let timed_out = now >= ctx.accounts.draw_state.request_ts + STALE_DRAW_TIMEOUT_SECONDS;
    let d = &ctx.accounts.draw_state;
    let reveal_expired = d.randomness_source == RandomnessSource::CommitReveal
        && d.randomness_status == RandomnessStatus::Pending
        && Clock::get()?.slot
            > d.request_slot + COMMIT_REVEAL_TARGET_OFFSET_SLOTS + SLOT_HASHES_MAX_ENTRIES;

    require!(
        is_admin || version_changed || timed_out || reveal_expired,
        RewardError::StaleTimeoutNotElapsed
    );

    let reason_code: u16 = if version_changed {
        1
    } else if timed_out {
        2
    } else if reveal_expired {
        3
    } else {
        0
    };
    ctx.accounts.draw_state.status = DrawStatus::Cancelled;
    ctx.accounts.config.pending_draw_id = None;

    emit!(DrawCancelled {
        draw_id: ctx.accounts.draw_state.draw_id,
        reason_code,
    });
    Ok(())
}

// ---------------- close_settled_draw (rent reclaim) ----------------

#[derive(Accounts)]
pub struct CloseSettledDraw<'info> {
    /// Receives the reclaimed rent (incentivizes cranking).
    #[account(mut)]
    pub recipient: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        close = recipient,
        seeds = [SEED_DRAW, &draw_state.draw_id.to_le_bytes()],
        bump = draw_state.bump,
        constraint = draw_state.status != DrawStatus::Pending @ RewardError::DrawNotPending,
    )]
    pub draw_state: Account<'info, DrawState>,
}

pub fn close_settled_draw(_ctx: Context<CloseSettledDraw>) -> Result<()> {
    // history is preserved via the emitted DrawSettled/DrawCancelled events
    Ok(())
}

// ---------------- claimable fallback ----------------

#[derive(Accounts)]
pub struct CreditClaimable<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    /// CHECK: owner the claim is credited to.
    pub owner: UncheckedAccount<'info>,

    #[account(
        init_if_needed,
        payer = admin,
        space = 8 + ClaimableReward::INIT_SPACE,
        seeds = [SEED_CLAIM, owner.key().as_ref(), config.ebtc_mint.as_ref()],
        bump
    )]
    pub claimable: Account<'info, ClaimableReward>,

    pub system_program: Program<'info, System>,
}

pub fn credit_claimable(ctx: Context<CreditClaimable>, amount: u64) -> Result<()> {
    let c = &mut ctx.accounts.claimable;
    if c.owner == Pubkey::default() {
        c.owner = ctx.accounts.owner.key();
        c.mint = ctx.accounts.config.ebtc_mint;
        c.bump = ctx.bumps.claimable;
    }
    c.amount = c.amount.checked_add(amount).ok_or(RewardError::MathOverflow)?;
    c.updated_ts = Clock::get()?.unix_timestamp;
    Ok(())
}

#[derive(Accounts)]
pub struct ClaimReward<'info> {
    #[account(mut)]
    pub owner: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [SEED_CLAIM, owner.key().as_ref(), config.ebtc_mint.as_ref()],
        bump = claimable.bump,
        constraint = claimable.owner == owner.key() @ RewardError::Unauthorized,
    )]
    pub claimable: Account<'info, ClaimableReward>,

    #[account(
        mut,
        seeds = [SEED_REWARD_VAULT],
        bump = config.reward_vault_auth_bump,
    )]
    /// CHECK: reward vault PDA authority.
    pub reward_vault_authority: UncheckedAccount<'info>,

    #[account(mut, constraint = reward_vault.mint == config.ebtc_mint @ RewardError::AccountMismatch)]
    pub reward_vault: Account<'info, TokenAccount>,

    #[account(
        mut,
        constraint = owner_token_account.mint == config.ebtc_mint @ RewardError::AccountMismatch,
        constraint = owner_token_account.owner == owner.key() @ RewardError::Unauthorized,
    )]
    pub owner_token_account: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn claim_reward(ctx: Context<ClaimReward>) -> Result<()> {
    let amount = ctx.accounts.claimable.amount;
    require!(amount > 0, RewardError::NothingToClaim);
    require!(amount <= ctx.accounts.reward_vault.amount, RewardError::InsufficientRewardVault);

    let bump = ctx.accounts.config.reward_vault_auth_bump;
    let signer_seeds: &[&[&[u8]]] = &[&[SEED_REWARD_VAULT, &[bump]]];
    let cpi = CpiContext::new_with_signer(
        ctx.accounts.token_program.to_account_info(),
        SplTransfer {
            from: ctx.accounts.reward_vault.to_account_info(),
            to: ctx.accounts.owner_token_account.to_account_info(),
            authority: ctx.accounts.reward_vault_authority.to_account_info(),
        },
        signer_seeds,
    );
    spl_transfer(cpi, amount)?;

    ctx.accounts.claimable.amount = 0;
    ctx.accounts.claimable.updated_ts = Clock::get()?.unix_timestamp;

    emit!(RewardClaimed {
        owner: ctx.accounts.owner.key(),
        amount,
    });
    Ok(())
}
