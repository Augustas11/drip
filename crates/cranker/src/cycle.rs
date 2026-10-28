//! One discover → rank → hint → schedule → submit cycle, shared by the
//! binary (`main.rs`) and the integration tests.
//!
//! Per candidate:
//! * **Live / Recovery market:** one hint per active leg asset (falling back
//!   to asset 0), each with the oracle accounts that asset's profile needs.
//! * **Resolved market:** zero or one oracle-less hint naming the asset whose
//!   backing bucket is due (`resolved::resolved_crank_hints`).
//!
//! The A6 drain (one source domain per call) runs inside one scheduler
//! attempt, bounded by `SOURCE_LIEN_DRAIN_MAX_STEPS`.

use anyhow::{anyhow, Context, Result};
use percolator::MarketModeV16;
use percolator_prog::state::{read_market, read_portfolio, MarketGroupV16};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{Keypair, Signer},
};

use crate::binding::SOURCE_LIEN_DRAIN_MAX_STEPS;
use crate::discovery;
use crate::observation::{self, FetchedObservation, PythFeedResolver};
use crate::quarantine::Quarantine;
use crate::resolved;
use crate::scheduler::{self, Attempt, Candidate, CycleBudget, CycleSummary};
use crate::submit::{self, CrankOutcome, CrankRoute, OutcomeClass};
use crate::work::{self, Priority};

/// Wrapper cap: `CRANK_OBSERVATION_DECODE_MAX = 16`.
pub const HINT_BUDGET: usize = 16;

pub struct CrankCtx<'a> {
    pub rpc: &'a RpcClient,
    pub program_id: &'a Pubkey,
    pub payer: &'a Keypair,
    pub market: &'a Pubkey,
    pub reward_portfolio: Option<&'a Pubkey>,
    pub pyth: &'a PythFeedResolver,
    /// Accounts never cranked (e.g. our own reward portfolio).
    pub exclude: &'a [Pubkey],
}

/// One submission, for the mirror-custody table and CU stats.
#[derive(Debug)]
pub struct SubmissionRecord {
    pub cycle: u64,
    pub target: Pubkey,
    pub plan_class: String,
    pub outcome: CrankOutcome,
}

/// CU observations across a run.
#[derive(Debug, Default, Clone, Copy)]
pub struct CuStats {
    /// Smallest program CU seen on an `Ok(no-mut)` crank.
    pub noop_min: Option<u64>,
    /// Whole-tx CU for that same smallest no-op crank.
    pub noop_tx: Option<u64>,
    /// Largest program CU seen on any crank that landed.
    pub worst_ok: Option<u64>,
    /// Largest program CU seen on a reverted crank (preflight logs).
    pub worst_reject: Option<u64>,
}

impl CuStats {
    pub fn observe(&mut self, o: &CrankOutcome) {
        let Some(cu) = o.program_cu() else { return };
        match o {
            CrankOutcome::Ok { mutated, tx_cu, unreconciled, .. } => {
                // An unreconciled outcome's effect is unknown: never a no-op sample.
                if !mutated && unreconciled.is_none() && self.noop_min.map_or(true, |m| cu < m) {
                    self.noop_min = Some(cu);
                    self.noop_tx = *tx_cu;
                }
                self.worst_ok = Some(self.worst_ok.map_or(cu, |m| m.max(cu)));
            }
            CrankOutcome::HardReject { .. } => {
                self.worst_reject = Some(self.worst_reject.map_or(cu, |m| m.max(cu)));
            }
        }
    }
}

fn plan_hints(
    group: &MarketGroupV16,
    market_data: &[u8],
    portfolio_data: &[u8],
    pyth: &PythFeedResolver,
    now_slot: u64,
    rotation: u64,
) -> Result<(Vec<FetchedObservation>, &'static str)> {
    if group.mode == MarketModeV16::Resolved {
        let hints = resolved::resolved_crank_hints(group, now_slot, rotation);
        let class = if hints.is_empty() {
            "resolved:close"
        } else {
            "resolved:backing-hint"
        };
        return Ok((hints, class));
    }
    let mut assets: Vec<u16> = read_portfolio(portfolio_data)
        .map(|p| {
            p.legs
                .iter()
                .filter(|l| l.active != 0)
                .map(|l| l.asset_index.get() as u16)
                .collect()
        })
        .unwrap_or_default();
    assets.sort_unstable();
    assets.dedup();
    if assets.is_empty() {
        assets.push(0);
    }
    assets.truncate(HINT_BUDGET);
    let hints = assets
        .into_iter()
        .map(|a| observation::fetch_observation_for_asset(market_data, a, pyth))
        .collect::<Result<Vec<_>>>()?;
    let class = if group.mode == MarketModeV16::Recovery {
        "recovery"
    } else {
        "live"
    };
    Ok((hints, class))
}

