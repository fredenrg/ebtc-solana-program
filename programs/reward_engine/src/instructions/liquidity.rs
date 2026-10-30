//! NFT-sale liquidity: route intents, the Raydium CPMM pool seed and per-route execution.
//! See spec §3.5, §14.
//!
//! Sale SOL only ever leaves the `liquidity_sol` vault into the PDA-owned wSOL account and from
//! there into the Raydium pool. The `liquidity_vault` PDA creates the pool, owns every token
//! account involved and receives all LP, and no instruction moves LP out, so the position is
//! locked by construction.

use crate::constants::*;
use crate::errors::RewardError;
use crate::events::*;
use crate::raydium::{self, PoolView, Q32, WSOL_MINT};
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer as sol_transfer, Transfer as SolTransfer};
use anchor_spl::associated_token::{get_associated_token_address, AssociatedToken};
use anchor_spl::token::{sync_native, Mint, SyncNative, Token, TokenAccount};

fn require_keeper_or_admin(config: &Config, signer: &Pubkey) -> Result<()> {
    require!(
        *signer == config.keeper || *signer == config.admin,
        RewardError::Unauthorized
    );
    Ok(())
}

#[derive(Accounts)]
pub struct RecordSaleRoute<'info> {
    #[account(mut)]
    pub caller: Signer<'info>,

    #[account(mut, seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        init,
        payer = caller,
        space = 8 + LiquidityRoute::INIT_SPACE,
        seeds = [SEED_ROUTE, &config.route_counter.to_le_bytes()],
        bump
    )]
    pub liquidity_route: Account<'info, LiquidityRoute>,

    pub system_program: Program<'info, System>,
}

pub fn record_sale_route(ctx: Context<RecordSaleRoute>, buyer: Pubkey, sale_lamports: u64) -> Result<()> {
    require_keeper_or_admin(&ctx.accounts.config, &ctx.accounts.caller.key())?;

    let route = &mut ctx.accounts.liquidity_route;
    route.route_id = ctx.accounts.config.route_counter;
    route.buyer = buyer;
    route.sale_lamports = sale_lamports;
    route.mode = ctx.accounts.config.liquidity_mode;
    route.target_pool = ctx.accounts.config.pool_address;
    route.status = RouteStatus::Pending;
    route.created_ts = Clock::get()?.unix_timestamp;
    route.executed_ts = None;
    route.failure_code = 0;
    route.retry_count = 0;
    route.bump = ctx.bumps.liquidity_route;

    ctx.accounts.config.route_counter += 1;

    emit!(SaleRouteCreated {
        route_id: route.route_id,
        buyer,
        sale_lamports,
    });
    Ok(())
}

#[derive(Accounts)]
pub struct UpdateRoute<'info> {
    pub caller: Signer<'info>,
    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [SEED_ROUTE, &liquidity_route.route_id.to_le_bytes()], bump = liquidity_route.bump)]
    pub liquidity_route: Account<'info, LiquidityRoute>,
}

/// Park a route the keeper cannot execute (e.g. dust that mints no LP) so it stops retrying.
pub fn mark_route_failed(ctx: Context<UpdateRoute>, reason_code: u16) -> Result<()> {
    require_keeper_or_admin(&ctx.accounts.config, &ctx.accounts.caller.key())?;
    let r = &mut ctx.accounts.liquidity_route;
    require!(r.status == RouteStatus::Pending, RewardError::RouteNotInExpectedState);
    r.status = RouteStatus::Failed;
    r.failure_code = reason_code;
    emit!(RouteStatusChanged { route_id: r.route_id, status_code: 2, reason_code });
    Ok(())
}

pub fn retry_route(ctx: Context<UpdateRoute>) -> Result<()> {
    require_keeper_or_admin(&ctx.accounts.config, &ctx.accounts.caller.key())?;
    let r = &mut ctx.accounts.liquidity_route;
    require!(r.status == RouteStatus::Failed, RewardError::RouteNotInExpectedState);
    r.status = RouteStatus::Pending;
    r.retry_count = r.retry_count.saturating_add(1);
    emit!(RouteStatusChanged { route_id: r.route_id, status_code: 0, reason_code: 0 });
    Ok(())
}

