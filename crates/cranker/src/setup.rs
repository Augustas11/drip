//! One-shot environment setup: SPL mint, market slab, InitMarket, one
//! user portfolio with a deposit, and one cranker reward portfolio.
//!
//! This mirrors what the M1 bring-up client does but
//! is inlined here so the cranker is self-contained: run against a
//! deployed program on a validator, get back a `CrankerEnv` describing
//! everything the loop needs. No coordination with `m1_bringup` required.
//!
//! Not idempotent — every run generates fresh keypairs. The bringup
//! script resets the ledger, so re-runs stay clean.

use anyhow::{anyhow, bail, Context, Result};
use percolator_prog::{
    ix::Instruction as ProgInstruction,
    state,
};

use crate::binding;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};

pub struct CrankerEnv {
    pub payer: Keypair,
    /// Admin keypair kept for post-setup governance calls (e.g. rotating
    /// fee policies mid-run in later work). Not read by the cranker loop.
    #[allow(dead_code)]
    pub admin: Keypair,
    #[allow(dead_code)]
    pub mint: Pubkey,
    pub market: Pubkey,
    #[allow(dead_code)]
    pub vault: Pubkey,
    pub user_portfolio: Pubkey,
    pub cranker_portfolio: Pubkey,
    /// Owner keypair of `user_portfolio`. Exposed so tests can sign
    /// `CloseResolved` (the wrapper checks `expect_portfolio_view_owner`
    /// against this key, and requires its signature inside the
    /// force-close delay window). Unused by the cranker loop.
    #[allow(dead_code)]
    pub user: Keypair,
}

/// `CRANKER_ENABLE_FEE_SHARES=1` opts the default bootstrap into fee
/// paths: a maintenance fee plus 50% liquidation and maintenance cranker
/// shares, so the keeper's reward portfolio accrues. Off by default.
pub fn fee_shares_enabled() -> bool {
    std::env::var("CRANKER_ENABLE_FEE_SHARES").ok().as_deref() == Some("1")
}

/// Default bootstrap for the binary: **no fee paths** unless
/// [`fee_shares_enabled`]. Off means maintenance fee 0, cranker shares left
/// at the wrapper default 0, and (in `main.rs`) no reward portfolio
/// attached to cranks.
pub fn bootstrap(rpc: &RpcClient, program_id: &Pubkey, payer: Keypair) -> Result<CrankerEnv> {
    if fee_shares_enabled() {
        bootstrap_with_options(rpc, program_id, payer, 100, true)
    } else {
        bootstrap_with_options(rpc, program_id, payer, 0, false)
    }
}

/// Like [`bootstrap`] but with a caller-chosen `maintenance_fee_per_slot`.
/// Pass `0` for a market that accrues no maintenance fee — used by the
/// drive-to-close test so `insurance`/`vault` stay zero after accounts are
/// paid out, which is what lets `CloseSlab` retire the slab.
///
/// This explicit form keeps the 50% cranker shares **on**: callers that use
/// it (the integration tests) exercise the reward-portfolio ABI on purpose.
pub fn bootstrap_with_fee(
    rpc: &RpcClient,
    program_id: &Pubkey,
    payer: Keypair,
    maintenance_fee_per_slot: u128,
) -> Result<CrankerEnv> {
    bootstrap_with_options(rpc, program_id, payer, maintenance_fee_per_slot, true)
}

