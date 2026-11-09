//! Randomness abstraction for draws. See spec §6.
//!
//! Backends, selected per-config via `RandomnessSource` and locked into each draw at
//! request time:
//!   - `SlotHash` (dev/local): 32 bytes frozen from a recent SlotHashes entry at the
//!     fulfillment step. Cheap, no external dependency, intentionally NOT casino-grade.
//!   - `Vrf` (provider-agnostic): 32 bytes delivered by a trusted external fulfiller
//!     (`Config::vrf_authority`). Used for deterministic tests and as the generic VRF seam.
//!   - `Switchboard` (retired: Switchboard ended support 2026-09-25): On-Demand
//!     commit→reveal (see the `switchboard` submodule). Kept so existing accounts decode.
//!   - `Orao` (production): ORAO VRF v2 (see the `orao` submodule). `commit_draw_orao`
//!     requests randomness by CPI with a program-derived seed, `fulfill_draw_orao` reads the
//!     quorum-fulfilled value from that exact request account.
//!   - `MagicBlock` (production): MagicBlock VRF (see the `magicblock` submodule).
//!     `commit_draw_magicblock` queues a request by CPI; MagicBlock verifies the oracle's
//!     proof and calls `magicblock_callback`, which freezes the value.
//!
//! All backends converge on the SAME settle path: a fixed 32-byte `randomness` value is
//! stored on the draw, and `settle_draw` derives the winner deterministically from it.
//! This removes the execution-slot retry loop that the v1 settle required.

use crate::errors::RewardError;
use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hashv;
use anchor_lang::solana_program::sysvar::slot_hashes;

/// Read the most recent slot hash from the SlotHashes sysvar account data.
/// Layout: `[num_entries: u64][ (slot: u64, hash: [u8;32]) ... ]`, most-recent first.
pub fn recent_slot_hash(slot_hashes_acc: &AccountInfo) -> Result<[u8; 32]> {
    require_keys_eq!(
        *slot_hashes_acc.key,
        slot_hashes::ID,
        RewardError::AccountMismatch
    );
    let data = slot_hashes_acc.try_borrow_data()?;
    require!(data.len() >= 48, RewardError::RandomnessNotReady);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&data[16..48]); // skip 8 (len) + 8 (slot) of first entry
    Ok(hash)
}

/// Freeze 32 bytes of slot-hash randomness for a draw (dev/local mode). Domain-separated
/// by draw id + slots so two draws can't collide and a value can't be silently reused.
pub fn slot_hash_randomness(
    slot_hash: &[u8; 32],
    draw_id: u64,
    request_slot: u64,
    fulfill_slot: u64,
) -> [u8; 32] {
    hashv(&[
        b"ebtc-rand-slot-v1",
        slot_hash,
        &draw_id.to_le_bytes(),
        &request_slot.to_le_bytes(),
        &fulfill_slot.to_le_bytes(),
    ])
    .to_bytes()
}

/// Derive the final 32-byte selection seed from a draw's FIXED randomness. Deterministic:
/// it depends only on values frozen before settlement, so settle has no slot dependence
/// and the keeper can compute the winner exactly once.
pub fn derive_seed(
    draw_id: u64,
    request_slot: u64,
    randomness: &[u8; 32],
    config_version: u64,
) -> [u8; 32] {
    hashv(&[
        b"ebtc-draw-v2",
        &draw_id.to_le_bytes(),
        &request_slot.to_le_bytes(),
        randomness,
        &config_version.to_le_bytes(),
    ])
    .to_bytes()
}

/// Hash-chain commit-reveal (`RandomnessSource::CommitReveal`).
///
/// The keeper keeps a secret root and the chain `s_n = root`, `s_{i-1} = H(tag || s_i)`; only
/// the far end `s_0` is published as the initial head. Each reveal must satisfy
/// `H(tag || secret) == head` and becomes the new head, so the order of secrets is fixed when
/// the chain is created. The draw value mixes the secret with a slot hash that did not exist
/// when the draw was requested: the keeper cannot steer it (secret already fixed) and nobody
/// else can predict it (secret unknown).
pub mod commit_reveal {
    use super::*;

    pub const CHAIN_TAG: &[u8] = b"ebtc-cr-chain-v1";

    pub fn chain_hash(secret: &[u8; 32]) -> [u8; 32] {
        hashv(&[CHAIN_TAG, secret]).to_bytes()
    }

