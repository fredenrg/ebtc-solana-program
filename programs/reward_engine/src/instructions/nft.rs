//! NFT minting, legacy registration, and active-state management. See spec §3.3.
//!
//! `mint_nft` creates the Metaplex Core asset via CPI (`CreateV2`), with the config PDA
//! acting as the collection's update authority, so the asset is minted directly into the
//! project collection and owned by the buyer. It then charges the sale, registers the
//! asset, and appends it to the tier bucket. The Core collection must be created
//! beforehand with `update_authority = config PDA` (see scripts/setup.ts).

use crate::constants::*;
use crate::errors::RewardError;
use crate::events::*;
use crate::merkle;
use crate::mpl::assert_in_collection;
use crate::selection::index_to_page_slot;
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer as sol_transfer, Transfer as SolTransfer};
use mpl_core::{
    instructions::{CreateV2CpiBuilder, UpdateCollectionV1CpiBuilder, UpdateV1CpiBuilder},
    ID as MPL_CORE_ID,
};

/// `(ops, liquidity)` lamports of a sale; ops is rounded down so liquidity never loses a lamport.
pub fn split_sale(price_lamports: u64) -> Result<(u64, u64)> {
    let ops = (price_lamports as u128 * OPS_FEE_BPS as u128 / BPS_DENOMINATOR as u128) as u64;
    let liquidity = price_lamports.checked_sub(ops).ok_or(RewardError::MathOverflow)?;
    Ok((ops, liquidity))
}