/// Move lamports out of the system-owned `liquidity_sol` PDA. The runtime rejects a system
/// account left with a non-zero balance below rent exemption, so fail early with a clear error.
fn release_sol<'info>(
    vault: &AccountInfo<'info>,
    to: &AccountInfo<'info>,
    system_program: &AccountInfo<'info>,
    bump: u8,
    lamports: u64,
) -> Result<()> {
    if lamports == 0 {
        return Ok(());
    }
    let remaining = vault
        .lamports()
        .checked_sub(lamports)
        .ok_or(RewardError::InsufficientLiquiditySol)?;
    require!(
        remaining == 0 || remaining >= Rent::get()?.minimum_balance(0),
        RewardError::InsufficientLiquiditySol
    );
    sol_transfer(
        CpiContext::new_with_signer(
            system_program.clone(),
            SolTransfer { from: vault.clone(), to: to.clone() },
            &[&[SEED_LIQUIDITY_SOL, &[bump]]],
        ),
        lamports,
    )
}

fn wrap<'info>(token_program: &AccountInfo<'info>, wsol: &AccountInfo<'info>) -> Result<()> {
    sync_native(CpiContext::new(token_program.clone(), SyncNative { account: wsol.clone() }))
}

fn token_amount(info: &AccountInfo) -> Result<u64> {
    Ok(TokenAccount::try_deserialize(&mut &info.try_borrow_data()?[..])?.amount)
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct SeedPoolArgs {
    pub ebtc_amount: u64,
    pub sol_amount: u64,
    /// Lamports moved to the `liquidity_vault` PDA to pay Raydium's create-pool fee and the
    /// pool account rent (~0.19 SOL on mainnet). Any excess stays on the PDA.
    pub creator_lamports: u64,
    pub open_time: u64,
}

#[derive(Accounts)]
pub struct SeedPool<'info> {
    #[account(mut, address = config.admin @ RewardError::Unauthorized)]
    pub admin: Signer<'info>,

    #[account(mut, seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,

    /// CHECK: PDA that creates the pool, owns both token accounts and receives the LP.
    #[account(mut, seeds = [SEED_LIQUIDITY_VAULT], bump = config.liquidity_vault_auth_bump)]
    pub liquidity_vault_authority: UncheckedAccount<'info>,

    /// CHECK: system-owned SOL vault PDA.
    #[account(mut, seeds = [SEED_LIQUIDITY_SOL], bump = config.liquidity_sol_bump)]
    pub liquidity_sol_vault: UncheckedAccount<'info>,

    #[account(address = config.ebtc_mint)]
    pub ebtc_mint: Box<Account<'info, Mint>>,

    #[account(address = WSOL_MINT)]
    pub wsol_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        associated_token::mint = ebtc_mint,
        associated_token::authority = liquidity_vault_authority,
    )]
    pub liquidity_ebtc_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        init_if_needed,
        payer = admin,
        associated_token::mint = wsol_mint,
        associated_token::authority = liquidity_vault_authority,
    )]
    pub liquidity_wsol_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: allowlisted Raydium CPMM deployment.
    #[account(executable, constraint = raydium::is_supported_program(cpmm_program.key) @ RewardError::InvalidPoolProgram)]
    pub cpmm_program: UncheckedAccount<'info>,

    /// CHECK: owner + discriminator checked in the handler.
    pub amm_config: UncheckedAccount<'info>,

    /// CHECK: Raydium verifies its own authority PDA.
    pub cpmm_authority: UncheckedAccount<'info>,

    /// CHECK: derived and checked in the handler; created by Raydium.
    #[account(mut)]
    pub pool_state: UncheckedAccount<'info>,

    /// CHECK: derived and checked in the handler; created by Raydium.
    #[account(mut)]
    pub lp_mint: UncheckedAccount<'info>,

    /// CHECK: must be the PDA's LP ATA (checked in the handler); created by Raydium.
    #[account(mut)]
    pub lp_vault: UncheckedAccount<'info>,

    /// CHECK: Raydium verifies the vault seeds.
    #[account(mut)]
    pub token_0_vault: UncheckedAccount<'info>,

    /// CHECK: Raydium verifies the vault seeds.
    #[account(mut)]
    pub token_1_vault: UncheckedAccount<'info>,

    /// CHECK: Raydium pins the create-pool fee receiver.
    #[account(mut)]
    pub create_pool_fee: UncheckedAccount<'info>,

    /// CHECK: derived and checked in the handler; created by Raydium.
    #[account(mut)]
    pub observation_state: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
    pub rent: Sysvar<'info, Rent>,
}

