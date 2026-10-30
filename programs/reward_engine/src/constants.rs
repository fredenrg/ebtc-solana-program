//! Launch constants and PDA seeds. Amounts are in base units (6 decimals).

// ---- Token / supply (DECIDED) ----
pub const EBTC_DECIMALS: u8 = 6;
pub const ONE_EBTC: u64 = 1_000_000; // 10^6

pub const TOTAL_SUPPLY_EBTC: u64 = 21_000_000;
pub const TOTAL_SUPPLY: u64 = TOTAL_SUPPLY_EBTC * ONE_EBTC; // 21_000_000_000_000

pub const REWARD_VAULT_EBTC: u64 = 16_800_000; // FIXED
pub const REWARD_VAULT_BASE_UNITS: u64 = REWARD_VAULT_EBTC * ONE_EBTC; // 16_800_000_000_000

// ---- Draw schedule defaults (admin-configurable) ----
pub const DEFAULT_DRAW_INTERVAL_SECONDS: i64 = 600; // 10 minutes
pub const DEFAULT_SETTLEMENT_DELAY_SECONDS: i64 = 90; // between 60-120, pinned to 90
pub const TOTAL_DRAWS_7Y: u64 = 367_920;

// ---- Reward cap suggestion (admin-configurable) ----
pub const SUGGESTED_MAX_REWARD_PER_DRAW: u64 = 200 * ONE_EBTC; // 200 eBTC

// ---- Liquidity policy bounds ----
pub const MAX_SWAP_TO_EBTC_BPS_LIMIT: u16 = 4_900; // hard ceiling per design
pub const BPS_DENOMINATOR: u64 = 10_000;
/// Share of each public NFT sale paid to `config.keeper` to fund draws; the rest goes to
/// the liquidity vault. Changing it takes a program upgrade.
pub const OPS_FEE_BPS: u64 = 500;
pub const MAX_SLIPPAGE_BPS_LIMIT: u16 = 5_000;
pub const MAX_PRICE_DEVIATION_BPS_LIMIT: u16 = 5_000;
// Raydium writes an observation at most every 15s into a 100-slot ring, so a wrapped ring always
// spans >= 99 * 15 = 1485s. A longer window could look past the oldest slot.
pub const MAX_TWAP_WINDOW_SECONDS: u32 = 1_440;

// ---- Tiers ----
pub const TIER_COUNT: usize = 5;

// tier_id -> (chance_weight, reward_multiplier_bps)
// FX100, FX250, GFX500, GFX750, GFX808
pub const TIER_CHANCE_WEIGHTS: [u32; TIER_COUNT] = [10, 25, 36, 60, 78];
pub const TIER_MULTIPLIER_BPS: [u32; TIER_COUNT] = [10_000, 12_500, 15_000, 20_000, 30_000];

// ---- Legacy NFT id ranges + max supply (from design §6A) ----
// (min_token_id, migration_max_id, max_supply) per tier
pub const TIER_TOKEN_RANGES: [(u32, u32, u32); TIER_COUNT] = [
    (100_001, 100_032, 1_500), // FX100
    (250_001, 250_022, 1_000), // FX250
    (500_001, 500_022, 900),   // GFX500 / FX500
    (750_001, 750_040, 500),   // GFX750 / FX750
    (808_001, 808_050, 369),   // GFX808 / FX808
];

// ---- Bucket paging ----
pub const TIER_BUCKET_PAGE_SIZE: usize = 256;

// ---- Display code fixed width ----
pub const DISPLAY_CODE_LEN: usize = 16;

/// Upper bound on `mint_nft` metadata proof length (2^16 leaves per tier; real tiers need 11).
pub const MAX_METADATA_PROOF_LEN: usize = 16;

// ---- Liveness ----
pub const STALE_DRAW_TIMEOUT_SECONDS: i64 = 3_600; // anyone may cancel a stuck pending draw after 1h

// ---- PDA seeds ----
pub const SEED_CONFIG: &[u8] = b"config";
pub const SEED_REWARD_VAULT: &[u8] = b"reward_vault";
pub const SEED_LIQUIDITY_VAULT: &[u8] = b"liquidity_vault";
pub const SEED_LIQUIDITY_SOL: &[u8] = b"liquidity_sol";
pub const SEED_COLLECTION: &[u8] = b"collection";
pub const SEED_TIER: &[u8] = b"tier";
pub const SEED_BUCKET: &[u8] = b"bucket";
pub const SEED_TOKEN: &[u8] = b"token";
pub const SEED_DRAW: &[u8] = b"draw";
pub const SEED_USER: &[u8] = b"user";
pub const SEED_CLAIM: &[u8] = b"claim";
pub const SEED_ROUTE: &[u8] = b"route";
