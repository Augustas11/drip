//! M1 bring-up binary.
//!
//! End-to-end: against a running solana-test-validator with percolator-prog already
//! deployed, this:
//!   1. Creates SPL mint + canonical vault ATA (vault_authority PDA, mint).
//!   2. Creates the market slab account.
//!   3. Calls InitMarket (asset 0 only, minimal risk params).
//!   4. Calls InitPortfolio for a user + Deposit collateral.
//!   5. Calls PermissionlessCrank with a single asset-0 / zero-oracle hint.
//!   6. Calls SyncMaintenanceFee with a cranker reward portfolio appended to
//!      verify the fee-collection appending convention. (Configured share is 0
//!      here; this proves the account-routing path, not nonzero fee movement.)
//!
//! All steps are idempotent only at the program-id level (validator is reset
//! per run via scripts/m1_bringup.sh). The market + portfolio addresses are
//! generated fresh each run.

// Plan-classification gate used by the cranker submission
// loop. Lives here so the predicate import is exercised by the M1 binary's
// build until ./cranker/ stands up. See plan_gate.rs for the full doc.
mod plan_gate;

use anyhow::{anyhow, bail, Context, Result};
use percolator_prog::{
    constants::{HEADER_LEN, MARKET_GROUP_OFF, WRAPPER_CONFIG_LEN},
    ix::{CrankObservationHint, Instruction as ProgInstruction},
    state,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    program_pack::Pack,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_instruction,
    transaction::Transaction,
};
use std::{env, str::FromStr};

const RPC_URL: &str = "http://127.0.0.1:8899";

