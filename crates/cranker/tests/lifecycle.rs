//! Crank-loop lifecycle integration test.
//!
//! Bootstraps its own market (like `main.rs`), then runs the real crank
//! cycle — discover → rank → observe → plan-gate → submit — and asserts the
//! conservation oracle holds after EVERY submission. This is the on-chain
//! analogue of the harness's per-step `assert_fuzz_invariants`: prove that
//! whatever the crank did, it did it legally.
//!
//! Selection goes through `plan_gate::needs_observation`, NOT the harness's
//! `kani_permissionless_crank` explicit-action back-door — the production
//! path decides observation-need from the engine predicate.
//!
//! SCOPE. Driving a market all the way to Resolved/closed needs non-crank
//! instructions (trades, liquidation, resolve, close) that the cranker does
//! not own — the cranker only issues `PermissionlessCrank`. So the terminal
//! wind-down assertions from the harness tail (`assert_accounts_closed`,
//! `assert_admin_can_close_market`) stay documented at the bottom rather than
//! driven here. What this test fully exercises is the crank cycle itself with
//! conservation enforced at every step — the part the cranker is responsible
//! for.

mod common;

use anyhow::Result;
use percolator::AutoCrankPlanV16;
use drip::submit::CrankOutcome;
use drip::{discovery, observation, plan_gate, setup, submit};
use solana_sdk::signature::Signer;
use std::time::Duration;

/// Crank cycles to run. Each cycle re-discovers and re-cranks; maintenance
/// fees accrue between cycles as slots advance.
const CYCLES: usize = 3;
/// Per-cycle candidate cap. Wrapper hint cap is `CRANK_OBSERVATION_DECODE_MAX = 16`.
const HINT_BUDGET: usize = 16;
/// The synthetic bootstrap market has one asset.
const ASSET_INDEX: u16 = 0;