fn write_display_code(dst: &mut [u8; DISPLAY_CODE_LEN], src: &str) -> Result<()> {
    let bytes = src.as_bytes();
    require!(bytes.len() <= DISPLAY_CODE_LEN, RewardError::InvalidDisplayCode);
    *dst = [0u8; DISPLAY_CODE_LEN];
    dst[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct MintNftArgs {
    pub token_id: u32,
    pub page: u16,
    pub display_code: String,
    pub name: String,
    pub uri: String,
    /// Merkle proof of `(token_id, display_code, name, uri)` against `tier_config.metadata_root`.
    pub proof: Vec<[u8; 32]>,
}

#[derive(Accounts)]
#[instruction(args: MintNftArgs)]
pub struct MintNft<'info> {
    #[account(mut)]
    pub buyer: Signer<'info>,

    #[account(
        mut,
        seeds = [SEED_CONFIG],
        bump = config.bump,
        has_one = keeper @ RewardError::AccountMismatch,
        constraint = !config.paused @ RewardError::Paused
    )]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [SEED_TIER, &[tier_config.tier_id]], bump = tier_config.bump)]
    pub tier_config: Account<'info, TierConfig>,

    #[account(mut, seeds = [SEED_COLLECTION], bump = collection_state.bump)]
    pub collection_state: Account<'info, CollectionState>,

    #[account(
        init,
        payer = buyer,
        space = 8 + TokenRegistry::INIT_SPACE,
        seeds = [SEED_TOKEN, &args.token_id.to_le_bytes()],
        bump
    )]
    pub token_registry: Account<'info, TokenRegistry>,

    #[account(
        init_if_needed,
        payer = buyer,
        space = 8 + TierBucket::INIT_SPACE,
        seeds = [SEED_BUCKET, &[tier_config.tier_id], &args.page.to_le_bytes()],
        bump
    )]
    pub tier_bucket: Account<'info, TierBucket>,

    /// CHECK: the new Metaplex Core asset (a fresh keypair signer); created via CPI.
    #[account(mut)]
    pub asset: Signer<'info>,

    /// CHECK: the Metaplex Core collection; must match config.collection. Mutated by CPI.
    #[account(mut, address = config.collection @ RewardError::AccountMismatch)]
    pub collection: UncheckedAccount<'info>,

    /// CHECK: system-owned SOL vault PDA receiving the liquidity share of the sale.
    #[account(mut, seeds = [SEED_LIQUIDITY_SOL], bump = config.liquidity_sol_bump)]
    pub liquidity_sol_vault: UncheckedAccount<'info>,

    /// CHECK: `config.keeper` (enforced by `has_one`); receives `OPS_FEE_BPS` of the sale.
    #[account(mut)]
    pub keeper: UncheckedAccount<'info>,

    #[account(
        init,
        payer = buyer,
        space = 8 + LiquidityRoute::INIT_SPACE,
        seeds = [SEED_ROUTE, &config.route_counter.to_le_bytes()],
        bump
    )]
    pub liquidity_route: Account<'info, LiquidityRoute>,

    /// CHECK: Metaplex Core program.
    #[account(address = MPL_CORE_ID @ RewardError::AccountMismatch)]
    pub mpl_core_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn mint_nft(ctx: Context<MintNft>, args: MintNftArgs) -> Result<()> {
    // ---- validations (immutable reads) ----
    {
        let tier = &ctx.accounts.tier_config;
        require!(tier.active, RewardError::TierInactive);
        require!(tier.minted < tier.max_supply, RewardError::TierSupplyExhausted);
        // public mints use ids above the migration range
        require!(
            args.token_id > tier.migration_max_id
                && args.token_id >= tier.min_token_id
                && args.token_id < tier.min_token_id + tier.max_supply,
            RewardError::TokenIdOutOfTierRange
        );
        require!(tier.metadata_root != [0u8; 32], RewardError::MetadataRootNotSet);
        require!(
            args.proof.len() <= MAX_METADATA_PROOF_LEN
                && merkle::verify(
                    &args.proof,
                    &tier.metadata_root,
                    merkle::leaf_hash(args.token_id, &args.display_code, &args.name, &args.uri),
                ),
            RewardError::InvalidMetadataProof
        );
    }

    // page must match the next active index
    let new_index = ctx.accounts.tier_config.active_count;
    let (expected_page, slot) = index_to_page_slot(new_index);
    require!(args.page == expected_page, RewardError::InvalidBucketPage);

    // ---- create the Core asset into the collection (config PDA is the collection
    //      update authority and signs the CPI) ----
    let config_bump = ctx.accounts.config.bump;
    let signer_seeds: &[&[&[u8]]] = &[&[SEED_CONFIG, &[config_bump]]];
    CreateV2CpiBuilder::new(&ctx.accounts.mpl_core_program.to_account_info())
        .asset(&ctx.accounts.asset.to_account_info())
        .collection(Some(&ctx.accounts.collection.to_account_info()))
        .authority(Some(&ctx.accounts.config.to_account_info()))
        .payer(&ctx.accounts.buyer.to_account_info())
        .owner(Some(&ctx.accounts.buyer.to_account_info()))
        .system_program(&ctx.accounts.system_program.to_account_info())
        .name(args.name.clone())
        .uri(args.uri.clone())
        .invoke_signed(signer_seeds)?;

    // ---- charge the sale: OPS_FEE_BPS to the keeper, the rest to the SOL liquidity vault ----
    let price_lamports = ctx.accounts.tier_config.price_lamports;
    let (ops_lamports, liquidity_lamports) = split_sale(price_lamports)?;
    let cpi = CpiContext::new(
        ctx.accounts.system_program.to_account_info(),
        SolTransfer {
            from: ctx.accounts.buyer.to_account_info(),
            to: ctx.accounts.liquidity_sol_vault.to_account_info(),
        },
    );
    sol_transfer(cpi, liquidity_lamports)?;
    if ops_lamports > 0 {
        let cpi = CpiContext::new(
            ctx.accounts.system_program.to_account_info(),
            SolTransfer {
                from: ctx.accounts.buyer.to_account_info(),
                to: ctx.accounts.keeper.to_account_info(),
            },
        );
        sol_transfer(cpi, ops_lamports)?;
    }

    let tier = &mut ctx.accounts.tier_config;

    // registry
    let reg = &mut ctx.accounts.token_registry;
    reg.token_id = args.token_id;
    reg.asset = ctx.accounts.asset.key();
    reg.tier_id = tier.tier_id;
    reg.serial = tier.next_serial;
    write_display_code(&mut reg.display_code, &args.display_code)?;
    reg.active = true;
    reg.bump = ctx.bumps.token_registry;

    // bucket append (contiguous active index)
    let bucket = &mut ctx.accounts.tier_bucket;
    if bucket.token_ids.is_empty() {
        bucket.tier_id = tier.tier_id;
        bucket.page = args.page;
        bucket.bump = ctx.bumps.tier_bucket;
    }
    require!((slot as usize) == bucket.token_ids.len(), RewardError::InvalidBucketPage);
    bucket.token_ids.push(args.token_id);
    bucket.len = bucket.token_ids.len() as u16;

    // counters / weight
    tier.minted += 1;
    tier.active_count += 1;
    tier.next_serial += 1;

    let cs = &mut ctx.accounts.collection_state;
    cs.total_minted += 1;
    cs.total_active += 1;
    cs.total_active_weight = cs
        .total_active_weight
        .checked_add(tier.chance_weight as u64)
        .ok_or(RewardError::MathOverflow)?;

    // liquidity route intent
    let route = &mut ctx.accounts.liquidity_route;
    route.route_id = ctx.accounts.config.route_counter;
    route.buyer = ctx.accounts.buyer.key();
    route.sale_lamports = liquidity_lamports;
    route.mode = ctx.accounts.config.liquidity_mode;
    route.target_pool = ctx.accounts.config.pool_address;
    route.status = RouteStatus::Pending;
    route.created_ts = Clock::get()?.unix_timestamp;
    route.executed_ts = None;
    route.failure_code = 0;
    route.retry_count = 0;
    route.bump = ctx.bumps.liquidity_route;

    ctx.accounts.config.route_counter += 1;

    emit!(NftMinted {
        token_id: args.token_id,
        asset: ctx.accounts.asset.key(),
        tier_id: tier.tier_id,
        owner: ctx.accounts.buyer.key(),
        price_lamports: tier.price_lamports,
    });
    emit!(SaleRouteCreated {
        route_id: route.route_id,
        buyer: route.buyer,
        sale_lamports: route.sale_lamports,
    });
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct RegisterAssetArgs {
    pub token_id: u32,
    pub tier_id: u8,
    pub serial: u32,
    pub page: u16,
    pub display_code: String,
    pub asset: Pubkey,
}

#[derive(Accounts)]
#[instruction(args: RegisterAssetArgs)]
pub struct RegisterExistingAsset<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [SEED_TIER, &[args.tier_id]], bump = tier_config.bump)]
    pub tier_config: Account<'info, TierConfig>,

    #[account(mut, seeds = [SEED_COLLECTION], bump = collection_state.bump)]
    pub collection_state: Account<'info, CollectionState>,

    #[account(
        init,
        payer = admin,
        space = 8 + TokenRegistry::INIT_SPACE,
        seeds = [SEED_TOKEN, &args.token_id.to_le_bytes()],
        bump
    )]
    pub token_registry: Account<'info, TokenRegistry>,

    #[account(
        init_if_needed,
        payer = admin,
        space = 8 + TierBucket::INIT_SPACE,
        seeds = [SEED_BUCKET, &[args.tier_id], &args.page.to_le_bytes()],
        bump
    )]
    pub tier_bucket: Account<'info, TierBucket>,

    /// CHECK: the legacy Core asset being registered; verified to be in the collection.
    #[account(address = args.asset @ RewardError::AccountMismatch)]
    pub asset: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn register_existing_asset(
    ctx: Context<RegisterExistingAsset>,
    args: RegisterAssetArgs,
) -> Result<()> {
    // the asset must be a real Core asset belonging to the project collection
    assert_in_collection(
        &ctx.accounts.asset.to_account_info(),
        &ctx.accounts.config.collection,
    )?;

    let tier = &mut ctx.accounts.tier_config;
    // legacy ids are within the migration range
    require!(
        args.token_id >= tier.min_token_id && args.token_id <= tier.migration_max_id,
        RewardError::TokenIdOutOfTierRange
    );

    let new_index = tier.active_count;
    let (expected_page, slot) = index_to_page_slot(new_index);
    require!(args.page == expected_page, RewardError::InvalidBucketPage);

    let reg = &mut ctx.accounts.token_registry;
    reg.token_id = args.token_id;
    reg.asset = args.asset;
    reg.tier_id = args.tier_id;
    reg.serial = args.serial;
    write_display_code(&mut reg.display_code, &args.display_code)?;
    reg.active = true;
    reg.bump = ctx.bumps.token_registry;

    let bucket = &mut ctx.accounts.tier_bucket;
    if bucket.token_ids.is_empty() {
        bucket.tier_id = args.tier_id;
        bucket.page = args.page;
        bucket.bump = ctx.bumps.tier_bucket;
    }
    require!((slot as usize) == bucket.token_ids.len(), RewardError::InvalidBucketPage);
    bucket.token_ids.push(args.token_id);
    bucket.len = bucket.token_ids.len() as u16;

    tier.minted += 1;
    tier.active_count += 1;

    let cs = &mut ctx.accounts.collection_state;
    cs.total_minted += 1;
    cs.total_active += 1;
    cs.total_active_weight = cs
        .total_active_weight
        .checked_add(tier.chance_weight as u64)
        .ok_or(RewardError::MathOverflow)?;

    emit!(AssetRegistered {
        token_id: args.token_id,
        asset: args.asset,
        tier_id: args.tier_id,
    });
    Ok(())
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct MigrateMintArgs {
    pub token_id: u32,
    pub tier_id: u8,
    pub serial: u32,
    pub page: u16,
    pub display_code: String,
    pub name: String,
    pub uri: String,
}

#[derive(Accounts)]
#[instruction(args: MigrateMintArgs)]
pub struct MigrateMint<'info> {
    /// `admin` OR the scoped `config.migration_authority` (checked in the handler). Pays rent.
    #[account(mut)]
    pub authority: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [SEED_TIER, &[args.tier_id]], bump = tier_config.bump)]
    pub tier_config: Account<'info, TierConfig>,

    #[account(mut, seeds = [SEED_COLLECTION], bump = collection_state.bump)]
    pub collection_state: Account<'info, CollectionState>,

    #[account(
        init,
        payer = authority,
        space = 8 + TokenRegistry::INIT_SPACE,
        seeds = [SEED_TOKEN, &args.token_id.to_le_bytes()],
        bump
    )]
    pub token_registry: Account<'info, TokenRegistry>,

    #[account(
        init_if_needed,
        payer = authority,
        space = 8 + TierBucket::INIT_SPACE,
        seeds = [SEED_BUCKET, &[args.tier_id], &args.page.to_le_bytes()],
        bump
    )]
    pub tier_bucket: Account<'info, TierBucket>,

    /// CHECK: the new Metaplex Core asset (a fresh keypair signer); created via CPI.
    #[account(mut)]
    pub asset: Signer<'info>,

    /// CHECK: the Metaplex Core collection; must match config.collection. Mutated by CPI.
    #[account(mut, address = config.collection @ RewardError::AccountMismatch)]
    pub collection: UncheckedAccount<'info>,

    /// CHECK: the migrant's wallet that will OWN the migrated asset (not a signer; admin pays).
    pub owner: UncheckedAccount<'info>,

    /// CHECK: Metaplex Core program.
    #[account(address = MPL_CORE_ID @ RewardError::AccountMismatch)]
    pub mpl_core_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

