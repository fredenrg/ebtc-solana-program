//! Metaplex Core asset verification using the official `mpl-core` deserialization.
//!
//! Replaces the earlier manual byte parse. With `mpl-core` deserialization we can:
//!   - reliably read the current `owner`
//!   - verify the asset belongs to the expected collection (`UpdateAuthority::Collection`)
//!   - (combined with the on-chain `TokenRegistry`) confirm the asset is a recognised NFT

use crate::errors::RewardError;
use anchor_lang::prelude::*;
use mpl_core::{accounts::BaseAssetV1, types::UpdateAuthority, ID as MPL_CORE_ID};

/// Deserialize a Core asset account, asserting it is owned by the Core program.
pub fn load_asset(asset: &AccountInfo) -> Result<BaseAssetV1> {
    require_keys_eq!(*asset.owner, MPL_CORE_ID, RewardError::AccountMismatch);
    let data = asset.try_borrow_data()?;
    let parsed = BaseAssetV1::from_bytes(&data).map_err(|_| RewardError::WinnerVerificationFailed)?;
    Ok(parsed)
}

/// Read the current owner of a Core asset.
pub fn read_asset_owner(asset: &AccountInfo) -> Result<Pubkey> {
    Ok(load_asset(asset)?.owner)
}

/// Assert `expected_owner` currently owns `asset`.
pub fn assert_asset_owner(asset: &AccountInfo, expected_owner: &Pubkey) -> Result<()> {
    let parsed = load_asset(asset)?;
    require_keys_eq!(parsed.owner, *expected_owner, RewardError::NotAssetOwner);
    Ok(())
}

/// Assert `asset` belongs to `expected_collection` (its update authority is that collection).
pub fn assert_in_collection(asset: &AccountInfo, expected_collection: &Pubkey) -> Result<()> {
    let parsed = load_asset(asset)?;
    match parsed.update_authority {
        UpdateAuthority::Collection(c) => {
            require_keys_eq!(c, *expected_collection, RewardError::AccountMismatch);
            Ok(())
        }
        _ => err!(RewardError::AccountMismatch),
    }
}

/// Full verification used in draw flows: correct collection + correct owner.
/// "Recognised NFT" is additionally enforced by the caller via the `TokenRegistry`
/// PDA (`seeds = [b"token", token_id]`) whose `asset` field must equal this account.
pub fn verify_asset(
    asset: &AccountInfo,
    expected_owner: &Pubkey,
    expected_collection: &Pubkey,
) -> Result<()> {
    let parsed = load_asset(asset)?;
    require_keys_eq!(parsed.owner, *expected_owner, RewardError::NotAssetOwner);
    match parsed.update_authority {
        UpdateAuthority::Collection(c) => {
            require_keys_eq!(c, *expected_collection, RewardError::AccountMismatch);
            Ok(())
        }
        _ => err!(RewardError::AccountMismatch),
    }
}
