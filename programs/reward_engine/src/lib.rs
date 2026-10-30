//! eBTC Solana NFT reward engine.
//!
//! Tier-weighted reward draws over a Metaplex Core NFT collection, paid in SPL eBTC
//! from a PDA-controlled reward vault, with a request/settle lifecycle, holder
//! double-entry bonus, dynamic 7-year emissions, and NFT-sale liquidity routing.
//!
//! See `solana_ebtc_technical_specification.md` for the full design.

use anchor_lang::prelude::*;

pub mod constants;
pub mod errors;
pub mod events;
pub mod instructions;
pub mod math;
pub mod merkle;
pub mod mpl;
pub mod randomness;
pub mod raydium;
pub mod selection;
pub mod state;

use instructions::*;
use state::{LiquidityMode, RandomnessSource};

// Default build = mainnet id. `--features devnet` (scripts/deploy-devnet.sh) swaps in the
// throwaway devnet id so the mainnet keypair is never deployed to devnet.
#[cfg(not(feature = "devnet"))]
declare_id!("7g83deWxasLDaUywxCGyFynTSPAZs62dJ7ir3ApmoTHc");
#[cfg(feature = "devnet")]
declare_id!("DQuhT2RMZGMzcmJq2e6xh8ji1bWgEFiVAo7wi87YmpR3");

#[program]
pub mod reward_engine {
    use super::*;

    // ---------- Admin / setup ----------
    pub fn initialize_config(ctx: Context<InitializeConfig>, args: admin::InitConfigArgs) -> Result<()> {
        admin::initialize_config(ctx, args)
    }
    pub fn initialize_collection_state(ctx: Context<InitializeCollectionState>) -> Result<()> {
        admin::initialize_collection_state(ctx)
    }
    pub fn initialize_tier(ctx: Context<InitializeTier>, args: admin::InitTierArgs) -> Result<()> {
        admin::initialize_tier(ctx, args)
    }
    pub fn set_paused(ctx: Context<AdminConfig>, paused: bool) -> Result<()> {
        admin::set_paused(ctx, paused)
    }
    pub fn set_draw_interval(ctx: Context<AdminConfig>, seconds: i64) -> Result<()> {
        admin::set_draw_interval(ctx, seconds)
    }
    pub fn set_settlement_delay(ctx: Context<AdminConfig>, seconds: i64) -> Result<()> {
        admin::set_settlement_delay(ctx, seconds)
    }
    pub fn set_max_reward_per_draw(ctx: Context<AdminConfig>, amount: u64) -> Result<()> {
        admin::set_max_reward_per_draw(ctx, amount)
    }
    pub fn set_keeper(ctx: Context<AdminConfig>, new_keeper: Pubkey) -> Result<()> {
        admin::set_keeper(ctx, new_keeper)
    }
    pub fn set_randomness_config(
        ctx: Context<AdminConfig>,
        mode: RandomnessSource,
        vrf_authority: Pubkey,
    ) -> Result<()> {
        admin::set_randomness_config(ctx, mode, vrf_authority)
    }
    pub fn set_oracle_queue(ctx: Context<AdminConfig>, queue: Pubkey) -> Result<()> {
        admin::set_oracle_queue(ctx, queue)
    }
    pub fn set_migration_authority(ctx: Context<AdminConfig>, authority: Pubkey) -> Result<()> {
        admin::set_migration_authority(ctx, authority)
    }
    pub fn set_emission_start(ctx: Context<AdminConfig>, start_ts: i64) -> Result<()> {
        admin::set_emission_start(ctx, start_ts)
    }
    pub fn set_liquidity_mode(ctx: Context<AdminConfig>, mode: LiquidityMode) -> Result<()> {
        admin::set_liquidity_mode(ctx, mode)
    }
    pub fn set_liquidity_config(ctx: Context<AdminConfig>, args: admin::LiquidityConfigArgs) -> Result<()> {
        admin::set_liquidity_config(ctx, args)
    }
    pub fn set_tier_price(ctx: Context<AdminTier>, tier_id: u8, price_lamports: u64) -> Result<()> {
        admin::set_tier_price(ctx, tier_id, price_lamports)
    }
    pub fn set_tier_active(ctx: Context<AdminTier>, tier_id: u8, active: bool) -> Result<()> {
        admin::set_tier_active(ctx, tier_id, active)
    }
    pub fn set_tier_metadata_root(
        ctx: Context<AdminTier>,
        tier_id: u8,
        metadata_root: [u8; 32],
    ) -> Result<()> {
        admin::set_tier_metadata_root(ctx, tier_id, metadata_root)
    }
    pub fn propose_admin(ctx: Context<AdminConfig>, new_admin: Pubkey) -> Result<()> {
        admin::propose_admin(ctx, new_admin)
    }
    pub fn accept_admin(ctx: Context<AcceptAdmin>) -> Result<()> {
        admin::accept_admin(ctx)
    }

    // ---------- Vault funding ----------
    pub fn fund_ebtc_vault(ctx: Context<FundEbtcVault>, amount: u64) -> Result<()> {
        vault::fund_ebtc_vault(ctx, amount)
    }
    pub fn fund_liquidity_sol(ctx: Context<FundLiquiditySol>, lamports: u64) -> Result<()> {
        vault::fund_liquidity_sol(ctx, lamports)
    }