/// Admin-only legacy migration mint: creates the Core asset into the collection (config PDA
/// signs) OWNED BY THE MIGRANT, then registers it. Mirrors `register_existing_asset` but
/// CREATES the asset instead of requiring it to pre-exist, so the on-demand migration flow
/// (confirm surrendered Stellar NFT -> mint Solana NFT to the user) is one atomic call.
/// No sale/charge, no liquidity route, and no `paused` gate (migration runs during the
/// pre-activation window). Restricted to the legacy id range; `serial` is caller-provided
/// (the legacy serial) and the public-mint `next_serial` counter is intentionally untouched.
pub fn migrate_mint(ctx: Context<MigrateMint>, args: MigrateMintArgs) -> Result<()> {
    // authorization: admin OR the scoped migration_authority (when set to a non-default key)
    {
        let cfg = &ctx.accounts.config;
        let signer = ctx.accounts.authority.key();
        let is_admin = signer == cfg.admin;
        let is_migration_authority =
            cfg.migration_authority != Pubkey::default() && signer == cfg.migration_authority;
        require!(is_admin || is_migration_authority, RewardError::Unauthorized);
    }

    // legacy ids only + supply guard
    {
        let tier = &ctx.accounts.tier_config;
        require!(
            args.token_id >= tier.min_token_id && args.token_id <= tier.migration_max_id,
            RewardError::TokenIdOutOfTierRange
        );
        require!(tier.minted < tier.max_supply, RewardError::TierSupplyExhausted);
    }

    // page must match the next active index
    let new_index = ctx.accounts.tier_config.active_count;
    let (expected_page, slot) = index_to_page_slot(new_index);
    require!(args.page == expected_page, RewardError::InvalidBucketPage);

    // ---- create the Core asset into the collection, owned by the migrant ----
    let config_bump = ctx.accounts.config.bump;
    let signer_seeds: &[&[&[u8]]] = &[&[SEED_CONFIG, &[config_bump]]];
    CreateV2CpiBuilder::new(&ctx.accounts.mpl_core_program.to_account_info())
        .asset(&ctx.accounts.asset.to_account_info())
        .collection(Some(&ctx.accounts.collection.to_account_info()))
        .authority(Some(&ctx.accounts.config.to_account_info()))
        .payer(&ctx.accounts.authority.to_account_info())
        .owner(Some(&ctx.accounts.owner.to_account_info()))
        .system_program(&ctx.accounts.system_program.to_account_info())
        .name(args.name.clone())
        .uri(args.uri.clone())
        .invoke_signed(signer_seeds)?;

    // registry
    let reg = &mut ctx.accounts.token_registry;
    reg.token_id = args.token_id;
    reg.asset = ctx.accounts.asset.key();
    reg.tier_id = args.tier_id;
    reg.serial = args.serial;
    write_display_code(&mut reg.display_code, &args.display_code)?;
    reg.active = true;
    reg.bump = ctx.bumps.token_registry;

    // bucket append (contiguous active index)
    let bucket = &mut ctx.accounts.tier_bucket;
    if bucket.token_ids.is_empty() {
        bucket.tier_id = args.tier_id;
        bucket.page = args.page;
        bucket.bump = ctx.bumps.tier_bucket;
    }
    require!((slot as usize) == bucket.token_ids.len(), RewardError::InvalidBucketPage);
    bucket.token_ids.push(args.token_id);
    bucket.len = bucket.token_ids.len() as u16;

    // counters / weight
    let tier = &mut ctx.accounts.tier_config;
    tier.minted += 1;
    tier.active_count += 1;

    let cs = &mut ctx.accounts.collection_state;
    cs.total_minted += 1;
    cs.total_active += 1;
    cs.total_active_weight = cs
        .total_active_weight
        .checked_add(tier.chance_weight as u64)
        .ok_or(RewardError::MathOverflow)?;

    emit!(MigrationMinted {
        token_id: args.token_id,
        asset: ctx.accounts.asset.key(),
        tier_id: args.tier_id,
        owner: ctx.accounts.owner.key(),
    });
    Ok(())
}

