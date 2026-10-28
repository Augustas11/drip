//! The shared crank cycle (`cycle::run_one_cycle`) on a real validator,
//! through Live and then Resolved.
//!
//! The binary's dry run only exercises a Live market, and the lifecycle test
//! closes portfolios with its own owner-signed helper. This test makes the
//! KEEPER's Resolved route do the work: after `ResolveMarket`, only
//! `run_one_cycle` cranks the funded portfolio, and the test asserts that
//! the route ran (`resolved:` plan class, Progress), closed the portfolio,
//! and paid the owner's capital to the owner's token account.
//!
//! Validator-gated: run with
//! `PERCOLATOR_TEST_RPC=http://127.0.0.1:8899 PERCOLATOR_PROGRAM_ID=<id>
//!  cargo test --test cycle_resolved -- --include-ignored`.

mod common;

use anyhow::{ensure, Result};
use drip::cycle::{self, CrankCtx, CuStats, SubmissionRecord};
use drip::observation;
use drip::quarantine::{Quarantine, QuarantineConfig};
use drip::scheduler::CycleBudget;
use drip::setup;
use drip::submit::OutcomeClass;
use solana_sdk::program_pack::Pack;
use solana_sdk::signature::Signer;

const DEPOSIT: u64 = 1_000_000; // setup.rs deposits this into the user portfolio

fn token_balance(rpc: &solana_client::rpc_client::RpcClient, ata: &solana_sdk::pubkey::Pubkey) -> Result<u64> {
    Ok(spl_token::state::Account::unpack(&rpc.get_account_data(ata)?)?.amount)
}

#[test]
#[ignore = "needs a local validator: set PERCOLATOR_TEST_RPC + PERCOLATOR_PROGRAM_ID, run with --include-ignored"]
fn run_one_cycle_live_then_resolved_route_pays_owner() -> Result<()> {
    let (rpc, program_id) = common::rpc_and_program_from_env()
        .ok_or_else(|| anyhow::anyhow!("PERCOLATOR_TEST_RPC / PERCOLATOR_PROGRAM_ID not set"))?;
    let env = setup::bootstrap_with_fee(&rpc, &program_id, common::load_payer()?, 0)?;
    let pyth = observation::empty_pyth_resolver();
    let ctx = CrankCtx {
        rpc: &rpc,
        program_id: &program_id,
        payer: &env.payer,
        market: &env.market,
        reward_portfolio: Some(&env.cranker_portfolio),
        pyth: &pyth,
        exclude: &[],
    };
    let budget = CycleBudget::default();
    let mut q = Quarantine::new(QuarantineConfig::default());
    let mut records: Vec<SubmissionRecord> = Vec::new();
    let mut cu = CuStats::default();

    // Live: one cycle, nothing may fail.
    let s = cycle::run_one_cycle(&ctx, 0, &budget, &mut q, &mut records, &mut cu)?;
    eprintln!("live: {}", s.line());
    ensure!(s.failed == 0, "live cycle failed: {:?}", s.failures);
    ensure!(s.txs >= 1, "live cycle sent nothing");

    // Resolve; force_close_delay_slots is 0 on this market, so the Resolved
    // crank is permissionless and the keeper (not the owner) may close.
    common::resolve_market(&rpc, &program_id, &env)?;
    let owner_ata =
        spl_associated_token_account::get_associated_token_address(&env.user.pubkey(), &env.mint);
    let before = token_balance(&rpc, &owner_ata)?;

    let first_resolved = records.len();
    for c in 1..=8u64 {
        let s = cycle::run_one_cycle(&ctx, c, &budget, &mut q, &mut records, &mut cu)?;
        eprintln!("resolved: {}", s.line());
        ensure!(s.failed == 0, "resolved cycle failed: {:?}", s.failures);
        if common::portfolio_is_closed(&rpc, &env.user_portfolio)? {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }

    ensure!(
        common::portfolio_is_closed(&rpc, &env.user_portfolio)?,
        "the keeper's Resolved route did not close the user portfolio"
    );
    let route_progress = records[first_resolved..]
        .iter()
        .filter(|r| r.target == env.user_portfolio)
        .filter(|r| r.plan_class.starts_with("resolved:"))
        .filter(|r| r.outcome.class() == OutcomeClass::Progress)
        .count();
    ensure!(route_progress >= 1, "no Progress on a resolved: plan class for the user portfolio");
    let paid = token_balance(&rpc, &owner_ata)? - before;
    ensure!(paid == DEPOSIT, "owner paid {paid}, expected the full deposit {DEPOSIT}");
    eprintln!("keeper closed the portfolio via the Resolved route; owner paid {paid}");
    Ok(())
}
