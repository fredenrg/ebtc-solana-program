use anchor_lang::prelude::*;

#[event]
pub struct ConfigInitialized {
    pub admin: Pubkey,
    pub ebtc_mint: Pubkey,
    pub collection: Pubkey,
    pub config_version: u64,
}

#[event]
pub struct TierInitialized {
    pub tier_id: u8,
    pub max_supply: u32,
    pub chance_weight: u32,
    pub reward_multiplier_bps: u32,
    pub price_lamports: u64,
}

#[event]
pub struct NftMinted {
    pub token_id: u32,
    pub asset: Pubkey,
    pub tier_id: u8,
    pub owner: Pubkey,
    pub price_lamports: u64,
}

#[event]
pub struct AssetRegistered {
    pub token_id: u32,
    pub asset: Pubkey,
    pub tier_id: u8,
}

/// Emitted when a legacy NFT is migrated: the Core asset is created into the collection and
/// registered to the migrant in a single admin instruction (no sale, no charge).
#[event]
pub struct MigrationMinted {
    pub token_id: u32,
    pub asset: Pubkey,
    pub tier_id: u8,
    pub owner: Pubkey,
}

/// A registered asset's name and metadata URI were changed by the admin.
#[event]
pub struct AssetMetadataUpdated {
    pub token_id: u32,
    pub asset: Pubkey,
    pub name: String,
    pub uri: String,
}

/// The collection's name and metadata URI were changed by the admin.
#[event]
pub struct CollectionMetadataUpdated {
    pub collection: Pubkey,
    pub name: String,
    pub uri: String,
}

#[event]
pub struct TokenActiveChanged {
    pub token_id: u32,
    pub active: bool,
}

#[event]
pub struct DrawRequested {
    pub draw_id: u64,
    pub request_ts: i64,
    pub total_weight: u64,
    pub eligible_supply: u32,
}

#[event]
pub struct DrawRequestedWithBonus {
    pub draw_id: u64,
    pub bonus_token_id: u32,
    pub bonus_caller: Pubkey,
    pub total_weight: u64,
}

/// Emitted when a randomness account is bound to a draw (Switchboard / ORAO commit phase).
#[event]
pub struct RandomnessCommitted {
    pub draw_id: u64,
    pub randomness_account: Pubkey,
    /// Switchboard: the committed round's `seed_slot`. ORAO: the request slot.
    pub commit_slot: u64,
}

#[event]
pub struct RandomnessFulfilled {
    pub draw_id: u64,
    /// 0 = SlotHash (dev), 1 = Vrf (trusted authority), 2 = Switchboard, 3 = Orao, 4 = MagicBlock.
    pub source_code: u8,
    pub slot: u64,
}

#[event]
pub struct DrawSettled {
    pub draw_id: u64,
    pub winner_token_id: u32,
    pub winner_owner: Pubkey,
    pub reward_amount: u64,
    pub settlement_slot: u64,
    pub pushed: bool, // true = direct transfer, false = credited as claimable
}

#[event]
pub struct DrawCancelled {
    pub draw_id: u64,
    pub reason_code: u16,
}

#[event]
pub struct RewardClaimed {
    pub owner: Pubkey,
    pub amount: u64,
}

#[event]
pub struct SaleRouteCreated {
    pub route_id: u64,
    pub buyer: Pubkey,
    pub sale_lamports: u64,
}

#[event]
pub struct RouteStatusChanged {
    pub route_id: u64,
    pub status_code: u8,
    pub reason_code: u16,
}

#[event]
pub struct PoolSeeded {
    pub pool: Pubkey,
    pub lp_mint: Pubkey,
    pub sol_amount: u64,
    pub ebtc_amount: u64,
    pub lp_amount: u64,
}

#[event]
pub struct LiquidityAdded {
    pub route_id: u64,
    pub sale_lamports: u64,
    pub swapped_lamports: u64,
    pub ebtc_bought: u64,
    pub sol_deposited: u64,
    pub ebtc_deposited: u64,
    pub lp_minted: u64,
}

#[event]
pub struct PausedChanged {
    pub paused: bool,
}

#[event]
pub struct ConfigChanged {
    pub config_version: u64,
    pub field_code: u16,
}