/// One-shot: create the SOL/eBTC Raydium CPMM pool from the PDA vaults. The opening price is
/// `sol_amount : ebtc_amount`.
pub fn seed_pool(ctx: Context<SeedPool>, args: SeedPoolArgs) -> Result<()> {
    let a = &ctx.accounts;
    require!(a.config.pool_address == Pubkey::default(), RewardError::PoolAlreadySeeded);
    require!(args.ebtc_amount > 0 && args.sol_amount > 0, RewardError::InvalidLiquidityConfig);

    let program = a.cpmm_program.key();
    raydium::read_trade_fee_rate(&a.amm_config, &program)?;

    let ebtc_mint = a.ebtc_mint.key();
    let sol_is_0 = WSOL_MINT < ebtc_mint;
    let (mint_0, mint_1) = if sol_is_0 { (WSOL_MINT, ebtc_mint) } else { (ebtc_mint, WSOL_MINT) };

    let pda = |seeds: &[&[u8]]| Pubkey::find_program_address(seeds, &program).0;
    let pool = pda(&[raydium::POOL_SEED, a.amm_config.key.as_ref(), mint_0.as_ref(), mint_1.as_ref()]);
    require_keys_eq!(a.pool_state.key(), pool, RewardError::InvalidPoolAccount);
    let lp_mint = pda(&[raydium::POOL_LP_MINT_SEED, pool.as_ref()]);
    require_keys_eq!(a.lp_mint.key(), lp_mint, RewardError::InvalidPoolAccount);
    let observation = pda(&[raydium::OBSERVATION_SEED, pool.as_ref()]);
    require_keys_eq!(a.observation_state.key(), observation, RewardError::InvalidPoolAccount);
    require_keys_eq!(
        a.lp_vault.key(),
        get_associated_token_address(a.liquidity_vault_authority.key, &lp_mint),
        RewardError::InvalidPoolAccount
    );

    let system = a.system_program.to_account_info();
    let sol_vault = a.liquidity_sol_vault.to_account_info();
    let sol_bump = a.config.liquidity_sol_bump;
    let wsol = a.liquidity_wsol_vault.to_account_info();
    release_sol(&sol_vault, &wsol, &system, sol_bump, args.sol_amount)?;
    wrap(&a.token_program.to_account_info(), &wsol)?;
    release_sol(&sol_vault, &a.liquidity_vault_authority.to_account_info(), &system, sol_bump, args.creator_lamports)?;

    let ebtc = a.liquidity_ebtc_vault.to_account_info();
    let wsol_mint = a.wsol_mint.to_account_info();
    let ebtc_mint_info = a.ebtc_mint.to_account_info();
    let (m0, m1, c0, c1, amount_0, amount_1) = if sol_is_0 {
        (&wsol_mint, &ebtc_mint_info, &wsol, &ebtc, args.sol_amount, args.ebtc_amount)
    } else {
        (&ebtc_mint_info, &wsol_mint, &ebtc, &wsol, args.ebtc_amount, args.sol_amount)
    };
    let vault_bump = a.config.liquidity_vault_auth_bump;
    let signer: &[&[&[u8]]] = &[&[SEED_LIQUIDITY_VAULT, &[vault_bump]]];
    raydium::initialize(
        raydium::InitializeAccounts {
            program: &a.cpmm_program.to_account_info(),
            creator: &a.liquidity_vault_authority.to_account_info(),
            amm_config: &a.amm_config.to_account_info(),
            authority: &a.cpmm_authority.to_account_info(),
            pool_state: &a.pool_state.to_account_info(),
            token_0_mint: m0,
            token_1_mint: m1,
            lp_mint: &a.lp_mint.to_account_info(),
            creator_token_0: c0,
            creator_token_1: c1,
            creator_lp_token: &a.lp_vault.to_account_info(),
            token_0_vault: &a.token_0_vault.to_account_info(),
            token_1_vault: &a.token_1_vault.to_account_info(),
            create_pool_fee: &a.create_pool_fee.to_account_info(),
            observation_state: &a.observation_state.to_account_info(),
            token_program: &a.token_program.to_account_info(),
            associated_token_program: &a.associated_token_program.to_account_info(),
            system_program: &system,
            rent: &a.rent.to_account_info(),
        },
        amount_0,
        amount_1,
        args.open_time,
        signer,
    )?;

    let lp_amount = token_amount(&a.lp_vault.to_account_info())?;
    let amm_config = a.amm_config.key();
    let c = &mut ctx.accounts.config;
    c.pool_program = program;
    c.pool_address = pool;
    c.pool_amm_config = amm_config;
    c.pool_lp_mint = lp_mint;
    c.pool_observation = observation;
    c.pool_seeded_ts = Clock::get()?.unix_timestamp;
    c.pool_seed_price_x32 = raydium::price_x32(args.sol_amount, args.ebtc_amount)?;

    emit!(PoolSeeded {
        pool,
        lp_mint,
        sol_amount: args.sol_amount,
        ebtc_amount: args.ebtc_amount,
        lp_amount,
    });
    Ok(())
}