    // ---------- NFT / registry ----------
    pub fn mint_nft(ctx: Context<MintNft>, args: nft::MintNftArgs) -> Result<()> {
        nft::mint_nft(ctx, args)
    }
    pub fn register_existing_asset(ctx: Context<RegisterExistingAsset>, args: nft::RegisterAssetArgs) -> Result<()> {
        nft::register_existing_asset(ctx, args)
    }
    pub fn migrate_mint(ctx: Context<MigrateMint>, args: nft::MigrateMintArgs) -> Result<()> {
        nft::migrate_mint(ctx, args)
    }
    pub fn update_asset_metadata(
        ctx: Context<UpdateAssetMetadata>,
        token_id: u32,
        name: String,
        uri: String,
    ) -> Result<()> {
        nft::update_asset_metadata(ctx, token_id, name, uri)
    }
    pub fn update_collection_metadata(ctx: Context<UpdateCollectionMetadata>, name: String, uri: String) -> Result<()> {
        nft::update_collection_metadata(ctx, name, uri)
    }
    pub fn set_token_active(ctx: Context<SetTokenActive>, active: bool) -> Result<()> {
        nft::set_token_active(ctx, active)
    }
    pub fn reactivate_token(ctx: Context<ReactivateToken>, page: u16) -> Result<()> {
        nft::reactivate_token(ctx, page)
    }

    // ---------- Draws ----------
    pub fn request_draw(ctx: Context<RequestDraw>, draw_id: u64) -> Result<()> {
        draw::request_draw(ctx, draw_id)
    }
    pub fn request_draw_with_bonus(ctx: Context<RequestDrawWithBonus>, draw_id: u64, token_id: u32) -> Result<()> {
        draw::request_draw_with_bonus(ctx, draw_id, token_id)
    }
    pub fn fulfill_draw_slot_hash(ctx: Context<FulfillDrawSlotHash>) -> Result<()> {
        draw::fulfill_draw_slot_hash(ctx)
    }
    pub fn fulfill_draw_vrf(ctx: Context<FulfillDrawVrf>, randomness: [u8; 32]) -> Result<()> {
        draw::fulfill_draw_vrf(ctx, randomness)
    }
    pub fn commit_draw_switchboard(ctx: Context<CommitDrawSwitchboard>) -> Result<()> {
        draw::commit_draw_switchboard(ctx)
    }
    pub fn fulfill_draw_switchboard(ctx: Context<FulfillDrawSwitchboard>) -> Result<()> {
        draw::fulfill_draw_switchboard(ctx)
    }
    pub fn commit_draw_orao(ctx: Context<CommitDrawOrao>) -> Result<()> {
        draw::commit_draw_orao(ctx)
    }
    pub fn fulfill_draw_orao(ctx: Context<FulfillDrawOrao>) -> Result<()> {
        draw::fulfill_draw_orao(ctx)
    }
    pub fn commit_draw_magicblock(ctx: Context<CommitDrawMagicBlock>) -> Result<()> {
        draw::commit_draw_magicblock(ctx)
    }
    pub fn magicblock_callback(ctx: Context<MagicBlockCallback>, randomness: [u8; 32], commit_slot: u64) -> Result<()> {
        draw::magicblock_callback(ctx, randomness, commit_slot)
    }
    pub fn settle_draw(ctx: Context<SettleDraw>) -> Result<()> {
        draw::settle_draw(ctx)
    }
    pub fn cancel_stale_draw(ctx: Context<CancelStaleDraw>) -> Result<()> {
        draw::cancel_stale_draw(ctx)
    }
    pub fn close_settled_draw(ctx: Context<CloseSettledDraw>) -> Result<()> {
        draw::close_settled_draw(ctx)
    }
    pub fn credit_claimable(ctx: Context<CreditClaimable>, amount: u64) -> Result<()> {
        draw::credit_claimable(ctx, amount)
    }
    pub fn claim_reward(ctx: Context<ClaimReward>) -> Result<()> {
        draw::claim_reward(ctx)
    }

    // ---------- Liquidity routing ----------
    pub fn record_sale_route(ctx: Context<RecordSaleRoute>, buyer: Pubkey, sale_lamports: u64) -> Result<()> {
        liquidity::record_sale_route(ctx, buyer, sale_lamports)
    }
    pub fn mark_route_failed(ctx: Context<UpdateRoute>, reason_code: u16) -> Result<()> {
        liquidity::mark_route_failed(ctx, reason_code)
    }
    pub fn retry_route(ctx: Context<UpdateRoute>) -> Result<()> {
        liquidity::retry_route(ctx)
    }
    pub fn seed_pool(ctx: Context<SeedPool>, args: liquidity::SeedPoolArgs) -> Result<()> {
        liquidity::seed_pool(ctx, args)
    }
    pub fn execute_route(ctx: Context<ExecuteRoute>) -> Result<()> {
        liquidity::execute_route(ctx)
    }
}