/// CU an attempt is charged per tx: the measured whole-tx CU, or the full
/// requested limit when it could not be read. This is the same amount the
/// scheduler's meter keeps reserved, so the summary and the budget agree.
fn outcome_cu(o: &CrankOutcome) -> u64 {
    o.tx_cu().unwrap_or(submit::TX_CU_LIMIT as u64)
}

/// Fold one candidate's drain into a scheduler attempt. `rpc_error` is an
/// RPC failure that ended the drain early; steps that already landed still
/// count as progress.
/// Why a drain stopped without an outcome for its last tx.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Interrupt {
    /// The deadline passed before the send: nothing went on chain.
    Deferred,
    /// Sent, fate unknown.
    Unconfirmed(String),
}

fn attempt_from(
    outcomes: &[CrankOutcome],
    rpc_error: Option<&str>,
    interrupt: Option<&Interrupt>,
) -> Attempt {
    let cu: u64 = outcomes.iter().map(outcome_cu).sum();
    let progressed = outcomes.iter().filter(|o| o.class() == OutcomeClass::Progress).count();
    if let Some(Interrupt::Unconfirmed(signature)) = interrupt {
        if progressed == 0 {
            return Attempt::Unconfirmed { signature: signature.clone() };
        }
    }
    if progressed > 0 {
        if let Some(o) = outcomes.last().filter(|o| o.class() == OutcomeClass::Failed) {
            println!(
                "  note: progressed {progressed} step(s) then failed: {}",
                o.error_key().unwrap_or_default()
            );
        }
        return Attempt::Progress {
            cu,
            steps: progressed,
        };
    }
    if let Some(e) = rpc_error {
        return Attempt::Failed {
            error: format!("Rpc: {e}"),
            cu,
        };
    }
    match outcomes.last() {
        Some(o) if o.class() == OutcomeClass::Failed => Attempt::Failed {
            error: o.error_key().unwrap_or_else(|| "unknown".into()),
            cu,
        },
        None if interrupt == Some(&Interrupt::Deferred) => Attempt::Deferred,
        _ => Attempt::Idle { cu },
    }
}

/// What the cycle decided to do with one candidate.
enum Plan {
    Submit {
        route_owner: Option<Pubkey>,
        hints: Vec<FetchedObservation>,
    },
    /// Resolved market inside `force_close_delay_slots`: the owner must sign,
    /// so a third-party crank would revert `Unauthorized`. Idle, no tx.
    OwnerWindow,
    /// Could not build hints for this account. Failed, no tx.
    HintError(String),
}