    /// The first (earliest) SlotHashes entry at or after `target`. Skipped slots have no
    /// entry, so this is the first block produced from `target` on. Errors with
    /// `RandomnessNotReady` before that block exists, and `RandomnessTargetExpired` once the
    /// sysvar no longer reaches back past `target` (the earliest such entry can't be proven).
    pub fn target_slot_hash(slot_hashes_acc: &AccountInfo, target: u64) -> Result<(u64, [u8; 32])> {
        require_keys_eq!(*slot_hashes_acc.key, slot_hashes::ID, RewardError::AccountMismatch);
        let data = slot_hashes_acc.try_borrow_data()?;
        require!(data.len() >= 8, RewardError::RandomnessNotReady);
        let n = u64::from_le_bytes(data[0..8].try_into().unwrap()) as usize;
        let mut found: Option<(u64, [u8; 32])> = None;
        let mut reaches_below_target = false;
        for i in 0..n {
            let off = 8 + i * 40;
            if data.len() < off + 40 {
                break;
            }
            let slot = u64::from_le_bytes(data[off..off + 8].try_into().unwrap());
            if slot < target {
                reaches_below_target = true;
                break;
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(&data[off + 8..off + 40]);
            found = Some((slot, hash)); // entries are newest-first; keep the oldest >= target
        }
        let found = found.ok_or(RewardError::RandomnessNotReady)?;
        require!(reaches_below_target, RewardError::RandomnessTargetExpired);
        Ok(found)
    }

    pub fn draw_randomness(secret: &[u8; 32], target_hash: &[u8; 32], draw_id: u64, request_slot: u64) -> [u8; 32] {
        hashv(&[
            b"ebtc-rand-cr-v1",
            secret,
            target_hash,
            &draw_id.to_le_bytes(),
            &request_slot.to_le_bytes(),
        ])
        .to_bytes()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn chain_links_back_to_the_head() {
            let s2 = [9u8; 32];
            let s1 = chain_hash(&s2);
            let head = chain_hash(&s1);
            assert_eq!(chain_hash(&s1), head);
            assert_eq!(chain_hash(&s2), s1);
            assert_ne!(chain_hash(&s2), head);
        }

        #[test]
        fn draw_value_depends_on_every_input() {
            let base = draw_randomness(&[1; 32], &[2; 32], 7, 100);
            assert_ne!(base, draw_randomness(&[3; 32], &[2; 32], 7, 100));
            assert_ne!(base, draw_randomness(&[1; 32], &[3; 32], 7, 100));
            assert_ne!(base, draw_randomness(&[1; 32], &[2; 32], 8, 100));
            assert_ne!(base, draw_randomness(&[1; 32], &[2; 32], 7, 101));
        }
    }
}

/// Switchboard On-Demand provider (commit→reveal), consumed on-chain.
///
/// We parse the Switchboard `RandomnessAccountData` directly from account bytes instead of
/// depending on the `switchboard-on-demand` crate: that crate's non-optional
/// `switchboard-protos`/`libsecp256k1` deps pull `prost`/`getrandom 0.4` (edition2024) which
/// do not build on the pinned SBF toolchain. The layout below is mirrored verbatim from the
/// crate's `RandomnessAccountData` (`#[repr(C)]`, 8-byte Anchor discriminator prefix); the
/// devnet smoke (`keeper/src/smoke-switchboard.ts`) cross-checks it against the live SDK.
/// This is the same manual-parse pattern already used for SlotHashes + Metaplex Core assets.
///
/// Security model (grind-resistant): the randomness account is bound to the draw at
/// **commit** time, while still unrevealed (`reveal_slot == 0`) and with `seed_slot` at or
/// after the draw's `request_slot`. **Reveal** then reads that exact same account/round, so a
/// caller cannot shop among pre-revealed accounts for a favourable winner.
pub mod switchboard {
    use super::*;
    use anchor_lang::solana_program::pubkey;

    // ----------------------------------------------------------------------------------------
    // Layout source of truth
    // ----------------------------------------------------------------------------------------
    // Mirrored from `switchboard-on-demand` v0.12.1,
    //   src/on_demand/accounts/randomness.rs  ->  `pub struct RandomnessAccountData`
    // which is `#[repr(C)]` + `bytemuck::Pod` with an 8-byte Anchor discriminator prefix:
    //
    //   DISCRIMINATOR = [10, 66, 229, 135, 220, 239, 217, 114]
    //   struct RandomnessAccountData {            // offsets are AFTER the 8-byte disc
    //       authority:     Pubkey,   // [  8.. 40)
    //       queue:         Pubkey,   // [ 40.. 72)
    //       seed_slothash: [u8;32],  // [ 72..104)
    //       seed_slot:     u64,      // [104..112)
    //       oracle:        Pubkey,   // [112..144)
    //       reveal_slot:   u64,      // [144..152)
    //       value:         [u8;32],  // [152..184)
    //       _ebuf2:        [u8;96],  // [184..280)
    //       _ebuf1:        [u8;128], // [280..408)
    //   }                            // size_of::<Self>() = 400, account len = 8 + 400 = 408
    //
    // We deliberately do NOT depend on the crate: its non-optional
    // `switchboard-protos`/`libsecp256k1` deps pull `prost`/`getrandom 0.4` (edition2024)
    // which do not build on the pinned SBF toolchain (Solana 2.1 / rustc 1.79). Same
    // manual-parse pattern already used for SlotHashes + Metaplex Core assets. The devnet
    // smoke (`keeper/src/smoke-switchboard.ts`) cross-checks this layout against the live SDK.
    // If you bump the SDK, re-confirm the discriminator + offsets against that exact version.
    // ----------------------------------------------------------------------------------------