/// Full form: `enable_fee_shares` sets 50% liquidation and maintenance
/// cranker shares (the wrapper accepts a trailing reward portfolio on a
/// crank only when the liquidation share is non-zero).
pub fn bootstrap_with_options(
    rpc: &RpcClient,
    program_id: &Pubkey,
    payer: Keypair,
    maintenance_fee_per_slot: u128,
    enable_fee_shares: bool,
) -> Result<CrankerEnv> {
    let admin = Keypair::new();
    rpc_airdrop(rpc, &admin.pubkey(), 5_000_000_000)?;

    // Mint.
    let mint = Keypair::new();
    create_spl_mint(rpc, &payer, &mint, &payer.pubkey(), 0).context("create mint")?;

    // Market slab.
    let market = Keypair::new();
    let max_portfolio_assets: u16 = 1;
    let market_len = state::market_account_len_for_capacity(max_portfolio_assets as usize)
        .map_err(|e| anyhow!("market_account_len_for_capacity: {e:?}"))?;
    create_program_owned_account(rpc, &payer, &market, market_len, program_id)?;

    // Vault (canonical ATA of vault_authority PDA).
    let (vault_authority, _bump) =
        Pubkey::find_program_address(&[b"vault", market.pubkey().as_ref()], program_id);
    let vault = spl_associated_token_account::get_associated_token_address(
        &vault_authority,
        &mint.pubkey(),
    );
    create_ata(rpc, &payer, &vault_authority, &mint.pubkey())?;

    // InitMarket with maintenance_fee_per_slot > 0 so the fee-routing path
    // has something to move each cycle.
    send_prog_ix(
        rpc,
        program_id,
        &payer,
        ProgInstruction::InitMarket {
            max_portfolio_assets,
            h_min: 0,
            h_max: 10,
            initial_price: 100,
            min_nonzero_mm_req: 1,
            min_nonzero_im_req: 2,
            maintenance_margin_bps: 10_000,
            initial_margin_bps: 10_000,
            max_trading_fee_bps: 10_000,
            trade_fee_base_bps: 0,
            liquidation_fee_bps: 0,
            liquidation_fee_cap: 0,
            min_liquidation_abs: 0,
            max_price_move_bps_per_slot: 10_000,
            max_accrual_dt_slots: 1,
            max_abs_funding_e9_per_slot: 0,
            min_funding_lifetime_slots: 1,
            max_account_b_settlement_chunks: 1,
            max_bankrupt_close_chunks: 1,
            max_bankrupt_close_lifetime_slots: 100,
            public_b_chunk_atoms: percolator::MAX_VAULT_TVL,
            maintenance_fee_per_slot,
        },
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(market.pubkey(), false),
            AccountMeta::new_readonly(mint.pubkey(), false),
        ],
        &[&admin],
    )?;

    if enable_fee_shares {
        // 50% cranker share of maintenance fees. Wrapper `#135` binds
        // policy_sequence (strictly newer than the current lane) and
        // authority_epoch (exact match on asset 0).
        let maint = binding::market_authority_binding(&rpc.get_account_data(&market.pubkey())?)?;
        send_prog_ix(
            rpc,
            program_id,
            &payer,
            ProgInstruction::UpdateMaintenanceFeePolicy {
                cranker_share_bps: 5000,
                policy_sequence: maint.maintenance_fee_seq.saturating_add(1),
                authority_epoch: maint.authority_epoch,
            },
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            &[&admin],
        )?;

        // Enable the liquidation cranker reward path (50% share). This gates
        // the reward-portfolio-append acceptance during PermissionlessCrank
        // (`reward_enabled = liquidation_cranker_fee_share_bps != 0`, at
        // the wrapper's src/v16_program.rs:10467). Without this the wrapper
        // treats an appended reward portfolio as an unaccounted-for oracle
        // tail account and rejects with InvalidInstruction. We still won't
        // see nonzero fee movement here (no liquidations happen in the
        // synthetic market), but the ABI shape is now correctly exercised.
        let liq = binding::market_authority_binding(&rpc.get_account_data(&market.pubkey())?)?;
        send_prog_ix(
            rpc,
            program_id,
            &payer,
            ProgInstruction::UpdateLiquidationFeePolicy {
                cranker_share_bps: 5000,
                policy_sequence: liq.liquidation_fee_seq.saturating_add(1),
                authority_epoch: liq.authority_epoch,
            },
            vec![
                AccountMeta::new(admin.pubkey(), true),
                AccountMeta::new(market.pubkey(), false),
            ],
            &[&admin],
        )?;
    }

    // User portfolio + a 1_000_000-atom deposit so maintenance fees can
    // accrue against real capital each slot.
    let user = Keypair::new();
    rpc_airdrop(rpc, &user.pubkey(), 5_000_000_000)?;
    let user_portfolio = create_portfolio(rpc, &payer, &user, &market.pubkey(), *program_id)?;
    let user_token =
        spl_associated_token_account::get_associated_token_address(&user.pubkey(), &mint.pubkey());
    create_ata(rpc, &payer, &user.pubkey(), &mint.pubkey())?;
    mint_to(rpc, &payer, &mint.pubkey(), &user_token, &payer, 1_000_000)?;
    let user_bind = binding::portfolio_binding(&rpc.get_account_data(&user_portfolio)?)?;
    send_prog_ix(
        rpc,
        program_id,
        &payer,
        ProgInstruction::Deposit {
            portfolio_id: user_bind.portfolio_id,
            expected_sequence: user_bind.sequence,
            amount: 1_000_000,
        },
        vec![
            AccountMeta::new(user.pubkey(), true),
            AccountMeta::new(market.pubkey(), false),
            AccountMeta::new(user_portfolio, false),
            AccountMeta::new(user_token, false),
            AccountMeta::new(vault, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        &[&user],
    )?;

    // Cranker's own reward portfolio. Owner MUST be the tx payer/signer —
    // the wrapper's PermissionlessCrank reward-append path (see
    // `expect_signer(owner)` + `reward_owner != owner.key.to_bytes()` at
    // the wrapper's src/v16_program.rs:10473,10485) rejects with Unauthorized
    // otherwise. In a real deployment the cranker's operator keypair would
    // both own the reward portfolio and sign submissions.
    let cranker_portfolio = create_portfolio(rpc, &payer, &payer, &market.pubkey(), *program_id)?;

    Ok(CrankerEnv {
        payer,
        admin,
        mint: mint.pubkey(),
        market: market.pubkey(),
        vault,
        user_portfolio,
        cranker_portfolio,
        user,
    })
}

// -------- low-level helpers (mirror the client) --------

pub fn rpc_airdrop(rpc: &RpcClient, to: &Pubkey, lamports: u64) -> Result<()> {
    let sig = rpc.request_airdrop(to, lamports)?;
    for _ in 0..40 {
        if let Ok(true) = rpc.confirm_transaction(&sig) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    bail!("airdrop {sig} not confirmed within timeout")
}

pub fn create_spl_mint(
    rpc: &RpcClient,
    payer: &Keypair,
    mint: &Keypair,
    mint_authority: &Pubkey,
    decimals: u8,
) -> Result<()> {
    let rent = rpc.get_minimum_balance_for_rent_exemption(spl_token::state::Mint::LEN)?;
    let ix1 = system_instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        rent,
        spl_token::state::Mint::LEN as u64,
        &spl_token::ID,
    );
    let ix2 = spl_token::instruction::initialize_mint(
        &spl_token::ID,
        &mint.pubkey(),
        mint_authority,
        None,
        decimals,
    )?;
    let bh = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix1, ix2],
        Some(&payer.pubkey()),
        &[payer, mint],
        bh,
    );
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