#[derive(Accounts)]
pub struct ExecuteRoute<'info> {
    pub caller: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Box<Account<'info, Config>>,

    #[account(mut, seeds = [SEED_ROUTE, &liquidity_route.route_id.to_le_bytes()], bump = liquidity_route.bump)]
    pub liquidity_route: Box<Account<'info, LiquidityRoute>>,

    /// CHECK: system-owned SOL vault PDA.
    #[account(mut, seeds = [SEED_LIQUIDITY_SOL], bump = config.liquidity_sol_bump)]
    pub liquidity_sol_vault: UncheckedAccount<'info>,

    /// CHECK: PDA signer for the pool position.
    #[account(seeds = [SEED_LIQUIDITY_VAULT], bump = config.liquidity_vault_auth_bump)]
    pub liquidity_vault_authority: UncheckedAccount<'info>,

    #[account(address = config.ebtc_mint)]
    pub ebtc_mint: Box<Account<'info, Mint>>,

    #[account(address = WSOL_MINT)]
    pub wsol_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        associated_token::mint = ebtc_mint,
        associated_token::authority = liquidity_vault_authority,
    )]
    pub liquidity_ebtc_vault: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        associated_token::mint = wsol_mint,
        associated_token::authority = liquidity_vault_authority,
    )]
    pub liquidity_wsol_vault: Box<Account<'info, TokenAccount>>,

    #[account(mut, address = config.pool_lp_mint @ RewardError::InvalidPoolAccount)]
    pub lp_mint: Box<Account<'info, Mint>>,

    #[account(
        mut,
        associated_token::mint = lp_mint,
        associated_token::authority = liquidity_vault_authority,
    )]
    pub lp_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: pinned at seed time.
    #[account(address = config.pool_program @ RewardError::PoolNotSeeded)]
    pub cpmm_program: UncheckedAccount<'info>,

    /// CHECK: Raydium verifies its own authority PDA.
    pub cpmm_authority: UncheckedAccount<'info>,

    /// CHECK: pinned at seed time.
    #[account(address = config.pool_amm_config @ RewardError::InvalidPoolAccount)]
    pub amm_config: UncheckedAccount<'info>,

    /// CHECK: pinned at seed time; parsed in the handler.
    #[account(mut, address = config.pool_address @ RewardError::InvalidPoolAccount)]
    pub pool_state: UncheckedAccount<'info>,

    /// Checked against the pool state in the handler.
    #[account(mut)]
    pub token_0_vault: Box<Account<'info, TokenAccount>>,

    /// Checked against the pool state in the handler.
    #[account(mut)]
    pub token_1_vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: pinned at seed time; parsed in the handler.
    #[account(mut, address = config.pool_observation @ RewardError::InvalidPoolAccount)]
    pub observation_state: UncheckedAccount<'info>,

    pub token_program: Program<'info, Token>,

    /// CHECK: Raydium's deposit requires the Token-2022 program account even for SPL mints.
    #[account(address = raydium::TOKEN_2022_PROGRAM_ID)]
    pub token_program_2022: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