    /// Anchor discriminator of `RandomnessAccountData` (switchboard-on-demand v0.12.1).
    const RANDOMNESS_DISCRIMINATOR: [u8; 8] = [10, 66, 229, 135, 220, 239, 217, 114];
    /// Switchboard On-Demand program ids (the owner of a randomness account).
    const ON_DEMAND_MAINNET: Pubkey = pubkey!("SBondMDrcV3K4kxZR1HNVT7osZxAHVHgYXL5Ze1oMUv");
    const ON_DEMAND_DEVNET: Pubkey = pubkey!("Aio4gaXjXzJNVLtzwtNVmSqGKpANtXhybbkhtAC94ji2");

    // Field byte offsets (see layout table above).
    const OFF_QUEUE: usize = 8 + 32;
    const OFF_SEED_SLOT: usize = OFF_QUEUE + 32 + 32;
    const OFF_REVEAL_SLOT: usize = OFF_SEED_SLOT + 8 + 32;
    const OFF_VALUE: usize = OFF_REVEAL_SLOT + 8;
    /// Full `RandomnessAccountData` account length (8-byte disc + 400-byte `#[repr(C)]` body).
    /// We require the full length (matching the SDK's own `parse` size check) rather than just
    /// "enough to reach `value`", so a too-short/truncated account is rejected outright.
    const RANDOMNESS_ACCOUNT_LEN: usize = 8 + 400;

    /// Data captured from a committed (not-yet-revealed) randomness account.
    #[derive(Debug)]
    pub struct CommitInfo {
        /// Switchboard `seed_slot` of the committed round (re-checked at reveal).
        pub seed_slot: u64,
    }

    #[cfg_attr(test, derive(Debug))]
    struct Parsed {
        queue: Pubkey,
        seed_slot: u64,
        reveal_slot: u64,
        value: [u8; 32],
    }

    /// Owner check: the account must be owned by the Switchboard On-Demand program on the
    /// running cluster (accept either id so one build serves devnet + mainnet).
    fn require_switchboard_owner(acc: &AccountInfo) -> Result<()> {
        require!(
            *acc.owner == ON_DEMAND_MAINNET || *acc.owner == ON_DEMAND_DEVNET,
            RewardError::InvalidSwitchboardAccount
        );
        Ok(())
    }

    /// Pure byte parse: validate length + discriminator, then read fields at fixed offsets.
    /// Owner is checked separately (it needs the `AccountInfo`); this stays pure for testing.
    fn parse_fields(data: &[u8]) -> Result<Parsed> {
        // Length BEFORE any offset read so all slices below are in-bounds (no panics on a
        // truncated / malformed account).
        require!(
            data.len() >= RANDOMNESS_ACCOUNT_LEN,
            RewardError::InvalidSwitchboardAccount
        );
        let mut disc = [0u8; 8];
        disc.copy_from_slice(&data[..8]);
        require!(
            disc == RANDOMNESS_DISCRIMINATOR,
            RewardError::InvalidSwitchboardAccount
        );

        let queue = Pubkey::new_from_array(data[OFF_QUEUE..OFF_QUEUE + 32].try_into().unwrap());
        let seed_slot =
            u64::from_le_bytes(data[OFF_SEED_SLOT..OFF_SEED_SLOT + 8].try_into().unwrap());
        let reveal_slot =
            u64::from_le_bytes(data[OFF_REVEAL_SLOT..OFF_REVEAL_SLOT + 8].try_into().unwrap());
        let mut value = [0u8; 32];
        value.copy_from_slice(&data[OFF_VALUE..OFF_VALUE + 32]);

        Ok(Parsed {
            queue,
            seed_slot,
            reveal_slot,
            value,
        })
    }

    fn parse(acc: &AccountInfo) -> Result<Parsed> {
        require_switchboard_owner(acc)?;
        let data = acc.try_borrow_data()?;
        parse_fields(&data)
    }

    /// Commit-phase rules (pure, testable): the round must belong to the configured queue
    /// (when pinned), be committed at/after the draw request (`seed_slot >= request_slot` so
    /// the value is unknowable at request time), and still be **unrevealed**
    /// (`reveal_slot == 0`) so a caller can't bind an account whose outcome they already know.
    fn check_commit(r: &Parsed, expected_queue: &Pubkey, request_slot: u64) -> Result<CommitInfo> {
        if *expected_queue != Pubkey::default() {
            require_keys_eq!(r.queue, *expected_queue, RewardError::RandomnessQueueMismatch);
        }
        require!(r.seed_slot >= request_slot, RewardError::RandomnessNotReady);
        require!(r.reveal_slot == 0, RewardError::RandomnessAlreadyFulfilled);
        Ok(CommitInfo { seed_slot: r.seed_slot })
    }

    /// Reveal-phase rules (pure, testable): must be the SAME committed round bound at commit
    /// (`seed_slot == commit_slot`) and the oracle must have revealed it (`reveal_slot != 0`).
    fn check_reveal(r: &Parsed, commit_slot: u64) -> Result<[u8; 32]> {
        require!(r.seed_slot == commit_slot, RewardError::RandomnessAccountMismatch);
        require!(r.reveal_slot != 0, RewardError::RandomnessNotReady);
        Ok(r.value)
    }

