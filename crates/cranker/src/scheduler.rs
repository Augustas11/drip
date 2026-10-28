//! One crank cycle: order candidates, spend a bounded budget, never let one
//! account starve the rest.
//!
//! * **Order.** Each priority class (liquidatable, pending, quiet) is
//!   rotated by the cycle number, then the classes are **interleaved** in a
//!   fixed 2:1:1 pattern. A steady stream of liquidatable accounts therefore
//!   cannot starve pending or quiet ones: every class gets a share of every
//!   cycle's budget.
//! * **Budget.** Wall-clock (measured from cycle entry, discovery included),
//!   compute units and tx count. Enforced **per tx**: the submit closure
//!   gets an [`AttemptBudget`] and must call `begin_tx` before every tx,
//!   which checks every budget and reserves the worst-case CU
//!   (`submit::TX_CU_LIMIT`), then `settle_tx` with the measured CU. A tx is
//!   counted when it is begun, whatever its outcome. One attempt (an A6
//!   drain) is also capped at `max_txs_per_attempt`. What doesn't fit is
//!   *deferred*, and rotation reaches it first next cycle.
//! * **Isolation.** A failed submission (revert, engine error, RPC error) is
//!   recorded and the loop moves on. Quarantined accounts are skipped
//!   without spending budget.
//! * **Idle vs failed vs stuck.** `EngineNonProgress` or `Ok(no-mut)` is
//!   idle, not a failure. Idle that persists for an account the cycle
//!   flagged as still owed work (`Candidate::pending_work`) is reported as
//!   `idle_pending_stuck`, so a claim that never becomes payable doesn't
//!   hide behind "idle".
//!
//! The submit step is a closure so the same loop runs against a real
//! validator (`main.rs`, the integration tests) and in unit tests.

use solana_sdk::pubkey::Pubkey;
use std::time::{Duration, Instant};

use crate::quarantine::{FailureReport, Quarantine};
use crate::work::Priority;

#[derive(Debug, Clone)]
pub struct Candidate {
    pub pubkey: Pubkey,
    pub priority: Priority,
    /// What the cranker is attempting, e.g. `live:liquidatable` or
    /// `resolved:backing-hint(2)`. Logged with quarantine events.
    pub plan_class: String,
    /// The cycle believes this account is still owed work (e.g. a resolved
    /// claimant with capital or PnL left). Persistent idle on such an
    /// account is reported as stuck.
    pub pending_work: bool,
}

/// What one candidate's submission (including any A6 drain steps) came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attempt {
    Progress { cu: u64, steps: usize },
    Idle { cu: u64 },
    Failed { error: String, cu: u64 },
    /// Nothing was sent: the cycle budget ran out (e.g. the deadline passed
    /// before the send). Not account health: streaks are untouched and the
    /// account keeps its place for next cycle.
    Deferred,
    /// A tx was sent but its fate is unknown (confirmation errored, status
    /// unknown). Not blamed on the account; reported separately.
    Unconfirmed { signature: String },
}

#[derive(Debug, Clone, Copy)]
pub struct CycleBudget {
    pub max_wall: Duration,
    pub max_cu: u64,
    /// Max txs per cycle (A6 drain steps included).
    pub max_submissions: usize,
    /// Max txs one candidate may send in one cycle (bounds an A6 drain;
    /// the rest continues next cycle).
    pub max_txs_per_attempt: usize,
    /// CU reserved before each tx: the requested compute-unit limit, i.e.
    /// the most one tx can consume.
    pub cu_per_tx_reserve: u64,
    /// Consecutive idle cycles on a `pending_work` account before it is
    /// reported as stuck.
    pub idle_stuck_cycles: u64,
}

impl Default for CycleBudget {
    fn default() -> Self {
        Self {
            max_wall: Duration::from_secs(30),
            max_cu: 24_000_000,
            max_submissions: 64,
            max_txs_per_attempt: 4,
            cu_per_tx_reserve: crate::submit::TX_CU_LIMIT as u64,
            idle_stuck_cycles: 10,
        }
    }
}

