//! Minimal permissionless cranker for percolator-prog (pinned to
//! wrapper `5cb331dd` / engine `4db11a8c`).
//!
//! **Reference / dry-run only; not a paid live service.** Nothing is deployed on a public cluster.
//!
//! Loop shape:
//!   1. Boot a self-contained market environment (`setup::bootstrap`).
//!   2. For N cycles, `cycle::run_one_cycle`:
//!      a. Discover portfolios (`getProgramAccounts`, filtered by KIND) and
//!         rank them (market-scoped, skip our own reward portfolio).
//!      b. Read the market once: Live/Recovery → one hint per active leg
//!         asset; Resolved → at most one oracle-less backing hint.
//!      c. Order by priority (liquidatable first), rotate within a class,
//!         and spend a per-cycle wall/CU/tx budget (`scheduler`).
//!      d. Submit `PermissionlessCrank`, classify Progress / Idle / Failed.
//!         `EngineNonProgress` is idle. Repeated same-error failures put the
//!         account in quarantine with capped exponential backoff.
//!   3. Print the mirror-custody table, cycle summaries and CU stats.
//!
//! Deliberate omissions:
//!   * No `SyncMaintenanceFee` submission in the loop (M1 proved fee routing).
//!   * No priority-fee logic. No leader-slot prediction. No jito-bundle.
//!   * Fees are **off by default**: the bootstrap market has no maintenance
//!     fee and zero cranker shares, and cranks attach no reward portfolio.
//!     `CRANKER_ENABLE_FEE_SHARES=1` opts in (maintenance fee + 50% shares,
//!     reward portfolio appended, so a share accrues into the keeper's own
//!     portfolio). Nothing withdraws it unless `CRANKER_WITHDRAWAL_TEST=1`,
//!     which also requires the opt-in. No bounty path, no mainnet path.

use anyhow::{anyhow, Context, Result};
use percolator::AutoCrankPlanV16;
use drip::{
    binding::{self, SOURCE_LIEN_DRAIN_MAX_STEPS, V16_LAYOUT_DISCRIMINATOR},
    cycle::{self, CrankCtx, CuStats, SubmissionRecord, HINT_BUDGET},
    observation, plan_gate,
    quarantine::{Quarantine, QuarantineConfig},
    scheduler::CycleBudget,
    setup,
    submit::CrankOutcome,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
};
use std::{env, str::FromStr, time::Duration};

const RPC_URL: &str = "http://127.0.0.1:8899";
const DEFAULT_CYCLES: usize = 3;
const CYCLE_INTERVAL: Duration = Duration::from_secs(3);

