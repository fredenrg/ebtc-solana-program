//! Vault funding instructions. See spec §3.2. All additive and permissionless.

use crate::constants::*;
use crate::state::*;
use anchor_lang::prelude::*;
use anchor_lang::system_program::{transfer as sol_transfer, Transfer as SolTransfer};
use anchor_spl::token::{transfer as spl_transfer, Mint, Token, TokenAccount, Transfer as SplTransfer};

#[derive(Accounts)]
pub struct FundEbtcVault<'info> {
    #[account(mut)]
    pub funder: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(address = config.ebtc_mint)]
    pub ebtc_mint: Account<'info, Mint>,

    #[account(
        mut,
        constraint = funder_token_account.mint == config.ebtc_mint,
        constraint = funder_token_account.owner == funder.key(),
    )]
    pub funder_token_account: Account<'info, TokenAccount>,

    /// Destination vault ATA (reward or liquidity eBTC). Caller chooses which.
    #[account(mut, constraint = destination_vault.mint == config.ebtc_mint)]
    pub destination_vault: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

pub fn fund_ebtc_vault(ctx: Context<FundEbtcVault>, amount: u64) -> Result<()> {
    let cpi = CpiContext::new(
        ctx.accounts.token_program.to_account_info(),
        SplTransfer {
            from: ctx.accounts.funder_token_account.to_account_info(),
            to: ctx.accounts.destination_vault.to_account_info(),
            authority: ctx.accounts.funder.to_account_info(),
        },
    );
    spl_transfer(cpi, amount)?;
    Ok(())
}

#[derive(Accounts)]
pub struct FundLiquiditySol<'info> {
    #[account(mut)]
    pub funder: Signer<'info>,

    #[account(seeds = [SEED_CONFIG], bump = config.bump)]
    pub config: Account<'info, Config>,

    /// CHECK: system-owned SOL vault PDA.
    #[account(mut, seeds = [SEED_LIQUIDITY_SOL], bump = config.liquidity_sol_bump)]
    pub liquidity_sol_vault: UncheckedAccount<'info>,

    pub system_program: Program<'info, System>,
}

pub fn fund_liquidity_sol(ctx: Context<FundLiquiditySol>, lamports: u64) -> Result<()> {
    let cpi = CpiContext::new(
        ctx.accounts.system_program.to_account_info(),
        SolTransfer {
            from: ctx.accounts.funder.to_account_info(),
            to: ctx.accounts.liquidity_sol_vault.to_account_info(),
        },
    );
    sol_transfer(cpi, lamports)?;
    Ok(())
}