    /// Validate a freshly-committed (unrevealed) randomness account and capture its `seed_slot`.
    pub fn validate_commit(
        acc: &AccountInfo,
        expected_queue: &Pubkey,
        request_slot: u64,
        _now_slot: u64,
    ) -> Result<CommitInfo> {
        check_commit(&parse(acc)?, expected_queue, request_slot)
    }

    /// Read the revealed 32-byte value for the round bound at commit. Read directly (not via
    /// `get_value`, which would force same-slot consumption) so reveal can be a standalone
    /// crank step; determinism is preserved because the value is frozen into the draw here.
    pub fn read_revealed(
        acc: &AccountInfo,
        commit_slot: u64,
        _now_slot: u64,
    ) -> Result<[u8; 32]> {
        check_reveal(&parse(acc)?, commit_slot)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        /// Build a well-formed 408-byte RandomnessAccountData buffer.
        fn build(queue: &Pubkey, seed_slot: u64, reveal_slot: u64, value: [u8; 32]) -> Vec<u8> {
            let mut d = vec![0u8; RANDOMNESS_ACCOUNT_LEN];
            d[..8].copy_from_slice(&RANDOMNESS_DISCRIMINATOR);
            d[OFF_QUEUE..OFF_QUEUE + 32].copy_from_slice(queue.as_ref());
            d[OFF_SEED_SLOT..OFF_SEED_SLOT + 8].copy_from_slice(&seed_slot.to_le_bytes());
            d[OFF_REVEAL_SLOT..OFF_REVEAL_SLOT + 8].copy_from_slice(&reveal_slot.to_le_bytes());
            d[OFF_VALUE..OFF_VALUE + 32].copy_from_slice(&value);
            d
        }

        fn err_code(e: anchor_lang::error::Error) -> u32 {
            match e {
                anchor_lang::error::Error::AnchorError(ae) => ae.error_code_number,
                _ => panic!("expected AnchorError"),
            }
        }

        #[test]
        fn parses_fields_at_correct_offsets() {
            let q = Pubkey::new_unique();
            let v = [7u8; 32];
            let p = parse_fields(&build(&q, 123, 456, v)).unwrap();
            assert_eq!(p.queue, q);
            assert_eq!(p.seed_slot, 123);
            assert_eq!(p.reveal_slot, 456);
            assert_eq!(p.value, v);
        }

        #[test]
        fn rejects_truncated_account() {
            let buf = build(&Pubkey::new_unique(), 1, 0, [0u8; 32]);
            let short = &buf[..RANDOMNESS_ACCOUNT_LEN - 1];
            assert_eq!(err_code(parse_fields(short).unwrap_err()), err_code(error!(RewardError::InvalidSwitchboardAccount)));
            // also an empty / tiny buffer must not panic
            assert!(parse_fields(&[]).is_err());
            assert!(parse_fields(&[0u8; 8]).is_err());
        }

        #[test]
        fn rejects_bad_discriminator() {
            let mut buf = build(&Pubkey::new_unique(), 1, 0, [0u8; 32]);
            buf[0] ^= 0xFF;
            assert_eq!(err_code(parse_fields(&buf).unwrap_err()), err_code(error!(RewardError::InvalidSwitchboardAccount)));
        }

        #[test]
        fn commit_rules() {
            let q = Pubkey::new_unique();
            // unrevealed, committed at/after request, queue matches -> ok
            let ok = parse_fields(&build(&q, 100, 0, [0u8; 32])).unwrap();
            assert_eq!(check_commit(&ok, &q, 50).unwrap().seed_slot, 100);
            // default expected_queue skips the queue pin
            assert!(check_commit(&ok, &Pubkey::default(), 50).is_ok());
            // queue mismatch when pinned
            assert_eq!(
                err_code(check_commit(&ok, &Pubkey::new_unique(), 50).unwrap_err()),
                err_code(error!(RewardError::RandomnessQueueMismatch))
            );
            // committed before the request slot -> not ready
            assert_eq!(
                err_code(check_commit(&ok, &q, 200).unwrap_err()),
                err_code(error!(RewardError::RandomnessNotReady))
            );
            // already revealed at commit time -> rejected
            let revealed = parse_fields(&build(&q, 100, 105, [1u8; 32])).unwrap();
            assert_eq!(
                err_code(check_commit(&revealed, &q, 50).unwrap_err()),
                err_code(error!(RewardError::RandomnessAlreadyFulfilled))
            );
        }

        #[test]
        fn reveal_rules() {
            let q = Pubkey::new_unique();
            let v = [9u8; 32];
            let revealed = parse_fields(&build(&q, 100, 130, v)).unwrap();
            // same committed round + revealed -> returns the value
            assert_eq!(check_reveal(&revealed, 100).unwrap(), v);
            // different round than bound at commit -> mismatch
            assert_eq!(
                err_code(check_reveal(&revealed, 99).unwrap_err()),
                err_code(error!(RewardError::RandomnessAccountMismatch))
            );
            // not yet revealed -> not ready
            let unrevealed = parse_fields(&build(&q, 100, 0, v)).unwrap();
            assert_eq!(
                err_code(check_reveal(&unrevealed, 100).unwrap_err()),
                err_code(error!(RewardError::RandomnessNotReady))
            );
        }
    }
}

/// ORAO VRF v2 provider (request by CPI → quorum fulfillment), consumed on-chain.
///
/// Vendored like `switchboard`: `orao-solana-vrf` 0.7 requires anchor 0.32, which conflicts
/// with the pinned 0.31.1, so the `request_v2` CPI and the `RandomnessV2` account are
/// hand-encoded here. Source of truth: github.com/orao-network/solana-vrf `rust/sdk`
/// (`lib.rs` seeds + `RequestV2` accounts, `state/randomness_v2.rs`,
/// `state/network_state.rs`). The devnet smoke (`keeper/src/smoke-orao.ts`) cross-checks
/// these against the live program; re-confirm them if ORAO ships a new account version.
///
/// Security model (grind-resistant): the request seed is derived by THIS program from the
/// draw's `draw_id` + `request_slot`, and the request is created by CPI inside
/// `commit_draw_orao`, so the account bound to the draw is necessarily unfulfilled at
/// binding and the caller has no choice of seed. Fulfillment is written by the ORAO program
/// only after its fulfillment-authority quorum signs the seed, so no single party (keeper,
/// admin, or one ORAO operator) can pick or preview the value.
pub mod orao {
    use super::*;
    use anchor_lang::solana_program::{
        instruction::{AccountMeta, Instruction},
        pubkey,
    };