/// Admin-only: rename a registered asset and point it at a new metadata URI (the config PDA
/// is the collection update authority, so only the program can sign the Core update). Owner
/// is untouched. Admin pays any realloc if the asset grows.
#[derive(Accounts)]
#[instruction(token_id: u32)]
pub struct UpdateAssetMetadata<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(seeds = [SEED_TOKEN, &token_id.to_le_bytes()], bump = token_registry.bump)]
    pub token_registry: Account<'info, TokenRegistry>,

    /// CHECK: the token's Core asset; must match the registry. Mutated by CPI.
    #[account(mut, address = token_registry.asset @ RewardError::AccountMismatch)]
    pub asset: UncheckedAccount<'info>,

    /// CHECK: the Metaplex Core collection; must match config.collection.
    #[account(address = config.collection @ RewardError::AccountMismatch)]
    pub collection: UncheckedAccount<'info>,

    /// CHECK: Metaplex Core program.
    #[account(address = MPL_CORE_ID @ RewardError::AccountMismatch)]
    pub mpl_core_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn update_asset_metadata(
    ctx: Context<UpdateAssetMetadata>,
    token_id: u32,
    name: String,
    uri: String,
) -> Result<()> {
    require!(!name.is_empty(), RewardError::InvalidMetadataName);
    require!(uri.starts_with("https://"), RewardError::InvalidMetadataUri);
    let config_bump = ctx.accounts.config.bump;
    let signer_seeds: &[&[&[u8]]] = &[&[SEED_CONFIG, &[config_bump]]];
    UpdateV1CpiBuilder::new(&ctx.accounts.mpl_core_program.to_account_info())
        .asset(&ctx.accounts.asset.to_account_info())
        .collection(Some(&ctx.accounts.collection.to_account_info()))
        .payer(&ctx.accounts.admin.to_account_info())
        .authority(Some(&ctx.accounts.config.to_account_info()))
        .system_program(&ctx.accounts.system_program.to_account_info())
        .new_name(name.clone())
        .new_uri(uri.clone())
        .invoke_signed(signer_seeds)?;

    emit!(AssetMetadataUpdated {
        token_id,
        asset: ctx.accounts.asset.key(),
        name,
        uri,
    });
    Ok(())
}