fn load_pool(a: &ExecuteRoute) -> Result<PoolView> {
    let pool = raydium::read_pool(&a.pool_state, a.cpmm_program.key)?;
    require_keys_eq!(pool.token_0_vault, a.token_0_vault.key(), RewardError::InvalidPoolAccount);
    require_keys_eq!(pool.token_1_vault, a.token_1_vault.key(), RewardError::InvalidPoolAccount);
    Ok(pool)
}

fn vault_signer(a: &ExecuteRoute) -> [u8; 1] {
    [a.config.liquidity_vault_auth_bump]
}

/// Reject when the spot price has been pushed away from the TWAP. Returns the SOL reserve and
/// the TWAP (eBTC base units per lamport, Q32) for sizing the swap.
#[inline(never)]
fn price_guard(a: &ExecuteRoute, pool: &PoolView, sol_is_0: bool, now: u64) -> Result<(u64, u128)> {
    let cfg = &a.config;
    let (r0, r1) = pool.reserves(a.token_0_vault.amount, a.token_1_vault.amount)?;
    let (r_sol, r_ebtc) = if sol_is_0 { (r0, r1) } else { (r1, r0) };
    let spot = raydium::price_x32(r_sol, r_ebtc)?;
    let obs = raydium::read_observation(&a.observation_state, a.cpmm_program.key, sol_is_0)?;
    let twap = raydium::twap_x32(
        &obs,
        spot,
        cfg.pool_seed_price_x32,
        cfg.pool_seeded_ts.max(0) as u64,
        now,
        cfg.twap_window_seconds as u64,
    )
    .ok_or(RewardError::MathOverflow)?;
    require!(
        raydium::within_deviation(spot, twap, cfg.max_price_deviation_bps),
        RewardError::PriceDeviationTooHigh
    );
    Ok((r_sol, twap))
}

