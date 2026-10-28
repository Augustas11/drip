//! Hard-reject rollback property.
//!
//! Mirrors the harness's `run_with_svm_rollback` error branch: when a crank
//! is rejected, state must be byte-identical to before the submit. On-chain
//! the SVM guarantees the rollback; this test confirms the wrapper actually
//! aborts cleanly (no partial write) and that `submit.rs` classifies the
//! failure as `CrankOutcome::HardReject` rather than a silent `Ok`.
//!
//! Deterministic fixture: two freshly bootstrapped markets A and B. Cranking
//! A's portfolio against market B must be rejected (the portfolio's
//! provenance names market A). An accepted submission fails the test.
//!
//! Validator-gated: `#[ignore]` so a plain `cargo test` reports it as
//! ignored, not passed. Run with
//! `PERCOLATOR_TEST_RPC=http://127.0.0.1:8899 PERCOLATOR_PROGRAM_ID=<id>
//!  cargo test --test hard_reject -- --include-ignored`.

mod common;

use drip::setup;
use drip::submit::{submit_crank, CrankOutcome};

#[test]
#[ignore = "needs a local validator: set PERCOLATOR_TEST_RPC + PERCOLATOR_PROGRAM_ID, run with --include-ignored"]
fn hard_reject_leaves_target_bytes_unchanged() -> anyhow::Result<()> {
    let (rpc, program_id) = common::rpc_and_program_from_env()
        .ok_or_else(|| anyhow::anyhow!("PERCOLATOR_TEST_RPC / PERCOLATOR_PROGRAM_ID not set"))?;
    let a = setup::bootstrap_with_fee(&rpc, &program_id, common::load_payer()?, 0)?;
    let b = setup::bootstrap_with_fee(&rpc, &program_id, common::load_payer()?, 0)?;

    let target = a.user_portfolio;
    let pre_target = rpc.get_account_data(&target)?;
    let pre_market_b = rpc.get_account_data(&b.market)?;

    let outcome = submit_crank(
        &rpc,
        &program_id,
        &common::load_payer()?,
        &b.market, // wrong market for this portfolio
        &target,
        None,
        &[],
        rpc.get_slot()?,
    )?;

    let post_target = rpc.get_account_data(&target)?;
    let post_market_b = rpc.get_account_data(&b.market)?;
    match outcome {
        CrankOutcome::HardReject { error, error_name, .. } => {
            eprintln!("hard reject as expected: {error_name:?} ({error})");
        }
        other => panic!("cross-market crank was accepted: {other:?}"),
    }
    assert_eq!(pre_target, post_target, "rejected crank must not mutate the target");
    assert_eq!(pre_market_b, post_market_b, "rejected crank must not mutate the market");
    Ok(())
}