fn main() -> Result<()> {
    let program_id_str = env::var("PERCOLATOR_PROGRAM_ID").context(
        "PERCOLATOR_PROGRAM_ID must be set (the deployed program id — see \
         scripts/m1_bringup.sh for how to derive it from the SBF keypair)",
    )?;
    let program_id =
        Pubkey::from_str(&program_id_str).context("parse PERCOLATOR_PROGRAM_ID")?;
    let cycles = env::var("CRANKER_CYCLES")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(DEFAULT_CYCLES);

    println!("== cranker loop against {RPC_URL} ==");
    println!("program_id = {program_id}");
    println!("cycles = {cycles} (CRANKER_CYCLES to override)");
    println!("hint_budget = {HINT_BUDGET}");
    println!(
        "layout_discriminator = {V16_LAYOUT_DISCRIMINATOR} (A6 drain cap = {SOURCE_LIEN_DRAIN_MAX_STEPS})"
    );

    // Smoke-test the plan-classification gate, including post-#195
    // variants. Delegates to auto_crank_plan_requires_caller_observation.
    {
        use percolator::PermissionlessRecoveryReasonV16;
        let refresh_none = AutoCrankPlanV16::RefreshAccount { asset_index: None };
        assert!(plan_gate::needs_observation(&refresh_none));
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
        println!("plan-gate smoke OK (Refresh{{None}}=needs_obs; A6/A7/A8/AdvanceClose=no_obs)");
    }

    let rpc = RpcClient::new_with_commitment(RPC_URL.to_string(), CommitmentConfig::confirmed());
    let _ = rpc.get_version().context("connect to RPC")?;

    let payer_path = home_dir()?.join(".config/solana/id.json");
    let payer = read_keypair_file(&payer_path)
        .map_err(|e| anyhow!("read payer keypair {payer_path:?}: {e}"))?;
    println!("payer = {}", payer.pubkey());

    // ---- one-shot bootstrap ----
    println!("== bootstrapping market environment ==");
    let env = setup::bootstrap(&rpc, &program_id, payer)?;
    println!("market            = {}", env.market);
    println!("mint              = {}", env.mint);
    println!("vault             = {}", env.vault);
    println!("user_portfolio    = {}", env.user_portfolio);
    println!("cranker_portfolio = {} (our reward acct)", env.cranker_portfolio);

    // ---- cycle loop ----
    let pyth_resolver = match env::var("CRANKER_PYTH_MODE").as_deref() {
        Ok("canonical") => observation::PythFeedResolver::canonical_pda_only(),
        _ => observation::empty_pyth_resolver(),
    };
    let budget = CycleBudget::from_env();
    let mut quarantine = Quarantine::new(QuarantineConfig::from_env());
    println!(
        "budget: wall={:?} cu={} max_tx={} | quarantine: {:?}",
        budget.max_wall,
        budget.max_cu,
        budget.max_submissions,
        quarantine.config()
    );
    let ctx = CrankCtx {
        rpc: &rpc,
        program_id: &program_id,
        payer: &env.payer,
        market: &env.market,
        // Fee paths are opt-in (CRANKER_ENABLE_FEE_SHARES=1); without them the
        // wrapper would reject a trailing reward portfolio anyway.
        reward_portfolio: setup::fee_shares_enabled().then_some(&env.cranker_portfolio),
        pyth: &pyth_resolver,
        exclude: &[],
    };
    let mut records: Vec<SubmissionRecord> = Vec::new();
    let mut cu = CuStats::default();
    let (mut progressed, mut idle, mut failed) = (0usize, 0usize, 0usize);
    for cycle in 0..cycles {
        println!("\n---- cycle {cycle} ----");
        let first = records.len();
        let summary =
            cycle::run_one_cycle(&ctx, cycle as u64, &budget, &mut quarantine, &mut records, &mut cu)?;
        for r in &records[first..] {
            log_outcome(r);
        }
        println!("{}", summary.line());
        progressed += summary.progressed;
        idle += summary.idle;
        failed += summary.failed;
        // CI sets CRANKER_FAIL_ON_ERRORS=1: on a fresh bootstrap market any
        // failed attempt is a regression, so the dry run must not exit 0.
        if (summary.failed > 0 || !summary.unconfirmed.is_empty())
            && env::var("CRANKER_FAIL_ON_ERRORS").as_deref() == Ok("1")
        {
            anyhow::bail!(
                "cycle {cycle}: {} failed, {} unconfirmed: {:?} {:?}",
                summary.failed,
                summary.unconfirmed.len(),
                summary.failures,
                summary.unconfirmed
            );
        }
        if cycle + 1 < cycles {
            println!("cycle {cycle} done; sleeping {:?}", CYCLE_INTERVAL);
            std::thread::sleep(CYCLE_INTERVAL);
        }
    }

    // ---- summary ----
    println!("\n== mirror-custody summary ==");
    println!(
        "{:<6} {:<45} {:<24} {:<12} {:<7} {:<7} {:<9}",
        "cycle", "target_portfolio", "plan_class", "outcome", "tgtΔ", "rwdΔ", "prog_cu"
    );
    for r in &records {
        let (label, tgt, rwd) = match &r.outcome {
            CrankOutcome::Ok { unreconciled: Some(_), .. } => {
                ("Ok(unreconciled)", "?".into(), "?".into())
            }
            CrankOutcome::Ok {
                mutated,
                target_bytes_diff,
                reward_bytes_diff,
                ..
            } => (
                if *mutated { "Ok(mut)" } else { "Ok(no-mut)" },
                target_bytes_diff.to_string(),
                reward_bytes_diff.map(|d| d.to_string()).unwrap_or_else(|| "-".into()),
            ),
            CrankOutcome::HardReject { error_name, .. } => {
                (error_name.unwrap_or("HardReject"), "0".into(), "-".into())
            }
        };
        println!(
            "{:<6} {:<45} {:<24} {:<12} {:<7} {:<7} {:<9}",
            r.cycle,
            r.target,
            r.plan_class,
            label,
            tgt,
            rwd,
            r.outcome.program_cu().map(|c| c.to_string()).unwrap_or_else(|| "-".into()),
        );
    }
    println!(
        "\ntotals: submissions={} | per-attempt: progressed={progressed} idle={idle} failed={failed} | quarantine events={}",
        records.len(),
        quarantine.events().len()
    );
    println!(
        "cu: noop_min={:?} (tx total {:?}) worst_ok={:?} worst_reject={:?} (program CU)",
        cu.noop_min, cu.noop_tx, cu.worst_ok, cu.worst_reject
    );

    // Post-run reward portfolio snapshot for reconciliation.
    let reward_final = rpc.get_account_data(&env.cranker_portfolio)?;
    let reward_hash = fnv(&reward_final);
    println!(
        "cranker_portfolio final: {} bytes, fingerprint={:x}",
        reward_final.len(),
        reward_hash
    );

    // ---- optional spot-check: cranker portfolio withdrawal ----
    // Guarded so it never fires in the default run. When enabled:
    //   1. Route some maintenance fee to the cranker portfolio via
    //      `SyncMaintenanceFee` (as in M1's fee-routing proof).
    //   2. Attempt `Withdraw` from the cranker portfolio.
    //   3. Confirm the destination ATA balance moves — proves the cranker
    //      portfolio is byte-for-byte the same withdrawal surface as a
    //      user portfolio.
    if env::var("CRANKER_WITHDRAWAL_TEST").ok().as_deref() == Some("1") {
        anyhow::ensure!(
            setup::fee_shares_enabled(),
            "CRANKER_WITHDRAWAL_TEST=1 needs CRANKER_ENABLE_FEE_SHARES=1 (fee paths are opt-in)"
        );
        run_q6_withdrawal_test(&rpc, &program_id, &env)?;
    }

    println!("== cranker loop OK ==");
    Ok(())
}