/// Swap part of the PDA's wSOL for eBTC, sized so the remainder pairs at the post-swap ratio
/// and capped at `max_swap_to_ebtc_bps`. Returns (lamports swapped, eBTC received).
#[inline(never)]
fn swap_step(a: &mut ExecuteRoute, sol_is_0: bool, r_sol: u64, twap: u128) -> Result<(u64, u64)> {
    let available = a.liquidity_wsol_vault.amount;
    let cap = (available as u128 * a.config.max_swap_to_ebtc_bps as u128 / BPS_DENOMINATOR as u128) as u64;
    let fee_rate = raydium::read_trade_fee_rate(&a.amm_config, a.cpmm_program.key)?;
    let amount_in = raydium::zap_swap_amount(available, r_sol, fee_rate).min(cap);
    if amount_in == 0 {
        return Ok((0, 0));
    }
    let slippage = a.config.max_slippage_bps as u128;
    let min_out = (amount_in as u128 * twap / Q32) * (BPS_DENOMINATOR as u128 - slippage) / BPS_DENOMINATOR as u128;
    let ebtc_before = a.liquidity_ebtc_vault.amount;
    let (sol_vault, ebtc_vault) = if sol_is_0 {
        (a.token_0_vault.to_account_info(), a.token_1_vault.to_account_info())
    } else {
        (a.token_1_vault.to_account_info(), a.token_0_vault.to_account_info())
    };
    let bump = vault_signer(a);
    raydium::swap_base_input(
        raydium::SwapAccounts {
            program: &a.cpmm_program.to_account_info(),
            payer: &a.liquidity_vault_authority.to_account_info(),
            authority: &a.cpmm_authority.to_account_info(),
            amm_config: &a.amm_config.to_account_info(),
            pool_state: &a.pool_state.to_account_info(),
            input_token_account: &a.liquidity_wsol_vault.to_account_info(),
            output_token_account: &a.liquidity_ebtc_vault.to_account_info(),
            input_vault: &sol_vault,
            output_vault: &ebtc_vault,
            token_program: &a.token_program.to_account_info(),
            input_mint: &a.wsol_mint.to_account_info(),
            output_mint: &a.ebtc_mint.to_account_info(),
            observation_state: &a.observation_state.to_account_info(),
        },
        amount_in,
        min_out.min(u64::MAX as u128) as u64,
        &[&[SEED_LIQUIDITY_VAULT, &bump]],
    )?;
    a.liquidity_wsol_vault.reload()?;
    a.liquidity_ebtc_vault.reload()?;
    a.token_0_vault.reload()?;
    a.token_1_vault.reload()?;
    Ok((amount_in, a.liquidity_ebtc_vault.amount.saturating_sub(ebtc_before)))
}

/// Deposit as much of the PDA's wSOL + eBTC as pairs at the pool ratio. Returns
/// (SOL deposited, eBTC deposited, LP minted).
#[inline(never)]
fn deposit_step(a: &mut ExecuteRoute, sol_is_0: bool) -> Result<(u64, u64, u64)> {
    let pool = load_pool(a)?;
    let (r0, r1) = pool.reserves(a.token_0_vault.amount, a.token_1_vault.amount)?;
    let (sol_before, ebtc_before, lp_before) =
        (a.liquidity_wsol_vault.amount, a.liquidity_ebtc_vault.amount, a.lp_vault.amount);
    let (b0, b1) = if sol_is_0 { (sol_before, ebtc_before) } else { (ebtc_before, sol_before) };
    let lp = raydium::lp_for_balances(b0, b1, r0, r1, pool.lp_supply);
    if lp == 0 {
        return Ok((0, 0, 0));
    }
    let (wsol, ebtc) = (a.liquidity_wsol_vault.to_account_info(), a.liquidity_ebtc_vault.to_account_info());
    let (wsol_mint, ebtc_mint) = (a.wsol_mint.to_account_info(), a.ebtc_mint.to_account_info());
    let (acc_0, acc_1, mint_0, mint_1) = if sol_is_0 {
        (&wsol, &ebtc, &wsol_mint, &ebtc_mint)
    } else {
        (&ebtc, &wsol, &ebtc_mint, &wsol_mint)
    };
    let bump = vault_signer(a);
    raydium::deposit(
        raydium::DepositAccounts {
            program: &a.cpmm_program.to_account_info(),
            owner: &a.liquidity_vault_authority.to_account_info(),
            authority: &a.cpmm_authority.to_account_info(),
            pool_state: &a.pool_state.to_account_info(),
            owner_lp_token: &a.lp_vault.to_account_info(),
            token_0_account: acc_0,
            token_1_account: acc_1,
            token_0_vault: &a.token_0_vault.to_account_info(),
            token_1_vault: &a.token_1_vault.to_account_info(),
            token_program: &a.token_program.to_account_info(),
            token_program_2022: &a.token_program_2022.to_account_info(),
            vault_0_mint: mint_0,
            vault_1_mint: mint_1,
            lp_mint: &a.lp_mint.to_account_info(),
        },
        lp,
        b0,
        b1,
        &[&[SEED_LIQUIDITY_VAULT, &bump]],
    )?;
    a.liquidity_wsol_vault.reload()?;
    a.liquidity_ebtc_vault.reload()?;
    a.lp_vault.reload()?;
    Ok((
        sol_before - a.liquidity_wsol_vault.amount,
        ebtc_before - a.liquidity_ebtc_vault.amount,
        a.lp_vault.amount - lp_before,
    ))
}

