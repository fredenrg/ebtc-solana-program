use anchor_lang::prelude::*;

#[error_code]
pub enum RewardError {
    #[msg("Program is paused")]
    Paused,
    #[msg("Signer is not authorized for this action")]
    Unauthorized,
    #[msg("Draw interval has not elapsed yet")]
    DrawIntervalNotElapsed,
    #[msg("A pending draw already exists")]
    PendingDrawExists,
    #[msg("No pending draw exists")]
    NoPendingDraw,
    #[msg("Settlement delay has not elapsed yet")]
    SettlementDelayNotElapsed,
    #[msg("Draw is not in Pending status")]
    DrawNotPending,
    #[msg("Config version changed since the draw was requested")]
    ConfigVersionMismatch,
    #[msg("Reward vault has insufficient balance")]
    InsufficientRewardVault,
    #[msg("No active tokens are eligible")]
    NoActiveTokens,
    #[msg("Total active weight is zero")]
    ZeroTotalWeight,
    #[msg("Signer is not the current owner of the asset")]
    NotAssetOwner,
    #[msg("Token is not active")]
    TokenInactive,
    #[msg("Token is already registered")]
    TokenAlreadyRegistered,
    #[msg("token_id is outside the tier range")]
    TokenIdOutOfTierRange,
    #[msg("Tier is not active")]
    TierInactive,
    #[msg("Tier max supply exhausted")]
    TierSupplyExhausted,
    #[msg("Bonus token already set for this draw")]
    BonusAlreadySet,
    #[msg("Provided tier bucket page is invalid for the winner")]
    InvalidBucketPage,
    #[msg("Winner verification failed")]
    WinnerVerificationFailed,
    #[msg("Reward exceeds max reward per draw cap")]
    RewardCapExceeded,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Invalid liquidity configuration")]
    InvalidLiquidityConfig,
    #[msg("Swap bps exceeds the 4900 ceiling")]
    SwapBpsTooHigh,
    #[msg("Slippage limit exceeded")]
    SlippageExceeded,
    #[msg("Liquidity route is not in the expected state")]
    RouteNotInExpectedState,
    #[msg("Invalid emission window")]
    InvalidEmissionWindow,
    #[msg("Invalid interval / delay configuration")]
    InvalidInterval,
    #[msg("eBTC mint authority has not been revoked")]
    MintAuthorityNotRevoked,
    #[msg("Invalid tier id")]
    InvalidTierId,
    #[msg("Invalid display code length")]
    InvalidDisplayCode,
    #[msg("Stale draw timeout has not elapsed")]
    StaleTimeoutNotElapsed,
    #[msg("Nothing to claim")]
    NothingToClaim,
    #[msg("Account mismatch")]
    AccountMismatch,
    #[msg("Cannot modify the active set while a draw is pending")]
    DrawInProgress,
    #[msg("Snapshot total weight does not match the stored draw weight")]
    SnapshotWeightMismatch,
    #[msg("Draw randomness has not been fulfilled yet")]
    RandomnessNotFulfilled,
    #[msg("Draw randomness has already been fulfilled")]
    RandomnessAlreadyFulfilled,
    #[msg("Randomness source does not match the configured mode")]
    RandomnessModeMismatch,
    #[msg("Randomness cannot be fulfilled yet (delay not elapsed)")]
    RandomnessNotReady,
    #[msg("Switchboard randomness account is invalid")]
    InvalidSwitchboardAccount,
    #[msg("Switchboard randomness account does not match the one bound to the draw")]
    RandomnessAccountMismatch,
    #[msg("Draw randomness has not been committed yet")]
    RandomnessNotCommitted,
    #[msg("Draw randomness has already been committed")]
    RandomnessAlreadyCommitted,
    #[msg("Switchboard randomness queue does not match the configured queue")]
    RandomnessQueueMismatch,
    #[msg("Liquidity routing is paused")]
    LiquidityPaused,
    #[msg("The liquidity pool has already been seeded")]
    PoolAlreadySeeded,
    #[msg("The liquidity pool has not been seeded yet")]
    PoolNotSeeded,
    #[msg("Pool program is not a supported Raydium CPMM deployment")]
    InvalidPoolProgram,
    #[msg("Pool account does not match the seeded Raydium pool")]
    InvalidPoolAccount,
    #[msg("Pool spot price deviates too far from its TWAP")]
    PriceDeviationTooHigh,
    #[msg("Liquidity SOL vault cannot cover this amount and stay rent-exempt")]
    InsufficientLiquiditySol,
    #[msg("Route deployed less SOL into liquidity than the configured minimum")]
    LiquidityBelowMinimum,
    #[msg("Tier metadata root has not been set")]
    MetadataRootNotSet,
    #[msg("NFT metadata does not match the tier's committed metadata root")]
    InvalidMetadataProof,
    #[msg("Emission has not started yet")]
    EmissionNotStarted,
    #[msg("Token is already active")]
    TokenAlreadyActive,
    #[msg("ORAO VRF account is invalid")]
    InvalidOraoAccount,
    #[msg("MagicBlock VRF account is invalid")]
    InvalidMagicBlockAccount,
    #[msg("Oracle queue is not configured")]
    OracleQueueNotSet,
    #[msg("Emission schedule is fixed once the first draw has been requested")]
    EmissionAlreadyStarted,
    #[msg("Metadata URI must be an https URL")]
    InvalidMetadataUri,
    #[msg("Metadata name must not be empty")]
    InvalidMetadataName,
    #[msg("Revealed secret does not match the committed hash-chain head")]
    InvalidReveal,
    #[msg("The commit-reveal target slot hash is no longer available")]
    RandomnessTargetExpired,
}