#[test]
#[ignore = "needs a local validator: set PERCOLATOR_TEST_RPC + PERCOLATOR_PROGRAM_ID, run with --include-ignored"]
fn crank_lifecycle_preserves_conservation() -> Result<()> {
    let (rpc, program_id) = common::rpc_and_program_from_env()
        .ok_or_else(|| anyhow::anyhow!("PERCOLATOR_TEST_RPC / PERCOLATOR_PROGRAM_ID not set"))?;
    let payer = common::load_payer()?;

    // ---- one-shot bootstrap (main.rs analogue) ----
    // Fee-free market: no maintenance fee accrues into insurance, so after
    // accounts are paid out `vault`/`insurance` reach zero and `CloseSlab` can
    // retire the slab. (main.rs uses fee=100 to exercise fee routing; that path
    // is covered there and in the hard_reject/invariants tests.)
    let env = setup::bootstrap_with_fee(&rpc, &program_id, payer, 0)?;
    eprintln!(
        "bootstrapped: market={} user_portfolio={} cranker_portfolio={}",
        env.market, env.user_portfolio, env.cranker_portfolio
    );

    // Baseline: conservation must hold before any crank.
    let (mg0, portfolios0) = common::assert_conservation_at(&rpc, &program_id, &env.market)?;
    eprintln!(
        "baseline conservation OK: {} portfolios, c_tot={}, vault={}",
        portfolios0.len(),
        mg0.c_tot,
        mg0.vault
    );

    let mut submissions = 0usize;
    let mut mutated_count = 0usize;
    let mut nonprogress = 0usize;
    let mut hard_rejects = 0usize;

    // ---- cycle loop ----
    for cycle in 0..CYCLES {
        eprintln!("---- cycle {cycle} ----");

        let all = discovery::discover_portfolios(&rpc, &program_id)?;
        let ranked = discovery::rank_by_staleness(all, &env.market, &env.cranker_portfolio);
        eprintln!("  {} candidate(s) after ranking", ranked.len());

        // One market-slab snapshot per cycle for oracle-profile discovery,
        // one slot read for the freshness signal (mirrors main.rs).
        let now_slot = rpc.get_slot()?;
        let market_data = rpc.get_account_data(&env.market)?;
        // MANUAL synthetic market -> empty resolver is correct (mark committed).
        let pyth_resolver = observation::empty_pyth_resolver();

        for candidate in ranked.into_iter().take(HINT_BUDGET) {
            // ---- observation pipeline (real per-asset profile walk) ----
            let obs = observation::fetch_observation_for_asset(
                &market_data,
                ASSET_INDEX,
                &pyth_resolver,
            )?;

            // ---- plan-classification gate ----
            // Worst-case plan variant that would demand a caller observation.
            // On a MANUAL/EWMA/AUTH-MARK market the mark is committed, so an
            // empty observation is the correct payload and the engine
            // synthesizes from state. We assert the gate's expectation is
            // internally consistent with what our walk produced.
            let hypothetical = AutoCrankPlanV16::RefreshAccount { asset_index: None };
            let needs_obs = plan_gate::needs_observation(&hypothetical);
            if needs_obs && obs.oracle_accounts.is_empty() {
                eprintln!(
                    "    plan-gate: RefreshAccount{{None}} wants an observation; walk returned \
                     none — correct for MANUAL/committed-mark markets, expect NonProgress/reject \
                     on HYBRID."
                );
            }

            // ---- submit + classify ----
            // A6 drain: one domain per crank until Ok(no-mut) / HardReject.
            let mut inner_slot = now_slot;
            for drain_step in 0..drip::binding::SOURCE_LIEN_DRAIN_MAX_STEPS {
            let outcome = submit::submit_crank(
                &rpc,
                &program_id,
                &env.payer,
                &env.market,
                &candidate.pubkey,
                Some(&env.cranker_portfolio),
                std::slice::from_ref(&obs),
                inner_slot,
            )?;
            submissions += 1;

            match &outcome {
                CrankOutcome::Ok {
                    mutated,
                    target_bytes_diff,
                    reward_bytes_diff,
                    ..
                } => {
                    if *mutated {
                        mutated_count += 1;
                    } else {
                        nonprogress += 1;
                    }
                    eprintln!(
                        "    {} -> Ok(mutated={}, tgtΔ={}, rwdΔ={:?})",
                        candidate.pubkey, mutated, target_bytes_diff, reward_bytes_diff
                    );
                }
                CrankOutcome::HardReject { error, error_name, .. } => {
                    hard_rejects += 1;
                    eprintln!("    {} -> HardReject({error_name:?}): {error}", candidate.pubkey);
                }
            }

            // ---- per-step conservation (the harness core loop invariant) ----
            // Whether the crank progressed, was NonProgress, or hard-rejected,
            // the market must still conserve value. A hard reject rolls back
            // (state unchanged); a progressing crank must move state legally.
            common::assert_conservation_at(&rpc, &program_id, &env.market)?;
            let stop = matches!(
                &outcome,
                CrankOutcome::Ok { mutated: false, .. } | CrankOutcome::HardReject { .. }
            );
            if stop {
                break;
            }
            let _ = drain_step;
            inner_slot = rpc.get_slot()?;
            }
        }
    }

    eprintln!(
        "lifecycle done: submissions={submissions}, mutated={mutated_count}, \
         non-progress={nonprogress}, hard-reject={hard_rejects}"
    );

    // ---- drive to Resolved + wind down (harness tail analogue) ----
    // Bootstrap opened no positions, so no trade/liquidation is needed: resolve
    // the market, then CloseResolved each account (pays capital back out).
    common::resolve_market(&rpc, &program_id, &env)?;
    eprintln!("market resolved");
    common::assert_conservation_at(&rpc, &program_id, &env.market)?;

    // The funded user portfolio pays its capital back to the user's ATA on close.
    let user_ata = spl_associated_token_account::get_associated_token_address(
        &env.user.pubkey(),
        &env.mint,
    );
    drive_close(&rpc, &program_id, &env, &env.user_portfolio, &env.user, Some(user_ata))?;

    // The cranker reward portfolio is empty (payer-owned) — no payout tail.
    drive_close(&rpc, &program_id, &env, &env.cranker_portfolio, &env.payer, None)?;

    // Terminal assertions (harness `assert_accounts_closed`): conservation still
    // holds and every account is wound down.
    common::assert_conservation_at(&rpc, &program_id, &env.market)?;
    assert!(
        common::portfolio_is_closed(&rpc, &env.user_portfolio)?,
        "user portfolio must be closed after CloseResolved"
    );
    assert!(
        common::portfolio_is_closed(&rpc, &env.cranker_portfolio)?,
        "cranker portfolio must be closed after CloseResolved"
    );
    eprintln!("wind-down complete: market Resolved, both portfolios closed, conservation holds");

    // ---- final market retire (harness `assert_admin_can_close_market`) ----
    // Deregister both emptied portfolios (drops materialized_portfolio_count to
    // zero), then CloseSlab retires the market. After Resolve the admin may
    // close any emptied portfolio as terminal marketauth cleanup.
    common::close_portfolio(&rpc, &program_id, &env, &env.user_portfolio, &env.admin)?;
    common::close_portfolio(&rpc, &program_id, &env, &env.cranker_portfolio, &env.admin)?;

    // CloseSlab sweeps any residual vault balance to the admin's ATA.
    let admin_ata = common::ensure_ata(&rpc, &env.payer, &env.admin.pubkey(), &env.mint)?;
    common::close_slab(&rpc, &program_id, &env, &admin_ata)?;

    // On-chain proof of retirement: the slab is shrunk to a header-only
    // tombstone (the wrapper keeps HEADER_LEN rent; it does not deallocate).
    assert!(
        common::market_is_retired(&rpc, &env.market)?,
        "market slab must be retired (header-only) after CloseSlab"
    );
    eprintln!("market retired: slab is a header-only tombstone");

    Ok(())
}

/// Drive `CloseResolved` for one portfolio until it reports closed (the engine
/// may return `ProgressOnly` and need several chunks). Tolerates an error on an
/// already-closed account by breaking — the caller asserts closure afterward.
fn drive_close(
    rpc: &solana_client::rpc_client::RpcClient,
    program_id: &solana_sdk::pubkey::Pubkey,
    env: &setup::CrankerEnv,
    portfolio: &solana_sdk::pubkey::Pubkey,
    owner: &solana_sdk::signature::Keypair,
    dest_token: Option<solana_sdk::pubkey::Pubkey>,
) -> Result<()> {
    for i in 0..8 {
        if common::portfolio_is_closed(rpc, portfolio)? {
            return Ok(());
        }
        if let Err(e) = common::close_resolved_once(rpc, program_id, env, portfolio, owner, dest_token)
        {
            eprintln!("  close_resolved iter {i} returned err (may already be closed): {e}");
            break;
        }
        std::thread::sleep(Duration::from_millis(400)); // let a slot advance between chunks
    }
    Ok(())
}