    /// ORAO VRF program (same id on devnet and mainnet).
    pub const PROGRAM_ID: Pubkey = pubkey!("VRFzZoJdhFWL8rkvu87LpKM3RbcVezpMEc6X5GVDr7y");
    /// `NetworkState` PDA: `["orao-vrf-network-configuration"]` under `PROGRAM_ID`.
    pub const NETWORK_STATE: Pubkey = pubkey!("5ER1oENnV4srxYdAynUfRzWeQCPQaqMiAp4VqyMbSqnK");
    #[cfg(test)]
    const CONFIG_ACCOUNT_SEED: &[u8] = b"orao-vrf-network-configuration";
    pub const RANDOMNESS_ACCOUNT_SEED: &[u8] = b"orao-vrf-randomness-request";

    /// sha256("global:request_v2")[..8]
    const REQUEST_V2_DISCRIMINATOR: [u8; 8] = [0x26, 0x97, 0xd1, 0x06, 0xc3, 0x66, 0x1c, 0xd9];
    /// sha256("account:RandomnessV2")[..8]
    const RANDOMNESS_V2_DISCRIMINATOR: [u8; 8] = [0x8b, 0xef, 0xb8, 0xd7, 0xe3, 0x56, 0xbf, 0xe2];
    /// sha256("account:NetworkState")[..8]
    const NETWORK_STATE_DISCRIMINATOR: [u8; 8] = [0xd4, 0xed, 0x94, 0x38, 0x61, 0xf5, 0x33, 0xa9];

    // RandomnessV2 = disc(8) || RequestAccount (Borsh enum):
    //   tag 0 Pending   { client: Pubkey, seed: [u8;32], responses: Vec<..> }
    //   tag 1 Fulfilled { client: Pubkey, seed: [u8;32], randomness: [u8;64] }
    const TAG_FULFILLED: u8 = 1;
    const OFF_TAG: usize = 8;
    const OFF_SEED: usize = OFF_TAG + 1 + 32;
    const OFF_RANDOMNESS: usize = OFF_SEED + 32;
    const FULFILLED_LEN: usize = OFF_RANDOMNESS + 64;

    // NetworkState = disc(8) || config { authority: Pubkey, treasury: Pubkey, .. } || ..
    const OFF_TREASURY: usize = 8 + 32;

    /// The request seed for a draw. Unique per draw (draw ids strictly increase) and per
    /// program id (so devnet/mainnet deployments never collide on ORAO's request PDA).
    pub fn request_seed(program_id: &Pubkey, draw_id: u64, request_slot: u64) -> [u8; 32] {
        hashv(&[
            b"ebtc-orao-seed-v1",
            program_id.as_ref(),
            &draw_id.to_le_bytes(),
            &request_slot.to_le_bytes(),
        ])
        .to_bytes()
    }

    pub fn request_address(seed: &[u8; 32]) -> Pubkey {
        Pubkey::find_program_address(&[RANDOMNESS_ACCOUNT_SEED, seed], &PROGRAM_ID).0
    }

    /// Treasury that `request_v2` pays the fee to, read from the network config.
    pub fn read_treasury(network_state: &AccountInfo) -> Result<Pubkey> {
        require_keys_eq!(*network_state.key, NETWORK_STATE, RewardError::InvalidOraoAccount);
        require_keys_eq!(*network_state.owner, PROGRAM_ID, RewardError::InvalidOraoAccount);
        let data = network_state.try_borrow_data()?;
        parse_treasury(&data)
    }

    fn parse_treasury(data: &[u8]) -> Result<Pubkey> {
        require!(data.len() >= OFF_TREASURY + 32, RewardError::InvalidOraoAccount);
        require!(data[..8] == NETWORK_STATE_DISCRIMINATOR, RewardError::InvalidOraoAccount);
        Ok(Pubkey::new_from_array(data[OFF_TREASURY..OFF_TREASURY + 32].try_into().unwrap()))
    }