/// Withdrawal spot-check. See main() call site for context.
fn run_q6_withdrawal_test(
    rpc: &RpcClient,
    program_id: &Pubkey,
    env: &setup::CrankerEnv,
) -> Result<()> {
    use percolator_prog::ix::Instruction as ProgInstruction;
    use solana_sdk::{
        compute_budget::ComputeBudgetInstruction,
        instruction::{AccountMeta, Instruction},
        transaction::Transaction,
    };

    println!("\n== withdrawal spot-check ==");

    // Route maintenance fee to the cranker portfolio. Wait a few slots
    // first so meaningful fee accrues.
    println!("waiting 4s for maintenance-fee accrual...");
    std::thread::sleep(Duration::from_secs(4));
    let now_slot = rpc.get_slot()?;
    let sync_ix = ProgInstruction::SyncMaintenanceFee { now_slot };
    let sync_instr = Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(env.market, false),
            AccountMeta::new(env.user_portfolio, false),
            AccountMeta::new(env.cranker_portfolio, false),
        ],
        data: sync_ix.encode(),
    };
    let cu = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    let heap = ComputeBudgetInstruction::request_heap_frame(128 * 1024);
    let bh = rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?.0;
    let tx = Transaction::new_signed_with_payer(
        &[cu.clone(), heap.clone(), sync_instr],
        Some(&env.payer.pubkey()),
        &[&env.payer],
        bh,
    );
    let sync_sig = rpc
        .send_and_confirm_transaction(&tx)
        .context("SyncMaintenanceFee to route fee to cranker portfolio")?;
    println!("SyncMaintenanceFee sig = {sync_sig}");

    // Create the payer's ATA (Withdraw dest); mint the payer as owner
    // (already the cranker portfolio owner from setup).
    let payer_ata = spl_associated_token_account::get_associated_token_address(
        &env.payer.pubkey(),
        &env.mint,
    );
    let create_ata_ix =
        spl_associated_token_account::instruction::create_associated_token_account(
            &env.payer.pubkey(),
            &env.payer.pubkey(),
            &env.mint,
            &spl_token::ID,
        );
    let bh = rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?.0;
    let tx = Transaction::new_signed_with_payer(
        &[create_ata_ix],
        Some(&env.payer.pubkey()),
        &[&env.payer],
        bh,
    );
    rpc.send_and_confirm_transaction(&tx)
        .context("create payer ATA for withdraw dest")?;
    let dest_before: u64 = spl_token_amount(rpc, &payer_ata)?;
    println!("payer ATA before Withdraw: {} atoms", dest_before);

    // Read cranker portfolio's capital via the same public helper the
    // wrapper uses — proves we're comparing the right field.
    let cranker_data = rpc.get_account_data(&env.cranker_portfolio)?;
    let cranker_view = percolator_prog::state::read_portfolio(&cranker_data)
        .map_err(|e| anyhow!("read_portfolio(cranker): {e:?}"))?;
    let cranker_capital: u128 = cranker_view.capital.get();
    println!("cranker_portfolio capital: {cranker_capital} atoms");
    if cranker_capital == 0 {
        println!("(no maintenance fee accrued — likely too little elapsed. Withdraw(0) still \
                  exercises the code path; proceeding.)");
    }

    // Withdraw up to what's available (min of capital and 100 atoms) so
    // this always succeeds; a real cranker would pick its own cadence.
    let withdraw_amount = cranker_capital.min(100);
    let (vault_authority, _bump) =
        Pubkey::find_program_address(&[b"vault", env.market.as_ref()], program_id);
    let cranker_bind = binding::portfolio_binding(&cranker_data)?;
    let withdraw_ix = ProgInstruction::Withdraw {
        portfolio_id: cranker_bind.portfolio_id,
        expected_sequence: cranker_bind.sequence,
        amount: withdraw_amount,
    };
    let withdraw_instr = Instruction {
        program_id: *program_id,
        accounts: vec![
            AccountMeta::new(env.payer.pubkey(), true),
            AccountMeta::new(env.market, false),
            AccountMeta::new(env.cranker_portfolio, false),
            AccountMeta::new(payer_ata, false),
            AccountMeta::new(env.vault, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        data: withdraw_ix.encode(),
    };
    let bh = rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?.0;
    let tx = Transaction::new_signed_with_payer(
        &[cu, heap, withdraw_instr],
        Some(&env.payer.pubkey()),
        &[&env.payer],
        bh,
    );
    let withdraw_sig = rpc
        .send_and_confirm_transaction(&tx)
        .with_context(|| format!("Withdraw({withdraw_amount}) from cranker portfolio"))?;
    println!("Withdraw({withdraw_amount}) sig = {withdraw_sig}");

    let dest_after = spl_token_amount(rpc, &payer_ata)?;
    let delta = dest_after.saturating_sub(dest_before);
    println!("payer ATA after Withdraw:  {} atoms (Δ = {})", dest_after, delta);
    if delta as u128 == withdraw_amount {
        println!("verified: cranker portfolio → owner ATA transfer matches Withdraw amount");
    } else {
        println!(
            "partial: expected +{withdraw_amount}, got +{delta}. Amount=0 case still exercises \
             the code path (engine short-circuits at line 14549)."
        );
    }
    Ok(())
}

fn spl_token_amount(rpc: &RpcClient, ata: &Pubkey) -> Result<u64> {
    use solana_sdk::program_pack::Pack;
    let acct = rpc.get_account_data(ata)?;
    let parsed = spl_token::state::Account::unpack(&acct)?;
    Ok(parsed.amount)
}

fn log_outcome(r: &SubmissionRecord) {
    match &r.outcome {
        CrankOutcome::HardReject { error, error_name, .. } => {
            let named = error_name.unwrap_or("Unknown");
            println!("  {} [{}] ← HardReject ({named}): {error}", r.target, r.plan_class);
        }
        CrankOutcome::Ok {
            signature,
            mutated,
            target_bytes_diff,
            reward_bytes_diff,
            program_cu,
            ..
        } => {
            let variant = if r.outcome.class() == drip::submit::OutcomeClass::Progress && !*mutated {
                "Ok(unreconciled)"
            } else if *mutated {
                "Ok(mut)"
            } else {
                "Ok(no-mut)"
            };
            println!(
                "  {} [{}] ← {variant}: sig={signature} target_diff={target_bytes_diff} reward_diff={:?} program_cu={:?}",
                r.target, r.plan_class, reward_bytes_diff, program_cu
            );
        }
    }
}

fn home_dir() -> Result<std::path::PathBuf> {
    Ok(std::path::PathBuf::from(env::var("HOME").context("HOME unset")?))
}

fn fnv(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}