/// Add one sale's SOL to the pool. In `SwapAndPair` part of the PDA's wSOL is swapped for eBTC
/// first; in `TreasuryPair` it pairs with eBTC already in the liquidity vault. Leftovers stay in
/// the PDA accounts and roll into the next route.
///
/// A route that cannot execute (price guard, slippage, below-minimum deployment) reverts and
/// stays `Pending` for the keeper to retry.
pub fn execute_route(ctx: Context<ExecuteRoute>) -> Result<()> {
    let a = ctx.accounts;
    require_keeper_or_admin(&a.config, &a.caller.key())?;
    require!(a.config.pool_address != Pubkey::default(), RewardError::PoolNotSeeded);
    require!(a.config.liquidity_mode != LiquidityMode::Paused, RewardError::LiquidityPaused);
    require!(a.liquidity_route.status == RouteStatus::Pending, RewardError::RouteNotInExpectedState);

    let pool = load_pool(a)?;
    let ebtc_mint = a.config.ebtc_mint;
    let sol_is_0 = pool.token_0_mint == WSOL_MINT;
    let (expect_0, expect_1) = if sol_is_0 { (WSOL_MINT, ebtc_mint) } else { (ebtc_mint, WSOL_MINT) };
    require!(
        pool.token_0_mint == expect_0 && pool.token_1_mint == expect_1,
        RewardError::InvalidPoolAccount
    );

    let sale = a.liquidity_route.sale_lamports;
    release_sol(
        &a.liquidity_sol_vault.to_account_info(),
        &a.liquidity_wsol_vault.to_account_info(),
        &a.system_program.to_account_info(),
        a.config.liquidity_sol_bump,
        sale,
    )?;
    wrap(&a.token_program.to_account_info(), &a.liquidity_wsol_vault.to_account_info())?;
    a.liquidity_wsol_vault.reload()?;

    let now = Clock::get()?.unix_timestamp.max(0) as u64;
    let (r_sol, twap) = price_guard(a, &pool, sol_is_0, now)?;
    let (swapped, bought) = if a.config.liquidity_mode == LiquidityMode::SwapAndPair {
        swap_step(a, sol_is_0, r_sol, twap)?
    } else {
        (0, 0)
    };
    let (sol_deposited, ebtc_deposited, lp_minted) = deposit_step(a, sol_is_0)?;

    let deployed = swapped as u128 + sol_deposited as u128;
    require!(
        deployed * BPS_DENOMINATOR as u128
            >= sale as u128 * a.config.min_liquidity_pct_of_sale_bps as u128,
        RewardError::LiquidityBelowMinimum
    );

    let r = &mut a.liquidity_route;
    r.status = RouteStatus::Executed;
    r.executed_ts = Some(now as i64);
    r.swapped_lamports = swapped;
    r.ebtc_bought = bought;
    r.sol_deposited = sol_deposited;
    r.ebtc_deposited = ebtc_deposited;
    r.lp_minted = lp_minted;

    emit!(LiquidityAdded {
        route_id: r.route_id,
        sale_lamports: sale,
        swapped_lamports: swapped,
        ebtc_bought: bought,
        sol_deposited,
        ebtc_deposited,
        lp_minted,
    });
    emit!(RouteStatusChanged { route_id: r.route_id, status_code: 1, reason_code: 0 });
    Ok(())
}