    /// `request_v2(seed)` CPI. ORAO records `payer` as the request's client and refunds the
    /// pending→fulfilled rent difference to it.
    pub fn request_v2<'info>(
        orao_program: &AccountInfo<'info>,
        payer: &AccountInfo<'info>,
        network_state: &AccountInfo<'info>,
        treasury: &AccountInfo<'info>,
        request: &AccountInfo<'info>,
        system_program: &AccountInfo<'info>,
        seed: [u8; 32],
    ) -> Result<()> {
        require_keys_eq!(*orao_program.key, PROGRAM_ID, RewardError::InvalidOraoAccount);
        let mut data = Vec::with_capacity(8 + 32);
        data.extend_from_slice(&REQUEST_V2_DISCRIMINATOR);
        data.extend_from_slice(&seed);
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(*payer.key, true),
                AccountMeta::new(*network_state.key, false),
                AccountMeta::new(*treasury.key, false),
                AccountMeta::new(*request.key, false),
                AccountMeta::new_readonly(*system_program.key, false),
            ],
            data,
        };
        anchor_lang::solana_program::program::invoke(
            &ix,
            &[
                payer.clone(),
                network_state.clone(),
                treasury.clone(),
                request.clone(),
                system_program.clone(),
                orao_program.clone(),
            ],
        )?;
        Ok(())
    }

    /// Read the fulfilled 64-byte randomness of the request bound to a draw.
    pub fn read_fulfilled(request: &AccountInfo, expected_seed: &[u8; 32]) -> Result<[u8; 64]> {
        require_keys_eq!(*request.owner, PROGRAM_ID, RewardError::InvalidOraoAccount);
        let data = request.try_borrow_data()?;
        parse_fulfilled(&data, expected_seed)
    }

    fn parse_fulfilled(data: &[u8], expected_seed: &[u8; 32]) -> Result<[u8; 64]> {
        require!(data.len() > OFF_TAG, RewardError::InvalidOraoAccount);
        require!(data[..8] == RANDOMNESS_V2_DISCRIMINATOR, RewardError::InvalidOraoAccount);
        require!(data[OFF_TAG] == TAG_FULFILLED, RewardError::RandomnessNotReady);
        require!(data.len() >= FULFILLED_LEN, RewardError::InvalidOraoAccount);
        require!(
            data[OFF_SEED..OFF_SEED + 32] == expected_seed[..],
            RewardError::RandomnessAccountMismatch
        );
        let mut out = [0u8; 64];
        out.copy_from_slice(&data[OFF_RANDOMNESS..FULFILLED_LEN]);
        require!(out != [0u8; 64], RewardError::RandomnessNotReady);
        Ok(out)
    }

    /// Fold ORAO's 64-byte output into the draw's 32-byte randomness.
    pub fn draw_randomness(value: &[u8; 64]) -> [u8; 32] {
        hashv(&[b"ebtc-orao-v1", value]).to_bytes()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn err_code(e: anchor_lang::error::Error) -> u32 {
            match e {
                anchor_lang::error::Error::AnchorError(ae) => ae.error_code_number,
                _ => panic!("expected AnchorError"),
            }
        }
        fn code(e: RewardError) -> u32 {
            err_code(error!(e))
        }

        fn fulfilled(seed: &[u8; 32], value: [u8; 64]) -> Vec<u8> {
            let mut d = RANDOMNESS_V2_DISCRIMINATOR.to_vec();
            d.push(TAG_FULFILLED);
            d.extend_from_slice(Pubkey::new_unique().as_ref());
            d.extend_from_slice(seed);
            d.extend_from_slice(&value);
            d
        }

        #[test]
        fn constants_match_derivations() {
            let ns = Pubkey::find_program_address(&[CONFIG_ACCOUNT_SEED], &PROGRAM_ID).0;
            assert_eq!(ns, NETWORK_STATE);
            let d = |s: &str| hashv(&[s.as_bytes()]).to_bytes()[..8].to_vec();
            assert_eq!(d("global:request_v2"), REQUEST_V2_DISCRIMINATOR);
            assert_eq!(d("account:RandomnessV2"), RANDOMNESS_V2_DISCRIMINATOR);
            assert_eq!(d("account:NetworkState"), NETWORK_STATE_DISCRIMINATOR);
        }

        #[test]
        fn seed_is_unique_per_draw_and_program() {
            let p = Pubkey::new_unique();
            let s = request_seed(&p, 1, 100);
            assert_ne!(s, request_seed(&p, 2, 100));
            assert_ne!(s, request_seed(&p, 1, 101));
            assert_ne!(s, request_seed(&Pubkey::new_unique(), 1, 100));
        }

        #[test]
        fn reads_fulfilled_value() {
            let seed = [3u8; 32];
            assert_eq!(parse_fulfilled(&fulfilled(&seed, [7u8; 64]), &seed).unwrap(), [7u8; 64]);
        }

        #[test]
        fn rejects_pending_wrong_seed_and_malformed() {
            let seed = [3u8; 32];
            let mut pending = fulfilled(&seed, [7u8; 64]);
            pending[OFF_TAG] = 0;
            assert_eq!(err_code(parse_fulfilled(&pending, &seed).unwrap_err()), code(RewardError::RandomnessNotReady));
            assert_eq!(
                err_code(parse_fulfilled(&fulfilled(&seed, [7u8; 64]), &[4u8; 32]).unwrap_err()),
                code(RewardError::RandomnessAccountMismatch)
            );
            let mut bad = fulfilled(&seed, [7u8; 64]);
            bad[0] ^= 1;
            assert_eq!(err_code(parse_fulfilled(&bad, &seed).unwrap_err()), code(RewardError::InvalidOraoAccount));
            let full = fulfilled(&seed, [7u8; 64]);
            assert_eq!(
                err_code(parse_fulfilled(&full[..FULFILLED_LEN - 1], &seed).unwrap_err()),
                code(RewardError::InvalidOraoAccount)
            );
            assert!(parse_fulfilled(&[], &seed).is_err());
            assert_eq!(
                err_code(parse_fulfilled(&fulfilled(&seed, [0u8; 64]), &seed).unwrap_err()),
                code(RewardError::RandomnessNotReady)
            );
        }

        #[test]
        fn parses_treasury() {
            let t = Pubkey::new_unique();
            let mut d = NETWORK_STATE_DISCRIMINATOR.to_vec();
            d.extend_from_slice(Pubkey::new_unique().as_ref());
            d.extend_from_slice(t.as_ref());
            d.extend_from_slice(&[0u8; 16]);
            assert_eq!(parse_treasury(&d).unwrap(), t);
            d[1] ^= 1;
            assert!(parse_treasury(&d).is_err());
        }
    }
}

