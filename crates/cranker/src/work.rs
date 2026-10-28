//! Off-chain "how urgent is this account" estimate, used only to **order**
//! candidates inside a cycle. It never decides that an account can be
//! skipped: the engine's selector is the only authority on what a crank
//! does, and the scheduler still probes idle-looking accounts when budget
//! allows.
//!
//! Funding (engine #227, spec "Funding accrual and F settlement carry
//! sub-unit value"): F settlement pays `floor(|basis| · (F_target − f_snap) /
//! (a_basis · POS_SCALE))` and leaves a remainder worth less than one atom.
//! A leg in that state is **F-settled**. Counting it as pending work would
//! make the cranker loop on an account the engine considers done, so
//! `leg_f_settled` is the only F test used here. There is no standalone
//! funding apply/settle entry point upstream; funding settles inside refresh.
//!
//! Caveat: the sub-atom rule is engine #227, which is in `68859b94` (preview)
//! but not in the default pin `4db11a8c`, where some close paths still need
//! `f_snap == F_target` exactly. That only matters for ordering here: quiet
//! accounts are still probed.

use percolator::{PortfolioAccountV16Account, PortfolioLegV16Account, POS_SCALE};
use percolator_prog::state::MarketGroupV16;

/// Scheduling priority, lowest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Certified liquidation deficit or liquidation lock held.
    Liquidatable = 0,
    /// Stale / B-stale flags, or a leg whose K/F snapshot trails its asset.
    Pending = 1,
    /// Nothing visible off-chain; still probed if budget allows.
    Quiet = 2,
}

impl Priority {
    pub fn label(self) -> &'static str {
        match self {
            Priority::Liquidatable => "liquidatable",
            Priority::Pending => "pending",
            Priority::Quiet => "quiet",
        }
    }
}

/// `true` when a leg's unpaid funding remainder is worth less than one atom.
///
/// Overflow handling is exact, not a guess:
/// * the gap (`f_target − f_snap`) or the numerator `|basis| · gap`
///   overflowing means the payment is huge → **not settled**;
/// * the denominator `a_basis · POS_SCALE` overflowing means it exceeds any
///   u128 numerator, so the floor is exactly 0 → **settled**.
///
/// Only a non-negative gap can floor to zero: the engine floors F deltas
/// toward −∞, so any negative gap is at least a one-atom debit and is work.
pub fn leg_f_settled(basis_pos_q: i128, a_basis: u128, f_snap: i128, f_target: i128) -> bool {
    if basis_pos_q == 0 || a_basis == 0 {
        return true;
    }
    let Some(gap) = f_target.checked_sub(f_snap) else {
        return false;
    };
    if gap == 0 {
        return true;
    }
    if gap < 0 {
        return false;
    }
    let Some(num) = basis_pos_q.unsigned_abs().checked_mul(gap as u128) else {
        return false;
    };
    // Overflowing denominator: the true value exceeds u128::MAX >= num, so
    // floor(num / den) is exactly 0. Settled is the exact answer, not a guess.
    let Some(den) = a_basis.checked_mul(POS_SCALE) else {
        return true;
    };
    num / den == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortfolioWork {
    pub priority: Priority,
    /// Active legs with K/epoch drift or ≥ 1 atom of unpaid funding.
    pub pending_legs: usize,
    /// Active legs whose only gap is a sub-atom F remainder (not work).
    pub f_subatom_legs: usize,
}

fn leg_pending(leg: &PortfolioLegV16Account, group: &MarketGroupV16) -> (bool, bool) {
    let Some(asset) = group.assets.get(leg.asset_index.get() as usize) else {
        return (true, false);
    };
    let long = leg.side == 0;
    let (k, f, kf_epoch) = if long {
        (asset.k_long, asset.f_long_num, asset.kf_epoch_long)
    } else {
        (asset.k_short, asset.f_short_num, asset.kf_epoch_short)
    };
    if leg.stale != 0 || leg.b_stale != 0 || leg.kf_epoch_snap.get() != kf_epoch {
        return (true, false);
    }
    let f_settled = leg_f_settled(leg.basis_pos_q.get(), leg.a_basis.get(), leg.f_snap.get(), f);
    let k_settled = leg.k_snap.get() == k;
    let subatom_only = k_settled && f_settled && leg.f_snap.get() != f;
    (!(k_settled && f_settled), subatom_only)
}

pub fn assess(portfolio: &PortfolioAccountV16Account, group: &MarketGroupV16) -> PortfolioWork {
    let mut pending_legs = 0;
    let mut f_subatom_legs = 0;
    for leg in portfolio.legs.iter().filter(|l| l.active != 0) {
        let (pending, subatom) = leg_pending(leg, group);
        pending_legs += pending as usize;
        f_subatom_legs += subatom as usize;
    }
    let priority = if portfolio.liquidation_lock != 0
        || portfolio.health_cert.certified_liq_deficit.get() != 0
    {
        Priority::Liquidatable
    } else if portfolio.stale_state != 0 || portfolio.b_stale_state != 0 || pending_legs > 0 {
        Priority::Pending
    } else {
        Priority::Quiet
    };
    PortfolioWork {
        priority,
        pending_legs,
        f_subatom_legs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subatom_f_gap_is_settled() {
        // |basis| = 1 unit (POS_SCALE), a_basis = 1e15 (ADL_ONE scale-ish):
        // gap < a_basis pays floor(<1) = 0 atoms → settled.
        let a = 1_000_000_000_000_000u128;
        let basis = POS_SCALE as i128;
        assert!(leg_f_settled(basis, a, 0, (a - 1) as i128));
        assert!(!leg_f_settled(basis, a, 0, a as i128));
        assert!(!leg_f_settled(-basis, a, 0, a as i128), "short basis uses |basis|");
    }

    #[test]
    fn zero_gap_and_flat_legs_are_settled_negative_gap_is_not() {
        assert!(leg_f_settled(5, 7, 10, 10));
        assert!(!leg_f_settled(5, 7, 10, 9), "floor toward -inf: a debit");
        assert!(leg_f_settled(0, 7, 0, i128::MAX));
        assert!(leg_f_settled(5, 0, 0, i128::MAX));
    }

    #[test]
    fn overflowing_gap_is_not_settled() {
        assert!(!leg_f_settled(i128::MAX, 1, i128::MIN, i128::MAX));
    }

    #[test]
    fn priority_orders_liquidatable_first() {
        assert!(Priority::Liquidatable < Priority::Pending);
        assert!(Priority::Pending < Priority::Quiet);
    }
}