/// Admin-only: rename the Core collection and point it at a new metadata URI.
#[derive(Accounts)]
pub struct UpdateCollectionMetadata<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    /// CHECK: the Metaplex Core collection; must match config.collection. Mutated by CPI.
    #[account(mut, address = config.collection @ RewardError::AccountMismatch)]
    pub collection: UncheckedAccount<'info>,

    /// CHECK: Metaplex Core program.
    #[account(address = MPL_CORE_ID @ RewardError::AccountMismatch)]
    pub mpl_core_program: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn update_collection_metadata(
    ctx: Context<UpdateCollectionMetadata>,
    name: String,
    uri: String,
) -> Result<()> {
    require!(!name.is_empty(), RewardError::InvalidMetadataName);
    require!(uri.starts_with("https://"), RewardError::InvalidMetadataUri);
    let config_bump = ctx.accounts.config.bump;
    let signer_seeds: &[&[&[u8]]] = &[&[SEED_CONFIG, &[config_bump]]];
    UpdateCollectionV1CpiBuilder::new(&ctx.accounts.mpl_core_program.to_account_info())
        .collection(&ctx.accounts.collection.to_account_info())
        .payer(&ctx.accounts.admin.to_account_info())
        .authority(Some(&ctx.accounts.config.to_account_info()))
        .system_program(&ctx.accounts.system_program.to_account_info())
        .new_name(name.clone())
        .new_uri(uri.clone())
        .invoke_signed(signer_seeds)?;

    emit!(CollectionMetadataUpdated {
        collection: ctx.accounts.collection.key(),
        name,
        uri,
    });
    Ok(())
}