/// Run one full cycle against `ctx.market`.
pub fn run_one_cycle(
    ctx: &CrankCtx<'_>,
    cycle: u64,
    budget: &CycleBudget,
    quarantine: &mut Quarantine,
    records: &mut Vec<SubmissionRecord>,
    cu: &mut CuStats,
) -> Result<CycleSummary> {
    // The wall budget covers the whole cycle, discovery and planning included.
    let started = std::time::Instant::now();
    let all = discovery::discover_portfolios(ctx.rpc, ctx.program_id)?;
    let reward = ctx.reward_portfolio.copied().unwrap_or_default();
    let ranked: Vec<_> = discovery::rank_by_staleness(all, ctx.market, &reward)
        .into_iter()
        .filter(|p| !ctx.exclude.contains(&p.pubkey))
        .collect();

    let market_data = ctx
        .rpc
        .get_account_data(ctx.market)
        .context("read market slab")?;
    let (cfg, group) = read_market(&market_data).map_err(|e| anyhow!("read_market: {e:?}"))?;
    let now_slot = ctx.rpc.get_slot()?;

    // Resolved-route accounts (payout to the owner's ATA).
    let resolved = group.mode == MarketModeV16::Resolved;
    let mint = Pubkey::new_from_array(cfg.collateral_mint);
    let (vault_authority, _) =
        Pubkey::find_program_address(&[b"vault", ctx.market.as_ref()], ctx.program_id);
    let vault = spl_associated_token_account::get_associated_token_address(&vault_authority, &mint);
    let owner_window = resolved
        && cfg.force_close_delay_slots != 0
        && now_slot.saturating_sub(group.resolved_slot) < cfg.force_close_delay_slots;
    let payer = ctx.payer.pubkey();

    let mut plans = std::collections::HashMap::new();
    let mut candidates = Vec::with_capacity(ranked.len());
    for p in &ranked {
        let decoded = read_portfolio(&p.account.data).ok();
        let priority = decoded
            .as_ref()
            .map(|pf| work::assess(pf, &group).priority)
            .unwrap_or(Priority::Pending);
        // Still owed work: persistent idle on such an account is reported as
        // stuck, not hidden as "idle".
        // * Resolved: capital/PnL left, or a payout receipt not yet fully paid.
        // * Live/Recovery: the off-chain estimate says it is liquidatable or
        //   pending, yet the engine keeps answering NonProgress / no-mut.
        let owed = decoded.as_ref().is_some_and(|pf| {
            if resolved {
                let r = &pf.resolved_payout_receipt;
                pf.capital.get() != 0
                    || pf.pnl.get() != 0
                    || pf.reserved_pnl.get() != 0
                    || (r.present != 0
                        && r.finalized == 0
                        && r.paid_effective.get() < r.terminal_positive_claim_face.get())
            } else {
                priority != Priority::Quiet
            }
        });
        let owner = p.owner().unwrap_or_default();
        let (plan, plan_class) =
            match plan_hints(&group, &market_data, &p.account.data, ctx.pyth, now_slot, cycle) {
                Err(e) => (Plan::HintError(format!("HintError: {e}")), "hint-error".to_string()),
                Ok(_) if owner_window && owner != payer => {
                    (Plan::OwnerWindow, "resolved:owner-window".to_string())
                }
                Ok((hints, class)) => {
                    let plan_class = match (class, hints.first()) {
                        ("resolved:backing-hint", Some(h)) => format!("{class}({})", h.asset_index),
                        ("live" | "recovery", _) => format!("{class}:{}", priority.label()),
                        _ => class.to_string(),
                    };
                    let route_owner = resolved.then_some(owner);
                    (Plan::Submit { route_owner, hints }, plan_class)
                }
            };
        let pending_work = owed && matches!(plan, Plan::Submit { .. });
        plans.insert(p.pubkey, plan);
        candidates.push(Candidate {
            pubkey: p.pubkey,
            priority,
            plan_class,
            pending_work,
        });
    }

    let summary = scheduler::run_cycle(cycle, now_slot, started, candidates, budget, quarantine, |c, ab| {
        let (route, hints) = match plans.get(&c.pubkey) {
            Some(Plan::Submit { route_owner: None, hints }) => (
                CrankRoute::Live {
                    reward_portfolio: ctx.reward_portfolio,
                },
                hints.as_slice(),
            ),
            Some(Plan::Submit {
                route_owner: Some(owner),
                hints,
            }) => (
                CrankRoute::Resolved {
                    owner: *owner,
                    mint,
                    vault,
                    vault_authority,
                },
                hints.as_slice(),
            ),
            Some(Plan::OwnerWindow) | None => return Ok(Attempt::Idle { cu: 0 }),
            Some(Plan::HintError(e)) => return Ok(Attempt::Failed { error: e.clone(), cu: 0 }),
        };
        let mut outcomes = Vec::new();
        let mut rpc_error = None;
        let mut interrupt = None;
        let mut slot = now_slot;
        // Every tx is admitted by the budget first (wall, CU reserve, tx
        // count, per-attempt cap) and counted whatever its outcome.
        for _ in 0..SOURCE_LIEN_DRAIN_MAX_STEPS {
            if !ab.begin_tx() {
                break;
            }
            let deadline = std::time::Instant::now() + ab.time_left();
            let o = match submit::submit_crank_routed(
                ctx.rpc,
                ctx.program_id,
                ctx.payer,
                ctx.market,
                &c.pubkey,
                &route,
                hints,
                slot,
                Some(deadline),
            ) {
                Ok(o) => o,
                Err(e) if e.is::<submit::Unconfirmed>() => {
                    // Sent, fate unknown: keep the reservation charged, stop
                    // this drain, report it, and do not strike the account.
                    ab.settle_tx(None);
                    let sig = e
                        .downcast_ref::<submit::Unconfirmed>()
                        .map(|u| u.signature.to_string())
                        .unwrap_or_default();
                    // Reported whatever the attempt's overall outcome.
                    ab.note_unconfirmed(sig.clone());
                    interrupt = Some(Interrupt::Unconfirmed(sig));
                    break;
                }
                Err(e) if e.is::<submit::DeadlineReached>() => {
                    // Not sent: refund the reservation and stop. This is the
                    // budget running out (deferral), not an account outcome.
                    ab.settle_tx(Some(0));
                    interrupt = Some(Interrupt::Deferred);
                    break;
                }
                Err(e) => {
                    // Unknown CU: the full reservation stays charged.
                    ab.settle_tx(None);
                    rpc_error = Some(e.to_string());
                    break;
                }
            };
            // Settle on the WHOLE tx (compute-budget and ATA ixs included);
            // unknown keeps the full reservation.
            ab.settle_tx(o.tx_cu());
            cu.observe(&o);
            // Keep draining only while the TARGET moved (A6 one domain per
            // call); a reward-portfolio byte diff alone is not drain progress.
            let keep_draining =
                matches!(&o, CrankOutcome::Ok { target_bytes_diff, .. } if *target_bytes_diff > 0);
            outcomes.push(o);
            if !keep_draining {
                break;
            }
            match ctx.rpc.get_slot() {
                Ok(s) => slot = s,
                Err(e) => {
                    rpc_error = Some(e.to_string());
                    break;
                }
            }
        }
        let attempt = attempt_from(&outcomes, rpc_error.as_deref(), interrupt.as_ref());
        for o in outcomes {
            records.push(SubmissionRecord {
                cycle,
                target: c.pubkey,
                plan_class: c.plan_class.clone(),
                outcome: o,
            });
        }
        Ok(attempt)
    });
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::signature::Signature;

    fn ok(mutated: bool, cu: u64) -> CrankOutcome {
        CrankOutcome::Ok {
            signature: Signature::default(),
            pre_state_hash: 0,
            post_state_hash: 0,
            mutated,
            target_bytes_diff: 0,
            reward_bytes_diff: None,
            market_bytes_diff: None,
            program_cu: Some(cu),
            tx_cu: Some(cu),
            unreconciled: None,
        }
    }

    fn rej(name: &'static str) -> CrankOutcome {
        CrankOutcome::HardReject {
            error: String::new(),
            error_name: Some(name),
            program_cu: Some(10),
            tx_cu: Some(10),
        }
    }

    #[test]
    fn drain_then_idle_is_progress() {
        let a = attempt_from(&[ok(true, 100), ok(true, 100), ok(false, 18)], None, None);
        assert_eq!(a, Attempt::Progress { cu: 218, steps: 2 });
    }

    #[test]
    fn non_progress_revert_alone_is_idle() {
        assert_eq!(attempt_from(&[rej("EngineNonProgress")], None, None), Attempt::Idle { cu: 10 });
    }

    #[test]
    fn engine_error_alone_is_failed() {
        assert_eq!(
            attempt_from(&[rej("EngineCounterUnderflow")], None, None),
            Attempt::Failed {
                error: "EngineCounterUnderflow".into(),
                cu: 10
            }
        );
    }

    /// Review finding: an RPC error after a landed step must not erase the
    /// progress or strike the account.
    #[test]
    fn rpc_error_after_progress_is_still_progress() {
        assert_eq!(
            attempt_from(&[ok(true, 100)], Some("timeout"), None),
            Attempt::Progress { cu: 100, steps: 1 }
        );
        assert_eq!(
            attempt_from(&[], Some("timeout"), None),
            Attempt::Failed {
                error: "Rpc: timeout".into(),
                cu: 0
            }
        );
    }

    #[test]
    fn unknown_cu_is_charged_the_full_tx_limit() {
        let mut o = ok(false, 0);
        if let CrankOutcome::Ok { tx_cu, .. } = &mut o {
            *tx_cu = None;
        }
        assert_eq!(
            attempt_from(&[o], None, None),
            Attempt::Idle { cu: submit::TX_CU_LIMIT as u64 },
            "same amount the scheduler meter keeps reserved"
        );
    }

    /// Review finding: a confirmed tx whose post-state read failed must stay
    /// a confirmed outcome (Progress), never a failure strike.
    #[test]
    fn landed_tx_with_unreadable_post_state_is_progress() {
        let mut o = ok(false, 20_000);
        if let CrankOutcome::Ok { unreconciled, .. } = &mut o {
            *unreconciled = Some("read target portfolio post-state: timeout".into());
        }
        assert_eq!(o.class(), OutcomeClass::Progress);
        assert_eq!(attempt_from(&[o], None, None), Attempt::Progress { cu: 20_000, steps: 1 });
    }

    #[test]
    fn cu_stats_split_noop_and_worst() {
        let mut s = CuStats::default();
        s.observe(&ok(false, 18_000));
        s.observe(&ok(true, 300_000));
        s.observe(&ok(false, 17_975));
        s.observe(&rej("EngineStale"));
        assert_eq!(s.noop_min, Some(17_975));
        assert_eq!(s.worst_ok, Some(300_000));
        assert_eq!(s.worst_reject, Some(10));
    }
}