fn main() -> Result<()> {
    let program_id_str = env::var("PERCOLATOR_PROGRAM_ID")
        .unwrap_or_else(|_| "3bgLEkBtCM4LoA89XnP26UefYACrWi1CkiwvgMtJxWzx".to_string());
    let program_id = Pubkey::from_str(&program_id_str)
        .with_context(|| format!("PERCOLATOR_PROGRAM_ID parse: {program_id_str}"))?;

    println!("== M1 bring-up against {RPC_URL} ==");
    println!("program_id = {program_id}");

    // Smoke-test the plan-classification gate so a regression in the
    // upstream predicate trips this binary's build / first run.
    {
        use percolator::{AutoCrankPlanV16, PermissionlessRecoveryReasonV16};
        let refresh_none = AutoCrankPlanV16::RefreshAccount { asset_index: None };
        let liquidate = AutoCrankPlanV16::Liquidate { asset_index: 0 };
        assert!(
            plan_gate::needs_observation(&refresh_none),
            "predicate regression: RefreshAccount{{None}} must require an observation"
        );
        assert!(
            !plan_gate::needs_observation(&liquidate),
            "predicate regression: Liquidate must NOT require a caller observation \
             (upstream 3d250d2/f5d291e/8338726 made it dispatch from committed state)"
        );
        for plan in [
            AutoCrankPlanV16::ReleaseSourceLiens,
            AutoCrankPlanV16::AdvanceClose,
            AutoCrankPlanV16::FinalizeRecovery,
            AutoCrankPlanV16::CloseResolved,
            AutoCrankPlanV16::DeclareRecovery {
                reason: PermissionlessRecoveryReasonV16::ActiveBankruptCloseCannotProgress,
            },
        ] {
            assert!(
                !plan_gate::needs_observation(&plan),
                "predicate regression: {plan:?} must NOT require a caller observation"
            );
        }
        println!(
            "plan-gate smoke OK (Refresh{{None}}=needs_obs; Liquidate/A6/A8/AdvanceClose=no_obs)"
        );
    }

    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let cluster = rpc.get_version()?;
    println!("solana cluster version: {}", cluster.solana_core);

    let payer_path = dirs_home()?.join(".config/solana/id.json");
    let payer = read_keypair_file(&payer_path)
        .map_err(|e| anyhow!("read payer keypair {payer_path:?}: {e}"))?;
    println!("payer = {}", payer.pubkey());

    // Ensure payer balance.
    let bal = rpc.get_balance(&payer.pubkey())?;
    println!("payer balance lamports = {bal}");
    if bal < 50_000_000_000 {
        bail!("payer balance too low; airdrop more SOL first");
    }

    // -------- Step 1: mint + vault --------
    let admin = Keypair::new(); // separate "marketauth" admin keypair
    rpc_airdrop(&rpc, &admin.pubkey(), 5_000_000_000)?;
    println!("admin = {}", admin.pubkey());

    let mint = Keypair::new();
    create_spl_mint(&rpc, &payer, &mint, &payer.pubkey(), 0)
        .context("create SPL mint")?;
    println!("mint = {}", mint.pubkey());

    // -------- Step 2: market slab --------
    let market = Keypair::new();
    let max_portfolio_assets: u16 = 1;
    let market_account_len = state::market_account_len_for_capacity(max_portfolio_assets as usize)
        .map_err(|e| anyhow!("market_account_len_for_capacity: {e:?}"))?;
    println!("market = {} ({} bytes)", market.pubkey(), market_account_len);

    create_program_owned_account(&rpc, &payer, &market, market_account_len, &program_id)
        .context("create market slab account")?;

    // Derive vault_authority PDA and canonical ATA.
    let (vault_authority, _bump) =
        Pubkey::find_program_address(&[b"vault", market.pubkey().as_ref()], &program_id);
    let vault = spl_associated_token_account::get_associated_token_address(
        &vault_authority,
        &mint.pubkey(),
    );
    println!("vault_authority = {vault_authority}");
    println!("vault (canonical ATA) = {vault}");

    create_ata(&rpc, &payer, &vault_authority, &mint.pubkey())
        .context("create vault canonical ATA")?;

    // -------- Step 3: InitMarket --------
    let init_market_ix = ProgInstruction::InitMarket {
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
        // M1 fee-collection probe: 100 atoms/slot accrues a maintenance fee
        // so SyncMaintenanceFee has something to route to the cranker reward
        // portfolio after a few slots.
        maintenance_fee_per_slot: 100,
    };
    send_prog_ix(
        &rpc,
        program_id,
        &payer,
        init_market_ix,
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(market.pubkey(), false),
            AccountMeta::new_readonly(mint.pubkey(), false),
        ],
        &[&admin],
        "InitMarket",
    )?;

    // -------- Step 4: InitPortfolio + Deposit (user) --------
    let user = Keypair::new();
    rpc_airdrop(&rpc, &user.pubkey(), 5_000_000_000)?;
    println!("user = {}", user.pubkey());
    let user_portfolio = create_portfolio(&rpc, &payer, &user, &market.pubkey(), program_id)?;
    println!("user_portfolio = {user_portfolio}");

    // Create the user's source-of-funds token account, mint 1_000_000 atoms to it.
    let user_token = spl_associated_token_account::get_associated_token_address(
        &user.pubkey(),
        &mint.pubkey(),
    );
    create_ata(&rpc, &payer, &user.pubkey(), &mint.pubkey())
        .context("create user ATA")?;
    mint_to(&rpc, &payer, &mint.pubkey(), &user_token, &payer, 1_000_000)
        .context("mint to user")?;

    let deposit_amount: u128 = 1_000_000;
    let user_bind = portfolio_binding(&rpc.get_account_data(&user_portfolio)?)?;
    send_prog_ix(
        &rpc,
        program_id,
        &payer,
        ProgInstruction::Deposit {
            portfolio_id: user_bind.0,
            expected_sequence: user_bind.1,
            amount: deposit_amount,
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
        "Deposit",
    )?;
    println!("deposited {deposit_amount} atoms to user portfolio");

    // -------- Step 5: PermissionlessCrank (refresh path) --------
    // A single asset-0 hint with 0 oracle accounts. With a fresh portfolio
    // this exercises the wrapper's hint decode + engine's
    // permissionless_auto_crank_not_atomic entry; the engine returns a bounded
    // refresh/no-action plan and the tx succeeds with no mutation of value.
    let crank_ix = ProgInstruction::PermissionlessCrank {
        now_slot: 1,
        observations: vec![CrankObservationHint {
            asset_index: 0,
            oracle_accounts: 0,
        }],
    };
    let crank_logs = send_prog_ix_capture_logs(
        &rpc,
        program_id,
        &payer,
        crank_ix,
        vec![
            AccountMeta::new(payer.pubkey(), true),
            AccountMeta::new(market.pubkey(), false),
            AccountMeta::new(user_portfolio, false),
        ],
        &[],
        "PermissionlessCrank (refresh, asset 0)",
    )?;
    println!("--- PermissionlessCrank logs ---");
    for l in &crank_logs {
        println!("  {l}");
    }

    // -------- Step 5b: maintenance-fee crank with cranker reward portfolio --------
    // Set 50% cranker share so SyncMaintenanceFee routes a portion of the
    // accrued fee into the appended cranker portfolio.
    let maint = market_authority_binding(&rpc.get_account_data(&market.pubkey())?)?;
    send_prog_ix(
        &rpc,
        program_id,
        &payer,
        ProgInstruction::UpdateMaintenanceFeePolicy {
            cranker_share_bps: 5000,
            policy_sequence: maint.1.saturating_add(1),
            authority_epoch: maint.0,
        },
        vec![
            AccountMeta::new(admin.pubkey(), true),
            AccountMeta::new(market.pubkey(), false),
        ],
        &[&admin],
        "UpdateMaintenanceFeePolicy(5000bps)",
    )?;

    // Cranker reward portfolio.
    let cranker = Keypair::new();
    rpc_airdrop(&rpc, &cranker.pubkey(), 2_000_000_000)?;
    println!("cranker = {}", cranker.pubkey());
    let cranker_portfolio =
        create_portfolio(&rpc, &payer, &cranker, &market.pubkey(), program_id)?;
    println!("cranker_portfolio = {cranker_portfolio}");

    // Wait long enough for several slots of maintenance fee to accrue.
    println!("waiting ~5s for slots to advance...");
    std::thread::sleep(std::time::Duration::from_secs(5));

    let user_before = rpc.get_account_data(&user_portfolio)?;
    let cranker_before = rpc.get_account_data(&cranker_portfolio)?;
    let cur_slot = rpc.get_slot()?;

    let sync_logs = send_prog_ix_capture_logs(
        &rpc,
        program_id,
        &payer,
        ProgInstruction::SyncMaintenanceFee { now_slot: cur_slot },
        vec![
            AccountMeta::new(market.pubkey(), false),
            AccountMeta::new(user_portfolio, false),
            AccountMeta::new(cranker_portfolio, false),
        ],
        &[],
        "SyncMaintenanceFee(user, cranker_appended)",
    )?;
    println!("--- SyncMaintenanceFee logs ---");
    for l in &sync_logs {
        println!("  {l}");
    }

    let user_after = rpc.get_account_data(&user_portfolio)?;
    let cranker_after = rpc.get_account_data(&cranker_portfolio)?;
    let user_diff_bytes = byte_diff_count(&user_before, &user_after);
    let cranker_diff_bytes = byte_diff_count(&cranker_before, &cranker_after);
    println!("user_portfolio bytes differing after sync: {user_diff_bytes}");
    println!("cranker_portfolio bytes differing after sync: {cranker_diff_bytes}");
    if cranker_diff_bytes == 0 {
        println!("WARN: cranker portfolio bytes did not change — fee may not have routed.");
    } else {
        println!("OK: cranker portfolio state mutated → fee-share routing wired.");
    }

    // -------- Step 6: Sanity check + report --------
    let market_data = rpc.get_account_data(&market.pubkey())?;
    println!("market account size after bringup: {} bytes", market_data.len());
    println!(
        "market magic header (first 8 bytes): {:02x?}",
        &market_data[..HEADER_LEN.min(market_data.len())]
    );
    let wrapper_end = HEADER_LEN + WRAPPER_CONFIG_LEN;
    println!(
        "wrapper_config slice [{}..{}) ({} bytes)",
        HEADER_LEN, wrapper_end, WRAPPER_CONFIG_LEN
    );
    println!("MARKET_GROUP_OFF = {MARKET_GROUP_OFF}");
    println!("V16_LAYOUT_DISCRIMINATOR (expected) = 18");

    println!("== M1 bring-up OK ==");
    Ok(())
}