impl CycleBudget {
    /// `CRANKER_CYCLE_MS`, `CRANKER_CYCLE_CU`, `CRANKER_CYCLE_MAX_TX`,
    /// `CRANKER_ATTEMPT_MAX_TX`, `CRANKER_IDLE_STUCK_CYCLES`.
    pub fn from_env() -> Self {
        let d = Self::default();
        let get = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        let max_cu = get("CRANKER_CYCLE_CU").unwrap_or(d.max_cu);
        // Each tx reserves the full compute-unit limit before it is sent, so
        // a smaller cycle budget would send nothing at all. Clamp, loudly.
        let max_cu = if max_cu < d.cu_per_tx_reserve {
            eprintln!(
                "warning: CRANKER_CYCLE_CU={max_cu} is below one tx's reservation ({}); clamping to it",
                d.cu_per_tx_reserve
            );
            d.cu_per_tx_reserve
        } else {
            max_cu
        };
        Self {
            max_wall: get("CRANKER_CYCLE_MS")
                .map(Duration::from_millis)
                .unwrap_or(d.max_wall),
            max_cu,
            max_submissions: get("CRANKER_CYCLE_MAX_TX")
                .map(|v| v as usize)
                .unwrap_or(d.max_submissions),
            max_txs_per_attempt: get("CRANKER_ATTEMPT_MAX_TX")
                .map(|v| (v as usize).max(1))
                .unwrap_or(d.max_txs_per_attempt),
            cu_per_tx_reserve: d.cu_per_tx_reserve,
            idle_stuck_cycles: get("CRANKER_IDLE_STUCK_CYCLES")
                .map(|v| v.max(1))
                .unwrap_or(d.idle_stuck_cycles),
        }
    }
}

/// Cycle-wide remaining budget.
#[derive(Debug)]
struct Meter {
    deadline: Instant,
    txs_left: usize,
    cu_left: u64,
    reserve: u64,
    per_attempt: usize,
}

impl Meter {
    fn can_send(&self) -> bool {
        self.txs_left > 0 && Instant::now() < self.deadline && self.cu_left >= self.reserve
    }
}

/// Budget view handed to one attempt. Call [`begin_tx`](Self::begin_tx)
/// before every tx (it returns `false` when any budget is exhausted) and
/// [`settle_tx`](Self::settle_tx) after it with the measured CU, if known.
#[derive(Debug)]
pub struct AttemptBudget<'m> {
    meter: &'m mut Meter,
    txs: usize,
    open_reserve: u64,
    /// Max txs for this attempt: the per-attempt cap, further limited to the
    /// class's remaining quota while other classes still have quota and
    /// candidates (so one class's drain can't eat another's share).
    cap: usize,
    /// Signatures sent in this attempt whose fate is unknown. Reported in
    /// the cycle summary whatever the attempt's overall outcome.
    unconfirmed: Vec<String>,
}

