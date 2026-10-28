//! Invariant oracle smoke test.
//!
//! Loads the live market + all its portfolios and runs the conservation
//! checklist once. This is the test to run FIRST when bringing up a market:
//! if it fails, either (a) the market genuinely violates conservation (a real
//! bug — escalate), or (b) `common::PORTFOLIO_ENGINE_OFF` is wrong and the
//! decode is reading garbage. Rule out (b) before believing (a).

mod common;

#[test]
#[ignore = "needs a local validator: set PERCOLATOR_TEST_RPC + PERCOLATOR_PROGRAM_ID (+ optional PERCOLATOR_TEST_MARKET), run with --include-ignored"]
fn market_snapshot_satisfies_conservation() -> anyhow::Result<()> {
    // PERCOLATOR_TEST_MARKET names an existing market to check; without it
    // the test bootstraps its own, so it never silently skips.
    let live = match common::live_from_env() {
        Some(live) => live,
        None => {
            let (rpc, program_id) = common::rpc_and_program_from_env().ok_or_else(|| {
                anyhow::anyhow!("PERCOLATOR_TEST_RPC / PERCOLATOR_PROGRAM_ID not set")
            })?;
            let env = drip::setup::bootstrap_with_fee(
                &rpc,
                &program_id,
                common::load_payer()?,
                0,
            )?;
            common::Live { rpc, program_id, market: env.market }
        }
    };

    let (mg, portfolios) = common::assert_conservation_live(&live)?;
    eprintln!(
        "conservation OK: {} portfolios, c_tot={}, vault={}, insurance={}, pnl_pos_tot={}",
        portfolios.len(),
        mg.c_tot,
        mg.vault,
        mg.insurance,
        mg.pnl_pos_tot,
    );
    Ok(())
}