/// `(portfolio_id, expected_sequence)`.
fn portfolio_binding(data: &[u8]) -> Result<(u64, u64)> {
    let id = state::read_portfolio_id(data).map_err(|e| anyhow!("read_portfolio_id: {e}"))?;
    let seq = state::read_portfolio_matcher_sequence(data)
        .map_err(|e| anyhow!("read_portfolio_matcher_sequence: {e}"))?;
    Ok((id, seq))
}

/// `(authority_epoch, maintenance_fee_seq)`.
fn market_authority_binding(data: &[u8]) -> Result<(u64, u64)> {
    let seqs = state::read_asset_control_sequences(data, 0)
        .map_err(|e| anyhow!("read_asset_control_sequences: {e}"))?;
    Ok((seqs.authority_epoch, seqs.maintenance_fee))
}

fn dirs_home() -> Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(
        env::var("HOME").context("HOME unset")?,
    ))
}

fn rpc_airdrop(rpc: &RpcClient, to: &Pubkey, lamports: u64) -> Result<()> {
    let sig = rpc.request_airdrop(to, lamports)?;
    wait_for_signature(rpc, &sig)?;
    Ok(())
}

fn wait_for_signature(
    rpc: &RpcClient,
    sig: &solana_sdk::signature::Signature,
) -> Result<()> {
    for _ in 0..40 {
        if let Ok(true) = rpc.confirm_transaction(sig) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    bail!("signature {sig} not confirmed within timeout")
}

fn create_spl_mint(
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
    let sig = rpc.send_and_confirm_transaction(&tx)?;
    println!("  spl mint init sig: {sig}");
    Ok(())
}

fn create_program_owned_account(
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
    let sig = rpc.send_and_confirm_transaction(&tx)?;
    println!("  create_account({} bytes) sig: {sig}", space);
    Ok(())
}

fn create_ata(
    rpc: &RpcClient,
    payer: &Keypair,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<()> {
    let ix = spl_associated_token_account::instruction::create_associated_token_account(
        &payer.pubkey(),
        owner,
        mint,
        &spl_token::ID,
    );
    let bh = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[payer],
        bh,
    );
    let sig = rpc.send_and_confirm_transaction(&tx)?;
    println!("  create_ata({owner}, {mint}) sig: {sig}");
    Ok(())
}

fn mint_to(
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
    let sig = rpc.send_and_confirm_transaction(&tx)?;
    println!("  mint_to({amount}) sig: {sig}");
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
        &[
            cu_ix(),
            heap_ix(),
            ix,
        ],
        Some(&payer.pubkey()),
        &[payer, owner],
        bh,
    );
    let sig = rpc.send_and_confirm_transaction(&tx)?;
    println!("  InitPortfolio sig: {sig}");
    Ok(portfolio.pubkey())
}