/// MagicBlock VRF provider (queued request by CPI → proof-verified callback), on Solana.
///
/// Vendored: the SDK (`ephemeral-rollups-sdk` vrf) targets newer solana/anchor crates than
/// the pinned toolchain builds, so the `RequestRandomnessScoped` instruction is hand-encoded
/// here. Source of truth: github.com/magicblock-labs/solana-vrf (`api/src/instruction.rs`,
/// `program/src/request_randomness.rs`, `program/src/provide_randomness.rs`) and
/// ephemeral-rollups-sdk `rust/vrf-sdk` (`instructions.rs`, `types.rs`, `consts.rs`).
///
/// Flow: `commit_draw_magicblock` CPIs `RequestRandomnessScoped` signed by this program's
/// `["identity"]` PDA, naming `magicblock_callback` with the draw + config accounts and the
/// commit slot as callback args. The oracle later submits a VRF proof over the request id
/// (which MagicBlock derives from our caller seed + slot + slot hash); MagicBlock verifies it
/// on-chain and invokes the callback signed by its scoped identity PDA for this program,
/// which only it can sign. Each request is served by one oracle, which can withhold but not
/// choose a value; requests expire after 120 s, after which the keeper may re-request.
pub mod magicblock {
    use super::*;
    use anchor_lang::solana_program::{
        instruction::{AccountMeta, Instruction},
        pubkey,
    };

    pub const PROGRAM_ID: Pubkey = pubkey!("Vrf1RNUjXmQGjmQrQLvJHs9SNkvDJEsRVFPkfSQUwGz");
    /// MagicBlock's default Solana queue (same on devnet and mainnet); pinned in
    /// `Config::oracle_queue` so it can be rotated without an upgrade.
    pub const DEFAULT_QUEUE: Pubkey = pubkey!("Cuj97ggrhhidhbu39TijNVqE74xvKJ69gDervRUXAxGh");
    pub const IDENTITY_SEED: &[u8] = b"identity";
    const REQUEST_RANDOMNESS_SCOPED: u8 = 10;
    /// Re-request is allowed this many slots after the previous request (~3 min at 400 ms;
    /// MagicBlock expires unserved requests after 120 s).
    pub const RECOMMIT_AFTER_SLOTS: u64 = 450;

    /// This program's requester identity (signs the request CPI).
    pub fn requester_identity(program_id: &Pubkey) -> (Pubkey, u8) {
        Pubkey::find_program_address(&[IDENTITY_SEED], program_id)
    }

    /// MagicBlock's scoped identity for this program: the only signer of our callback.
    pub fn callback_identity(program_id: &Pubkey) -> Pubkey {
        Pubkey::find_program_address(&[IDENTITY_SEED, program_id.as_ref()], &PROGRAM_ID).0
    }

    pub fn caller_seed(program_id: &Pubkey, draw_id: u64, request_slot: u64, commit_slot: u64) -> [u8; 32] {
        hashv(&[
            b"ebtc-magicblock-seed-v1",
            program_id.as_ref(),
            &draw_id.to_le_bytes(),
            &request_slot.to_le_bytes(),
            &commit_slot.to_le_bytes(),
        ])
        .to_bytes()
    }