/// Deactivate a token via swap-remove with the last active token, keeping bucket
/// indices contiguous in `[0, active_count)`. Caller passes the page containing the
/// token and the current last active page (may be the same account).
/// Re-activation is the separate `reactivate_token` (it appends, so it needs a payer).
#[derive(Accounts)]
pub struct SetTokenActive<'info> {
    #[account(address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    // Deactivation does a swap-remove that mutates active-set indices, so it must not
    // run while a draw is pending (would invalidate the draw's snapshot mapping).
    #[account(
        seeds = [SEED_CONFIG],
        bump = config.bump,
        constraint = config.pending_draw_id.is_none() @ RewardError::DrawInProgress,
    )]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [SEED_TIER, &[token_registry.tier_id]], bump = tier_config.bump)]
    pub tier_config: Account<'info, TierConfig>,

    #[account(mut, seeds = [SEED_COLLECTION], bump = collection_state.bump)]
    pub collection_state: Account<'info, CollectionState>,

    #[account(mut, seeds = [SEED_TOKEN, &token_registry.token_id.to_le_bytes()], bump = token_registry.bump)]
    pub token_registry: Account<'info, TokenRegistry>,

    /// Page that currently contains the token being deactivated.
    #[account(mut, seeds = [SEED_BUCKET, &[token_registry.tier_id], &token_page.page.to_le_bytes()], bump = token_page.bump)]
    pub token_page: Account<'info, TierBucket>,

    /// Current last active page in this tier.
    #[account(mut, seeds = [SEED_BUCKET, &[token_registry.tier_id], &last_page.page.to_le_bytes()], bump = last_page.bump)]
    pub last_page: Account<'info, TierBucket>,
}