impl AttemptBudget<'_> {
    /// Admit one more tx: counts it immediately and reserves the worst-case
    /// CU. A tx that is begun is counted whatever happens to it afterwards.
    pub fn begin_tx(&mut self) -> bool {
        if self.open_reserve != 0 || self.txs >= self.cap || !self.meter.can_send() {
            return false;
        }
        self.meter.txs_left -= 1;
        self.meter.cu_left -= self.meter.reserve;
        self.open_reserve = self.meter.reserve;
        self.txs += 1;
        true
    }

    /// Replace the open reservation with the measured CU. `None` (unknown)
    /// keeps the whole reservation charged.
    pub fn settle_tx(&mut self, cu: Option<u64>) {
        let used = cu.map_or(self.open_reserve, |c| c.min(self.open_reserve));
        self.meter.cu_left += self.open_reserve - used;
        self.open_reserve = 0;
    }

    /// Wall-clock time left in the cycle. `cycle.rs` turns it into the
    /// per-send deadline that `submit_crank_routed` checks before every
    /// send, including blockhash retries.
    pub fn time_left(&self) -> Duration {
        self.meter.deadline.saturating_duration_since(Instant::now())
    }

    pub fn txs_sent(&self) -> usize {
        self.txs
    }

    /// Record a sent tx whose fate is unknown (reported even if earlier
    /// steps of the same attempt progressed).
    pub fn note_unconfirmed(&mut self, signature: String) {
        self.unconfirmed.push(signature);
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CycleSummary {
    pub cycle: u64,
    pub progressed: usize,
    pub idle: usize,
    pub failed: usize,
    pub quarantined_skipped: usize,
    pub budget_deferred: usize,
    pub newly_quarantined: usize,
    /// Txs begun this cycle (each counted once, whatever its outcome).
    pub txs: usize,
    /// CU charged this cycle: measured CU, or the full reserve when unknown.
    pub cu_spent: u64,
    pub max_attempt_cu: u64,
    /// `pending_work` accounts idle for `idle_stuck_cycles` or more.
    pub idle_pending_stuck: usize,
    pub failures: Vec<(Pubkey, String)>,
    /// Sent txs whose fate is unknown: (account, signature).
    pub unconfirmed: Vec<(Pubkey, String)>,
    pub progressed_accounts: Vec<Pubkey>,
    /// Accounts whose attempt was idle (including a no-tx owner-window skip).
    pub idle_accounts: Vec<Pubkey>,
}

impl CycleSummary {
    pub fn line(&self) -> String {
        format!(
            "cycle {} summary: progressed={} idle={} failed={} unconfirmed={} quarantined_skipped={} deferred={} new_quarantine={} idle_pending_stuck={} txs={} cu={} max_attempt_cu={}",
            self.cycle,
            self.progressed,
            self.idle,
            self.failed,
            self.unconfirmed.len(),
            self.quarantined_skipped,
            self.budget_deferred,
            self.newly_quarantined,
            self.idle_pending_stuck,
            self.txs,
            self.cu_spent,
            self.max_attempt_cu,
        )
    }
}

/// Tx-budget weights per priority class (liquidatable, pending, quiet).
const CLASS_WEIGHTS: [usize; 3] = [2, 1, 1];
/// Turn order between classes inside a cycle.
const CLASS_PATTERN: [Priority; 4] = [
    Priority::Liquidatable,
    Priority::Liquidatable,
    Priority::Pending,
    Priority::Quiet,
];

/// Cross-cycle fairness state. Per priority class, the pubkey of the last
/// candidate *visited*: next cycle that class starts at the first candidate
/// sorting after it (wrapping). Keyed by pubkey, not by position, so it
/// stays correct while accounts join, leave or change class between cycles:
/// an account deferred for lack of budget is first in its class next cycle.
#[derive(Debug, Default, Clone)]
pub struct ScheduleState {
    last_visited: [Option<Pubkey>; 3],
    /// Position in [`CLASS_PATTERN`] where the next cycle starts. Persisted
    /// so that a budget admitting only a few attempts still rotates the
    /// first turn across classes instead of always starting liquidatable.
    turn: usize,
}

/// Tx quota per class for one cycle: the cycle tx budget split 2:1:1 over
/// the **non-empty** classes (at least 1 tx each). A class past its quota
/// waits while any other class still has quota and candidates; leftover
/// budget then goes to whoever remains.
fn class_quotas(max_txs: usize, nonempty: [bool; 3]) -> [usize; 3] {
    let total: usize = (0..3).filter(|&c| nonempty[c]).map(|c| CLASS_WEIGHTS[c]).sum();
    let mut q = [0usize; 3];
    for c in 0..3 {
        if nonempty[c] && total > 0 {
            q[c] = (max_txs * CLASS_WEIGHTS[c] / total).max(1);
        }
    }
    q
}

/// Run one cycle. `started` is the cycle's entry time (discovery and
/// planning count against the wall budget). `submit` performs one
/// candidate's crank(s) within the given [`AttemptBudget`]; it must not
/// panic, and an `Err` is a failure of that candidate only.
#[allow(clippy::too_many_arguments)]
pub fn run_cycle<F>(
    cycle: u64,
    slot: u64,
    started: Instant,
    candidates: Vec<Candidate>,
    budget: &CycleBudget,
    quarantine: &mut Quarantine,
    mut submit: F,
) -> CycleSummary
where
    F: FnMut(&Candidate, &mut AttemptBudget<'_>) -> anyhow::Result<Attempt>,
{
    let mut s = CycleSummary {
        cycle,
        ..CycleSummary::default()
    };
    let mut meter = Meter {
        deadline: started + budget.max_wall,
        txs_left: budget.max_submissions,
        cu_left: budget.max_cu,
        reserve: budget.cu_per_tx_reserve,
        per_attempt: budget.max_txs_per_attempt.max(1),
    };

    // Per-class queues, each starting at its persistent cursor.
    let mut cands = candidates;
    cands.sort_by(|a, b| a.pubkey.to_bytes().cmp(&b.pubkey.to_bytes()));
    let last_visited = quarantine.schedule_state().last_visited;
    let mut turn = quarantine.schedule_state().turn;
    let mut queues: [std::collections::VecDeque<Candidate>; 3] = Default::default();
    for (c, p) in [Priority::Liquidatable, Priority::Pending, Priority::Quiet].into_iter().enumerate() {
        let mut class: Vec<Candidate> = cands.iter().filter(|x| x.priority == p).cloned().collect();
        if let Some(last) = last_visited[c] {
            // Sorted by pubkey: start at the first one after `last`.
            let start = class
                .iter()
                .position(|x| x.pubkey.to_bytes() > last.to_bytes())
                .unwrap_or(0);
            class.rotate_left(start);
        }
        queues[c] = class.into();
    }
    let quotas = class_quotas(budget.max_submissions, [0, 1, 2].map(|c| !queues[c].is_empty()));
    let mut used = [0usize; 3];
    let mut visited: [Option<Pubkey>; 3] = [None; 3];

    loop {
        // Next class: follow the pattern among classes with candidates left,
        // preferring classes still under quota.
        let has = |c: usize, q: &[std::collections::VecDeque<Candidate>; 3]| !q[c].is_empty();
        let any_under = (0..3).any(|c| has(c, &queues) && used[c] < quotas[c]);
        let mut pick = None;
        for k in 0..CLASS_PATTERN.len() {
            let pos = (turn + k) % CLASS_PATTERN.len();
            let c = CLASS_PATTERN[pos] as usize;
            if has(c, &queues) && (!any_under || used[c] < quotas[c]) {
                pick = Some((c, pos));
                break;
            }
        }
        let Some((c, pos)) = pick else { break };
        turn = (pos + 1) % CLASS_PATTERN.len();
        let cand = queues[c].pop_front().expect("non-empty");
        if quarantine.is_blocked(&cand.pubkey, cycle) {
            s.quarantined_skipped += 1;
            visited[c] = Some(cand.pubkey);
            continue;
        }
        if !meter.can_send() {
            // Out of budget: this and every remaining candidate is deferred
            // (quarantined ones still count as skipped). Not visited, so the
            // cursors leave them first in line next cycle, and the class turn
            // stays on this class so it opens the next cycle.
            turn = pos;
            s.budget_deferred += 1;
            for q in queues.iter_mut() {
                for rest in q.drain(..) {
                    if quarantine.is_blocked(&rest.pubkey, cycle) {
                        s.quarantined_skipped += 1;
                    } else {
                        s.budget_deferred += 1;
                    }
                }
            }
            break;
        }
        let prev_visited = visited[c];
        visited[c] = Some(cand.pubkey);
        let txs_before = meter.txs_left;
        let cu_before = meter.cu_left;
        let others_need = (0..3).any(|o| o != c && !queues[o].is_empty() && used[o] < quotas[o]);
        let cap = if others_need {
            meter.per_attempt.min(quotas[c].saturating_sub(used[c]).max(1))
        } else {
            meter.per_attempt
        };
        let mut ab = AttemptBudget {
            meter: &mut meter,
            txs: 0,
            open_reserve: 0,
            cap,
            unconfirmed: Vec::new(),
        };
        // An unsettled tx (the closure bailed mid-tx) keeps its whole
        // reservation charged: the meter was debited in `begin_tx`.
        let result = submit(&cand, &mut ab);
        let mut unconfirmed = std::mem::take(&mut ab.unconfirmed);
        let attempt = result.unwrap_or_else(|e| Attempt::Failed {
            error: format!("Rpc: {e}"),
            cu: 0,
        });
        // A callback may report an unknown tx through the outcome, the budget
        // note, or both: merge them, once each.
        if let Attempt::Unconfirmed { signature } = &attempt {
            if !unconfirmed.contains(signature) {
                unconfirmed.push(signature.clone());
            }
        }
        let sent = txs_before - meter.txs_left;
        used[c] += sent;
        s.txs += sent;
        // Charged = what the meter actually kept (measured CU, or the full
        // reservation when unknown), so the summary agrees with the budget.
        let charged = cu_before - meter.cu_left;
        s.cu_spent = s.cu_spent.saturating_add(charged);
        s.max_attempt_cu = s.max_attempt_cu.max(charged);
        for signature in unconfirmed {
            println!(
                "{}",
                serde_json::json!({
                    "event": "unconfirmed",
                    "account": cand.pubkey.to_string(),
                    "signature": signature,
                    "cycle": cycle,
                    "slot": slot,
                    "plan_class": cand.plan_class,
                })
            );
            s.unconfirmed.push((cand.pubkey, signature));
        }
        let deferred = attempt == Attempt::Deferred;
        record(&mut s, quarantine, budget, &cand, attempt, cycle, slot);
        if deferred {
            // Unsent because the cycle budget ran out mid-attempt: keep this
            // account (and its class) first in line next cycle, and stop the
            // cycle here so no later candidate can overwrite that.
            visited[c] = prev_visited;
            turn = pos;
            for q in queues.iter_mut() {
                for rest in q.drain(..) {
                    if quarantine.is_blocked(&rest.pubkey, cycle) {
                        s.quarantined_skipped += 1;
                    } else {
                        s.budget_deferred += 1;
                    }
                }
            }
            break;
        }
    }
    let st = quarantine.schedule_state_mut();
    for c in 0..3 {
        if visited[c].is_some() {
            st.last_visited[c] = visited[c];
        }
    }
    st.turn = turn;
    s
}

fn record(
    s: &mut CycleSummary,
    quarantine: &mut Quarantine,
    budget: &CycleBudget,
    cand: &Candidate,
    attempt: Attempt,
    cycle: u64,
    slot: u64,
) {
    match attempt {
        Attempt::Progress { .. } => {
            s.progressed += 1;
            s.progressed_accounts.push(cand.pubkey);
            quarantine.record_ok(&cand.pubkey);
        }
        Attempt::Idle { .. } => {
            s.idle += 1;
            s.idle_accounts.push(cand.pubkey);
            let streak = quarantine.record_idle(&cand.pubkey, cand.pending_work);
            if cand.pending_work && streak >= budget.idle_stuck_cycles {
                s.idle_pending_stuck += 1;
                if streak == budget.idle_stuck_cycles {
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "idle_with_pending_work",
                            "account": cand.pubkey.to_string(),
                            "cycle": cycle,
                            "slot": slot,
                            "plan_class": cand.plan_class,
                            "idle_cycles": streak,
                        })
                    );
                }
            }
        }
        Attempt::Deferred => {
            s.budget_deferred += 1;
        }
        // Signatures are reported via `AttemptBudget::note_unconfirmed`;
        // here only the account-health effect matters: none.
        Attempt::Unconfirmed { .. } => {}
        Attempt::Failed { error, .. } => {
            s.failed += 1;
            if let Some(ev) = quarantine.record_failure(FailureReport {
                account: &cand.pubkey,
                error: &error,
                cycle,
                slot,
                plan_class: &cand.plan_class,
            }) {
                println!("{}", ev.to_json_line());
                s.newly_quarantined += 1;
            }
            s.failures.push((cand.pubkey, error));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quarantine::QuarantineConfig;
    use std::collections::HashMap;

    fn cand(pk: Pubkey, priority: Priority) -> Candidate {
        Candidate {
            pubkey: pk,
            priority,
            plan_class: format!("live:{}", priority.label()),
            pending_work: false,
        }
    }

    fn pk(i: u8) -> Pubkey {
        Pubkey::new_from_array([i; 32])
    }

    /// One tx per attempt, settled with `cu`.
    fn one_tx(ab: &mut AttemptBudget<'_>, cu: u64, a: Attempt) -> anyhow::Result<Attempt> {
        if !ab.begin_tx() {
            return Ok(Attempt::Idle { cu: 0 });
        }
        ab.settle_tx(Some(cu));
        Ok(a)
    }

    fn run<F>(cycle: u64, cands: Vec<Candidate>, b: &CycleBudget, q: &mut Quarantine, f: F) -> CycleSummary
    where
        F: FnMut(&Candidate, &mut AttemptBudget<'_>) -> anyhow::Result<Attempt>,
    {
        run_cycle(cycle, 1_000 + cycle, Instant::now(), cands, b, q, f)
    }

    /// Deferred accounts are first in their class next cycle (cursors
    /// advance by the number visited, not by one).
    #[test]
    fn deferred_accounts_come_first_next_cycle() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { max_submissions: 2, ..CycleBudget::default() };
        let mut order = Vec::new();
        for cycle in 0..3 {
            let cands = (0..5u8).map(|i| cand(pk(i), Priority::Quiet)).collect();
            run(cycle, cands, &budget, &mut q, |c, ab| {
                order.push(c.pubkey.to_bytes()[0]);
                one_tx(ab, 1, Attempt::Idle { cu: 1 })
            });
        }
        assert_eq!(order, vec![0, 1, 2, 3, 4, 0]);
    }

    /// Audit finding: with 8 txs and 4-tx drains (quotas 4/2/2), a pending
    /// drain used to take 4 txs and leave quiet accounts nothing. A drain is
    /// now capped at its class's remaining quota while others still need it.
    #[test]
    fn drains_cannot_eat_another_class_quota() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { max_submissions: 8, max_txs_per_attempt: 4, ..CycleBudget::default() };
        let mut quiet_txs = 0usize;
        for cycle in 0..5 {
            let cands = vec![
                cand(pk(1), Priority::Liquidatable),
                cand(pk(2), Priority::Pending),
                cand(pk(3), Priority::Quiet),
            ];
            run(cycle, cands, &budget, &mut q, |c, ab| {
                let mut n = 0;
                while n < 4 && ab.begin_tx() {
                    ab.settle_tx(Some(1));
                    n += 1;
                }
                if c.priority == Priority::Quiet {
                    quiet_txs += n;
                }
                Ok(Attempt::Progress { cu: 1, steps: n })
            });
        }
        assert!(quiet_txs >= 5 * 2, "quiet got {quiet_txs} txs over 5 cycles");
    }

    /// Re-audit finding: a deadline hit before the send used to fall through
    /// to Idle (clearing history, bumping the idle streak, losing its place).
    /// Deferred touches no streak and keeps the account first next cycle;
    /// Unconfirmed is reported, never Idle or Failed.
    #[test]
    fn deferred_and_unconfirmed_are_not_account_health() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { idle_stuck_cycles: 1, ..CycleBudget::default() };
        let mut order = Vec::new();
        for cycle in 0..2 {
            let mut a = cand(pk(1), Priority::Quiet);
            a.pending_work = true;
            let cands = vec![a, cand(pk(2), Priority::Quiet)];
            let s = run(cycle, cands, &budget, &mut q, |c, _| {
                order.push(c.pubkey.to_bytes()[0]);
                Ok(match cycle {
                    0 => Attempt::Deferred,
                    _ => Attempt::Idle { cu: 0 },
                })
            });
            if cycle == 0 {
                // pk(1) defers, which ends the cycle: pk(2) is counted as
                // deferred, not run.
                assert_eq!((s.idle, s.failed, s.budget_deferred, s.unconfirmed.len()), (0, 0, 2, 0));
                assert_eq!(s.idle_pending_stuck, 0);
            }
        }
        // pk(1) deferred in cycle 0 and ended it; it is still first in cycle 1.
        assert_eq!(order, vec![1, 1, 2]);
    }

    /// Re-audit finding: a standalone Unconfirmed outcome (no budget note)
    /// must still be reported once, and must not touch account health.
    #[test]
    fn standalone_unconfirmed_is_reported_and_neutral() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { idle_stuck_cycles: 1, ..CycleBudget::default() };
        let mut a = cand(pk(1), Priority::Pending);
        a.pending_work = true;
        let s = run(0, vec![a.clone(), cand(pk(2), Priority::Pending)], &budget, &mut q, |c, ab| {
            Ok(if c.pubkey == pk(1) {
                Attempt::Unconfirmed { signature: "only-outcome".into() }
            } else {
                ab.note_unconfirmed("both".into());
                Attempt::Unconfirmed { signature: "both".into() }
            })
        });
        assert_eq!(
            s.unconfirmed,
            vec![(pk(1), "only-outcome".to_string()), (pk(2), "both".to_string())]
        );
        assert_eq!((s.idle, s.failed, s.idle_pending_stuck), (0, 0, 0));
    }

    /// Re-audit finding: an unconfirmed tail after earlier drain progress
    /// must still be reported, alongside the Progress.
    #[test]
    fn unconfirmed_tail_is_reported_even_after_progress() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let s = run(0, vec![cand(pk(1), Priority::Pending)], &CycleBudget::default(), &mut q, |_, ab| {
            assert!(ab.begin_tx());
            ab.settle_tx(Some(1));
            assert!(ab.begin_tx());
            ab.settle_tx(None);
            ab.note_unconfirmed("tail-sig".into());
            Ok(Attempt::Progress { cu: 1, steps: 1 })
        });
        assert_eq!(s.progressed, 1);
        assert_eq!(s.unconfirmed, vec![(pk(1), "tail-sig".to_string())]);
    }

    /// Audit finding: position cursors broke when class membership changed.
    /// With last-visited pubkeys, a deferred account is first next cycle even
    /// when a new account sorting before it joins.
    #[test]
    fn deferred_account_first_even_when_membership_changes() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { max_submissions: 2, ..CycleBudget::default() };
        let mut served = Vec::new();
        // cycle 0: 10, 11, 12 -> serves 10, 11; 12 deferred
        let c0 = [10u8, 11, 12].iter().map(|&i| cand(pk(i), Priority::Quiet)).collect();
        run(0, c0, &budget, &mut q, |c, ab| {
            served.push(c.pubkey.to_bytes()[0]);
            one_tx(ab, 1, Attempt::Idle { cu: 1 })
        });
        // cycle 1: a new account 5 sorts first; 12 must still be next
        let c1 = [5u8, 10, 11, 12].iter().map(|&i| cand(pk(i), Priority::Quiet)).collect();
        run(1, c1, &budget, &mut q, |c, ab| {
            served.push(c.pubkey.to_bytes()[0]);
            one_tx(ab, 1, Attempt::Idle { cu: 1 })
        });
        assert_eq!(served, vec![10, 11, 12, 5]);
    }

    /// Audit finding: with a 1-tx budget every cycle used to start (and end)
    /// with a liquidatable account. The class turn now persists across
    /// cycles, and a deferred class opens the next cycle.
    #[test]
    fn one_tx_budget_still_rotates_classes() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { max_submissions: 1, ..CycleBudget::default() };
        let mut hits: HashMap<Priority, usize> = HashMap::new();
        for cycle in 0..20 {
            let cands = vec![
                cand(pk(1), Priority::Liquidatable),
                cand(pk(2), Priority::Pending),
                cand(pk(3), Priority::Quiet),
            ];
            run(cycle, cands, &budget, &mut q, |c, ab| {
                *hits.entry(c.priority).or_default() += 1;
                one_tx(ab, 1, Attempt::Progress { cu: 1, steps: 1 })
            });
        }
        // One candidate per class: the pattern's second liquidatable slot
        // finds that class empty, so service evens out. What matters is
        // that no class is starved.
        for p in [Priority::Liquidatable, Priority::Pending, Priority::Quiet] {
            assert!(hits.get(&p).copied().unwrap_or(0) >= 6, "{p:?} starved: {hits:?}");
        }
    }

    /// Review probe: 64-tx budget, 40 liquidatable accounts draining 2 txs
    /// each, 200 pending. Every pending account must be served within a
    /// small multiple of the ideal 200 / (64 * 1/3) ≈ 10 cycles.
    #[test]
    fn pending_wait_is_bounded_under_liquidatable_drains() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { max_submissions: 64, ..CycleBudget::default() };
        let mut last_served: HashMap<Pubkey, u64> = HashMap::new();
        let mut worst_gap = 0u64;
        for cycle in 0..60u64 {
            let mut cands: Vec<Candidate> = (0..40u8).map(|i| cand(pk(i), Priority::Liquidatable)).collect();
            cands.extend((0..200u16).map(|i| {
                let mut b = [0u8; 32];
                b[0] = 1;
                b[1..3].copy_from_slice(&i.to_le_bytes());
                cand(Pubkey::new_from_array(b), Priority::Pending)
            }));
            run(cycle, cands, &budget, &mut q, |c, ab| {
                let n = if c.priority == Priority::Liquidatable { 2 } else { 1 };
                for _ in 0..n {
                    if !ab.begin_tx() {
                        break;
                    }
                    ab.settle_tx(Some(1));
                }
                if c.priority == Priority::Pending {
                    let prev = last_served.insert(c.pubkey, cycle).unwrap_or(0);
                    worst_gap = worst_gap.max(cycle - prev);
                }
                Ok(Attempt::Progress { cu: 1, steps: n })
            });
        }
        assert_eq!(last_served.len(), 200, "every pending account served");
        assert!(worst_gap <= 12, "worst pending wait {worst_gap} cycles");
    }

    /// Review probe: two 4-tx liquidatable drains with an 8-tx budget must
    /// not starve pending and quiet accounts.
    #[test]
    fn big_drains_leave_room_for_other_classes() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { max_submissions: 8, max_txs_per_attempt: 4, ..CycleBudget::default() };
        let mut hits: HashMap<Priority, usize> = HashMap::new();
        for cycle in 0..5 {
            let cands = vec![
                cand(pk(1), Priority::Liquidatable),
                cand(pk(2), Priority::Liquidatable),
                cand(pk(3), Priority::Pending),
                cand(pk(4), Priority::Quiet),
            ];
            run(cycle, cands, &budget, &mut q, |c, ab| {
                let mut n = 0;
                while (c.priority != Priority::Liquidatable || n < 4) && n < 4 && ab.begin_tx() {
                    ab.settle_tx(Some(1));
                    n += 1;
                    if c.priority != Priority::Liquidatable {
                        break;
                    }
                }
                *hits.entry(c.priority).or_default() += 1;
                Ok(Attempt::Progress { cu: 1, steps: n })
            });
        }
        assert_eq!(hits.get(&Priority::Pending), Some(&5), "pending served every cycle");
        assert_eq!(hits.get(&Priority::Quiet), Some(&5), "quiet served every cycle");
    }

    /// Review finding: sustained liquidatable load must not starve pending
    /// or quiet accounts.
    #[test]
    fn sustained_liquidatable_load_leaves_share_for_other_classes() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget {
            max_submissions: 12,
            ..CycleBudget::default()
        };
        let mut hits: HashMap<Priority, usize> = HashMap::new();
        for cycle in 0..10 {
            let mut cands: Vec<Candidate> = (0..30u8).map(|i| cand(pk(i), Priority::Liquidatable)).collect();
            cands.extend((100..110u8).map(|i| cand(pk(i), Priority::Pending)));
            cands.extend((200..210u8).map(|i| cand(pk(i), Priority::Quiet)));
            run(cycle, cands, &budget, &mut q, |c, ab| {
                *hits.entry(c.priority).or_default() += 1;
                one_tx(ab, 1, Attempt::Progress { cu: 1, steps: 1 })
            });
        }
        assert_eq!(hits[&Priority::Liquidatable], 60);
        assert_eq!(hits[&Priority::Pending], 30);
        assert_eq!(hits[&Priority::Quiet], 30);
    }

    /// Acceptance: a persistent per-account engine error gets quarantined
    /// while every other account keeps progressing, cycle after cycle.
    #[test]
    fn persistent_error_is_quarantined_and_others_progress() {
        let stuck = pk(0);
        let healthy: Vec<Pubkey> = (1..=5u8).map(pk).collect();
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget::default();
        let mut progressed: HashMap<Pubkey, usize> = HashMap::new();
        let mut stuck_attempts = 0usize;
        let cycles = 20u64;
        let mut quarantined_cycles = 0usize;
        for cycle in 0..cycles {
            let mut cands = vec![cand(stuck, Priority::Liquidatable)];
            cands.extend(healthy.iter().map(|p| cand(*p, Priority::Pending)));
            let s = run(cycle, cands, &budget, &mut q, |c, ab| {
                if c.pubkey == stuck {
                    stuck_attempts += 1;
                    one_tx(ab, 50_000, Attempt::Failed { error: "EngineCounterUnderflow".into(), cu: 50_000 })
                } else {
                    *progressed.entry(c.pubkey).or_default() += 1;
                    one_tx(ab, 40_000, Attempt::Progress { cu: 40_000, steps: 1 })
                }
            });
            assert_eq!(s.progressed, healthy.len(), "cycle {cycle}: healthy accounts starved");
            quarantined_cycles += s.quarantined_skipped;
        }
        for p in &healthy {
            assert_eq!(progressed[p], cycles as usize);
        }
        assert!(stuck_attempts < cycles as usize / 2, "stuck tried {stuck_attempts}x");
        assert!(quarantined_cycles > 0);
        assert!(q.events().len() >= 2, "re-quarantine with growing backoff");
        assert!(q.events()[1].backoff_cycles > q.events()[0].backoff_cycles);
    }

    #[test]
    fn idle_and_failed_are_counted_separately() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let cands = (0..3u8).map(|i| cand(pk(i), Priority::Quiet)).collect();
        let s = run(0, cands, &CycleBudget::default(), &mut q, |c, ab| match c.pubkey.to_bytes()[0] {
            0 => one_tx(ab, 18_000, Attempt::Idle { cu: 18_000 }),
            1 => one_tx(ab, 0, Attempt::Failed { error: "EngineStale".into(), cu: 0 }),
            _ => {
                assert!(ab.begin_tx());
                anyhow::bail!("connection reset")
            }
        });
        assert_eq!((s.progressed, s.idle, s.failed), (0, 1, 2));
        assert!(s.failures.iter().any(|(_, e)| e.starts_with("Rpc:")));
        assert_eq!(s.txs, 3, "the tx that errored is still counted");
        // The errored tx never settled: its whole reserve stays charged.
        assert_eq!(s.cu_spent, 18_000 + TX_RESERVE);
    }

    const TX_RESERVE: u64 = crate::submit::TX_CU_LIMIT as u64;

    /// A drain that would send 10 txs per attempt is stopped by each budget
    /// in turn: per-attempt cap, cycle tx cap, CU, wall clock.
    #[test]
    fn multi_step_drain_respects_every_budget() {
        let drain = |q: &mut Quarantine, budget: CycleBudget, started: Instant| {
            let cands: Vec<Candidate> = (0..4u8).map(|i| cand(pk(i), Priority::Pending)).collect();
            let mut per_attempt = Vec::new();
            let s = run_cycle(0, 0, started, cands, &budget, q, |_, ab| {
                let mut n = 0;
                while n < 10 && ab.begin_tx() {
                    ab.settle_tx(Some(1_000));
                    n += 1;
                }
                per_attempt.push(n);
                Ok(Attempt::Progress { cu: 1_000 * n as u64, steps: n })
            });
            (s, per_attempt)
        };
        let mut q = Quarantine::new(QuarantineConfig::default());

        let b = CycleBudget { max_txs_per_attempt: 3, ..CycleBudget::default() };
        let (s, per) = drain(&mut q, b, Instant::now());
        assert_eq!(per, vec![3, 3, 3, 3]);
        assert_eq!(s.txs, 12);

        let b = CycleBudget { max_txs_per_attempt: 10, max_submissions: 7, ..CycleBudget::default() };
        let (s, per) = drain(&mut q, b, Instant::now());
        assert_eq!(per, vec![7]);
        assert_eq!((s.txs, s.budget_deferred), (7, 3));

        // CU: each tx reserves TX_RESERVE up front; the budget fits 2 reserves.
        let b = CycleBudget { max_txs_per_attempt: 10, max_cu: 2 * TX_RESERVE, ..CycleBudget::default() };
        let (s, _) = drain(&mut q, b, Instant::now());
        assert!(s.cu_spent <= 2 * TX_RESERVE);
        assert!(s.txs >= 2, "settled CU is refunded, so later txs still fit");

        // Wall clock starts at cycle entry: an already-expired cycle sends nothing.
        let b = CycleBudget { max_wall: Duration::from_millis(1), ..CycleBudget::default() };
        let (s, per) = drain(&mut q, b, Instant::now() - Duration::from_secs(1));
        assert!(per.is_empty());
        assert_eq!((s.txs, s.budget_deferred), (0, 4));
    }

    /// Review finding: a claimant whose payout never becomes ready must not
    /// hide behind "idle" forever.
    #[test]
    fn persistent_idle_on_pending_work_is_reported() {
        let mut q = Quarantine::new(QuarantineConfig::default());
        let budget = CycleBudget { idle_stuck_cycles: 3, ..CycleBudget::default() };
        let mut stuck_counts = Vec::new();
        for cycle in 0..5 {
            let mut owed = cand(pk(1), Priority::Pending);
            owed.pending_work = true;
            let cands = vec![owed, cand(pk(2), Priority::Quiet)];
            let s = run(cycle, cands, &budget, &mut q, |_, ab| one_tx(ab, 1, Attempt::Idle { cu: 1 }));
            stuck_counts.push(s.idle_pending_stuck);
        }
        assert_eq!(stuck_counts, vec![0, 0, 1, 1, 1], "only the owed account, from cycle 3 on");
    }
}