    /// Borsh `RequestRandomness` behind the 8-byte instruction header.
    pub fn request_data(
        caller_seed: &[u8; 32],
        callback_program_id: &Pubkey,
        callback_discriminator: &[u8],
        callback_accounts: &[(Pubkey, bool)],
        callback_args: &[u8],
    ) -> Vec<u8> {
        let mut d = vec![REQUEST_RANDOMNESS_SCOPED, 0, 0, 0, 0, 0, 0, 0];
        d.extend_from_slice(caller_seed);
        d.extend_from_slice(callback_program_id.as_ref());
        d.extend_from_slice(&(callback_discriminator.len() as u32).to_le_bytes());
        d.extend_from_slice(callback_discriminator);
        d.extend_from_slice(&(callback_accounts.len() as u32).to_le_bytes());
        for (key, writable) in callback_accounts {
            d.extend_from_slice(key.as_ref());
            d.push(0); // is_signer
            d.push(*writable as u8);
        }
        d.extend_from_slice(&(callback_args.len() as u32).to_le_bytes());
        d.extend_from_slice(callback_args);
        d
    }

    #[allow(clippy::too_many_arguments)]
    pub fn request<'info>(
        vrf_program: &AccountInfo<'info>,
        payer: &AccountInfo<'info>,
        identity: &AccountInfo<'info>,
        identity_bump: u8,
        queue: &AccountInfo<'info>,
        system_program: &AccountInfo<'info>,
        slot_hashes_acc: &AccountInfo<'info>,
        data: Vec<u8>,
    ) -> Result<()> {
        require_keys_eq!(*vrf_program.key, PROGRAM_ID, RewardError::InvalidMagicBlockAccount);
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: vec![
                AccountMeta::new(*payer.key, true),
                AccountMeta::new_readonly(*identity.key, true),
                AccountMeta::new(*queue.key, false),
                AccountMeta::new_readonly(*system_program.key, false),
                AccountMeta::new_readonly(*slot_hashes_acc.key, false),
            ],
            data,
        };
        anchor_lang::solana_program::program::invoke_signed(
            &ix,
            &[
                payer.clone(),
                identity.clone(),
                queue.clone(),
                system_program.clone(),
                slot_hashes_acc.clone(),
                vrf_program.clone(),
            ],
            &[&[IDENTITY_SEED, &[identity_bump]]],
        )?;
        Ok(())
    }

    /// Whether a callback belongs to the draw's latest outstanding request. Anything else
    /// (stale request, cancelled/settled draw, config change) is ignored rather than failed,
    /// so the oracle's fulfilment tx still lands and the request leaves the queue.
    pub fn is_current(
        draw_pending: bool,
        source_is_magicblock: bool,
        randomness_pending: bool,
        version_matches: bool,
        committed_slot: Option<u64>,
        echoed_commit_slot: u64,
    ) -> bool {
        draw_pending
            && source_is_magicblock
            && randomness_pending
            && version_matches
            && committed_slot == Some(echoed_commit_slot)
    }

    pub fn draw_randomness(value: &[u8; 32]) -> [u8; 32] {
        hashv(&[b"ebtc-magicblock-v1", value]).to_bytes()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn encodes_scoped_request() {
            let seed = [5u8; 32];
            let prog = Pubkey::new_unique();
            let a = Pubkey::new_unique();
            let b = Pubkey::new_unique();
            let d = request_data(&seed, &prog, &[1, 2, 3, 4, 5, 6, 7, 8], &[(a, true), (b, false)], &9u64.to_le_bytes());
            assert_eq!(&d[..8], &[10, 0, 0, 0, 0, 0, 0, 0]);
            assert_eq!(&d[8..40], &seed);
            assert_eq!(&d[40..72], prog.as_ref());
            assert_eq!(&d[72..76], &8u32.to_le_bytes());
            assert_eq!(&d[76..84], &[1, 2, 3, 4, 5, 6, 7, 8]);
            assert_eq!(&d[84..88], &2u32.to_le_bytes());
            assert_eq!(&d[88..120], a.as_ref());
            assert_eq!(&d[120..122], &[0, 1]);
            assert_eq!(&d[122..154], b.as_ref());
            assert_eq!(&d[154..156], &[0, 0]);
            assert_eq!(&d[156..160], &8u32.to_le_bytes());
            assert_eq!(&d[160..], &9u64.to_le_bytes());
        }

        #[test]
        fn only_the_latest_request_is_current() {
            assert!(is_current(true, true, true, true, Some(7), 7));
            assert!(!is_current(true, true, true, true, Some(8), 7), "stale request");
            assert!(!is_current(true, true, true, true, None, 7), "never committed");
            assert!(!is_current(false, true, true, true, Some(7), 7), "draw not pending");
            assert!(!is_current(true, false, true, true, Some(7), 7), "other source");
            assert!(!is_current(true, true, false, true, Some(7), 7), "already fulfilled");
            assert!(!is_current(true, true, true, false, Some(7), 7), "config changed");
        }

        #[test]
        fn seeds_and_identities_are_distinct() {
            let p = Pubkey::new_unique();
            assert_ne!(caller_seed(&p, 1, 2, 3), caller_seed(&p, 1, 2, 4));
            assert_ne!(callback_identity(&p), requester_identity(&p).0);
            assert_ne!(callback_identity(&p), callback_identity(&Pubkey::new_unique()));
        }
    }
}