pub fn set_token_active(ctx: Context<SetTokenActive>, active: bool) -> Result<()> {
    // Deactivation only; `reactivate_token` is the inverse.
    require!(!active, RewardError::InvalidTierId);

    let reg = &mut ctx.accounts.token_registry;
    require!(reg.active, RewardError::TokenInactive);

    // swap-remove from token_page using the tail of last_page
    let token_id = reg.token_id;

    // pop tail from last_page
    let moved = {
        let lp = &mut ctx.accounts.last_page;
        let m = lp.token_ids.pop();
        lp.len = lp.token_ids.len() as u16;
        m
    };
    let moved = moved.ok_or(RewardError::InvalidBucketPage)?;

    // place moved into the slot held by token_id (search token_page)
    if ctx.accounts.token_page.key() == ctx.accounts.last_page.key() {
        // same page: moved came from this page; find token_id and overwrite
        let tp = &mut ctx.accounts.last_page;
        if let Some(pos) = tp.token_ids.iter().position(|&x| x == token_id) {
            // if token_id was the tail we just popped, nothing to place back
            if token_id != moved {
                tp.token_ids[pos] = moved;
            }
        } else if token_id != moved {
            return err!(RewardError::InvalidBucketPage);
        }
    } else {
        let tp = &mut ctx.accounts.token_page;
        let pos = tp
            .token_ids
            .iter()
            .position(|&x| x == token_id)
            .ok_or(RewardError::InvalidBucketPage)?;
        tp.token_ids[pos] = moved;
        tp.len = tp.token_ids.len() as u16;
    }

    reg.active = false;

    let tier = &mut ctx.accounts.tier_config;
    tier.active_count = tier.active_count.saturating_sub(1);

    let cs = &mut ctx.accounts.collection_state;
    cs.total_active = cs.total_active.saturating_sub(1);
    cs.total_active_weight = cs
        .total_active_weight
        .saturating_sub(tier.chance_weight as u64);

    emit!(TokenActiveChanged {
        token_id,
        active: false,
    });
    Ok(())
}