fn send_prog_ix(
    rpc: &RpcClient,
    program_id: Pubkey,
    payer: &Keypair,
    ix: ProgInstruction,
    accounts: Vec<AccountMeta>,
    extra_signers: &[&Keypair],
    label: &str,
) -> Result<()> {
    let _ = send_prog_ix_capture_logs(rpc, program_id, payer, ix, accounts, extra_signers, label)?;
    Ok(())
}

fn send_prog_ix_capture_logs(
    rpc: &RpcClient,
    program_id: Pubkey,
    payer: &Keypair,
    ix: ProgInstruction,
    accounts: Vec<AccountMeta>,
    extra_signers: &[&Keypair],
    label: &str,
) -> Result<Vec<String>> {
    let instruction = Instruction {
        program_id,
        accounts,
        data: ix.encode(),
    };
    let mut signer_refs: Vec<&Keypair> = Vec::with_capacity(1 + extra_signers.len());
    signer_refs.push(payer);
    signer_refs.extend_from_slice(extra_signers);

    // Retry on transient "Blockhash not found" — happens when the RPC's
    // latest blockhash hasn't propagated to the bank sim yet.
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..5 {
        let (bh, _) = rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
        let tx = Transaction::new_signed_with_payer(
            &[cu_ix(), heap_ix(), instruction.clone()],
            Some(&payer.pubkey()),
            &signer_refs,
            bh,
        );
        match rpc.send_and_confirm_transaction(&tx) {
            Ok(sig) => {
                println!("  {label} sig: {sig}");
                return fetch_tx_logs(rpc, &sig, label);
            }
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("Blockhash not found") && attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    last_err = Some(anyhow!("{msg}"));
                    continue;
                }
                return Err(anyhow!(e).context(format!("submit {label}")));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("submit {label}: exhausted retries")))
}

fn fetch_tx_logs(
    rpc: &RpcClient,
    sig: &solana_sdk::signature::Signature,
    label: &str,
) -> Result<Vec<String>> {
    // Fetch logs for this transaction. JsonParsed encoding returns log messages
    // in the meta; default Json encoding doesn't.
    let logs = match rpc.get_transaction(
        sig,
        solana_transaction_status::UiTransactionEncoding::JsonParsed,
    ) {
        Ok(tx) => tx
            .transaction
            .meta
            .and_then(|m| Option::<Vec<String>>::from(m.log_messages))
            .unwrap_or_default(),
        Err(_) => vec![],
    };
    let cu = parse_consumed_cu(&logs);
    if let Some(cu) = cu {
        println!("  {label} consumed_cu = {cu}");
    }
    Ok(logs)
}

fn parse_consumed_cu(logs: &[String]) -> Option<u64> {
    // "Program <id> consumed 42817 of 1400000 compute units"
    for line in logs {
        let Some(rest) = line.split(" consumed ").nth(1) else {
            continue;
        };
        let Some(num) = rest.split_whitespace().next() else {
            continue;
        };
        if let Ok(cu) = num.parse::<u64>() {
            return Some(cu);
        }
    }
    None
}

fn byte_diff_count(a: &[u8], b: &[u8]) -> usize {
    let mut diff = a.iter().zip(b.iter()).filter(|(x, y)| x != y).count();
    diff += a.len().abs_diff(b.len());
    diff
}

fn cu_ix() -> Instruction {
    ComputeBudgetInstruction::set_compute_unit_limit(1_400_000)
}

fn heap_ix() -> Instruction {
    ComputeBudgetInstruction::request_heap_frame(128 * 1024)
}