pub fn create_program_owned_account(
    rpc: &RpcClient,
    payer: &Keypair,
    new_account: &Keypair,
    space: usize,
    owner_program: &Pubkey,
) -> Result<()> {
    let rent = rpc.get_minimum_balance_for_rent_exemption(space)?;
    let ix = system_instruction::create_account(
        &payer.pubkey(),
        &new_account.pubkey(),
        rent,
        space as u64,
        owner_program,
    );
    let bh = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, new_account],
        bh,
    );
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

pub fn create_ata(rpc: &RpcClient, payer: &Keypair, owner: &Pubkey, mint: &Pubkey) -> Result<()> {
    let ix = spl_associated_token_account::instruction::create_associated_token_account(
        &payer.pubkey(),
        owner,
        mint,
        &spl_token::ID,
    );
    let bh = rpc.get_latest_blockhash()?;
    let tx =
        Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer], bh);
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

pub fn mint_to(
    rpc: &RpcClient,
    payer: &Keypair,
    mint: &Pubkey,
    dest: &Pubkey,
    mint_authority: &Keypair,
    amount: u64,
) -> Result<()> {
    let ix = spl_token::instruction::mint_to(
        &spl_token::ID,
        mint,
        dest,
        &mint_authority.pubkey(),
        &[],
        amount,
    )?;
    let bh = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer, mint_authority],
        bh,
    );
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(())
}

fn create_portfolio(
    rpc: &RpcClient,
    payer: &Keypair,
    owner: &Keypair,
    market: &Pubkey,
    program_id: Pubkey,
) -> Result<Pubkey> {
    let portfolio = Keypair::new();
    let portfolio_len =
        state::portfolio_account_len_for_market_slots(1).map_err(|e| anyhow!("{e:?}"))?;
    create_program_owned_account(rpc, payer, &portfolio, portfolio_len, &program_id)?;
    let ix = Instruction {
        program_id,
        accounts: vec![
            AccountMeta::new(owner.pubkey(), true),
            AccountMeta::new(*market, false),
            AccountMeta::new(portfolio.pubkey(), false),
        ],
        data: ProgInstruction::InitPortfolio.encode(),
    };
    let bh = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[cu_ix(), heap_ix(), ix],
        Some(&payer.pubkey()),
        &[payer, owner],
        bh,
    );
    rpc.send_and_confirm_transaction(&tx)?;
    Ok(portfolio.pubkey())
}

fn send_prog_ix(
    rpc: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    ix: ProgInstruction,
    accounts: Vec<AccountMeta>,
    extra_signers: &[&Keypair],
) -> Result<()> {
    let instruction = Instruction {
        program_id: *program_id,
        accounts,
        data: ix.encode(),
    };
    let mut signer_refs: Vec<&Keypair> = Vec::with_capacity(1 + extra_signers.len());
    signer_refs.push(payer);
    signer_refs.extend_from_slice(extra_signers);
    for attempt in 0..5 {
        let (bh, _) = rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
        let tx = Transaction::new_signed_with_payer(
            &[cu_ix(), heap_ix(), instruction.clone()],
            Some(&payer.pubkey()),
            &signer_refs,
            bh,
        );
        match rpc.send_and_confirm_transaction(&tx) {
            Ok(_) => return Ok(()),
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("Blockhash not found") && attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    continue;
                }
                return Err(anyhow!(msg));
            }
        }
    }
    bail!("send_prog_ix: retries exhausted");
}

fn cu_ix() -> Instruction {
    ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)
}

fn heap_ix() -> Instruction {
    ComputeBudgetInstruction::request_heap_frame(128 * 1024)
}