/// Re-activate a previously deactivated token: append it at the tail of its tier's active
/// set (index `active_count`, same contiguous layout as minting) and restore its weight.
/// The Core asset must still exist in the collection, or a later settle could not resolve
/// the winner's owner.
#[derive(Accounts)]
#[instruction(page: u16)]
pub struct ReactivateToken<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    // Appending mutates the active-set mapping a pending draw snapshotted.
    #[account(
        seeds = [SEED_CONFIG],
        bump = config.bump,
        constraint = config.pending_draw_id.is_none() @ RewardError::DrawInProgress,
    )]
    pub config: Account<'info, Config>,

    #[account(mut, seeds = [SEED_TIER, &[token_registry.tier_id]], bump = tier_config.bump)]
    pub tier_config: Account<'info, TierConfig>,

    #[account(mut, seeds = [SEED_COLLECTION], bump = collection_state.bump)]
    pub collection_state: Account<'info, CollectionState>,

    #[account(
        mut,
        seeds = [SEED_TOKEN, &token_registry.token_id.to_le_bytes()],
        bump = token_registry.bump,
        constraint = !token_registry.active @ RewardError::TokenAlreadyActive,
    )]
    pub token_registry: Account<'info, TokenRegistry>,

    /// Page holding index `tier_config.active_count` (created if it is a new page).
    #[account(
        init_if_needed,
        payer = admin,
        space = 8 + TierBucket::INIT_SPACE,
        seeds = [SEED_BUCKET, &[token_registry.tier_id], &page.to_le_bytes()],
        bump
    )]
    pub tier_bucket: Account<'info, TierBucket>,

    /// CHECK: the token's Core asset; must match the registry and still be in the collection.
    #[account(address = token_registry.asset @ RewardError::AccountMismatch)]
    pub asset: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn reactivate_token(ctx: Context<ReactivateToken>, page: u16) -> Result<()> {
    assert_in_collection(&ctx.accounts.asset.to_account_info(), &ctx.accounts.config.collection)?;

    let (expected_page, slot) = index_to_page_slot(ctx.accounts.tier_config.active_count);
    require!(page == expected_page, RewardError::InvalidBucketPage);

    let token_id = ctx.accounts.token_registry.token_id;
    let tier_id = ctx.accounts.token_registry.tier_id;

    let bucket = &mut ctx.accounts.tier_bucket;
    if bucket.token_ids.is_empty() {
        bucket.tier_id = tier_id;
        bucket.page = page;
        bucket.bump = ctx.bumps.tier_bucket;
    }
    require!((slot as usize) == bucket.token_ids.len(), RewardError::InvalidBucketPage);
    bucket.token_ids.push(token_id);
    bucket.len = bucket.token_ids.len() as u16;

    ctx.accounts.token_registry.active = true;

    let tier = &mut ctx.accounts.tier_config;
    tier.active_count += 1;

    let cs = &mut ctx.accounts.collection_state;
    cs.total_active += 1;
    cs.total_active_weight = cs
        .total_active_weight
        .checked_add(tier.chance_weight as u64)
        .ok_or(RewardError::MathOverflow)?;

    emit!(TokenActiveChanged {
        token_id,
        active: true,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_sale_tier_prices() {
        assert_eq!(split_sale(100_000_000).unwrap(), (5_000_000, 95_000_000));
        assert_eq!(split_sale(808_000_000).unwrap(), (40_400_000, 767_600_000));
        assert_eq!(split_sale(0).unwrap(), (0, 0));
    }

    #[test]
    fn split_sale_rounds_ops_down() {
        let (ops, liq) = split_sale(19).unwrap();
        assert_eq!((ops, liq), (0, 19));
        let (ops, liq) = split_sale(u64::MAX).unwrap();
        assert_eq!(ops + liq, u64::MAX);
    }
}
