//! Plan-classification gate for the cranker submission loop.
//!
//! Single responsibility: **decide whether a planned `PermissionlessCrank`
//! submission needs a fresh authenticated observation in its account tail.**
//!
//! Source of truth: the upstream free function
//! [`percolator::auto_crank_plan_requires_caller_observation`]
//! (the engine's `src/v16.rs:187`, re-exported in the engine's `src/lib.rs:54`).
//! Backing proofs: `proof_v16_auto_crank_refresh_is_unique_observation_requiring_plan`
//! and `v16_auto_crank_progress_realizable_without_observation_for_every_class`
//! in `tests/proofs_v16.rs` / `tests/v16_spec_tests.rs`.
//!
//! **Why we don't hand-roll the match:** upstream's predicate is the only
//! place the engine's truth-table is pinned by proof. A copy here would
//! silently rot when a new `AutoCrankPlanV16` variant lands — the compiler
//! couldn't catch the drift because the new variant would just fall through
//! to the wrong arm. Always delegate.
//!
//! Usage from the cranker loop:
//! ```ignore
//! use percolator::AutoCrankPlanV16;
//! if plan_gate::needs_observation(&predicted_plan) {
//!     let oracle_accts = fetch_authenticated_observation(asset_idx)?;
//!     submit_crank(hint, oracle_accts);
//! } else {
//!     submit_crank(hint, vec![]); // no oracle tail; engine synthesizes from
//!                                  // committed asset_state via obs_or_current_asset
//! }
//! ```
//!
//! Same gate also governs retry safety: a "needs_observation = false"
//! plan may MUTATE on re-submission (Liquidate, SettleBChunk,
//! RefreshAccount{Some} all progress from committed state via
//! `obs_or_current_asset` at the engine's `src/v16.rs:11328`). Retry MUST re-read
//! portfolio state first.

use percolator::{auto_crank_plan_requires_caller_observation, AutoCrankPlanV16};

/// Thin re-export so call sites import one symbol, not two.
///
/// `true`  → cranker MUST fetch a fresh authenticated observation
///           (oracle accounts in the tx tail) before submitting.
///           Only `AutoCrankPlanV16::RefreshAccount { asset_index: None }`
///           currently lives in this bucket.
/// `false` → engine will synthesize an observation from committed
///           `asset_state` (`obs_or_current_asset`, funding_rate_e9: 0);
///           submission may proceed without an oracle tail, but the
///           call WILL mutate state and is NOT idempotent on retry.
#[inline]
pub fn needs_observation(plan: &AutoCrankPlanV16) -> bool {
    auto_crank_plan_requires_caller_observation(plan)
}

/// Retry-safety classification.
///
/// Mirrors `needs_observation` semantically but is named for the caller's
/// concern at retry time: "is it safe to blindly resubmit this exact tx?"
///
/// `true`  → safe to retry (only `RefreshAccount{None}` is idempotent-clean
///           in the absent-observation, NonProgress-no-mutation sense).
/// `false` → state-mutating progress on retry; cranker MUST re-read the
///           target portfolio post-state and re-decide before resubmitting.
#[inline]
#[allow(dead_code)] // exercised by the cranker crate, not this binary
pub fn blind_retry_is_safe(plan: &AutoCrankPlanV16) -> bool {
    // Identical to needs_observation today because the no-observation =
    // NonProgress invariant only survives for one variant. Keeping a
    // separate accessor so the call site reads at the right altitude;
    // collapse if upstream ever decouples the two concepts.
    auto_crank_plan_requires_caller_observation(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use percolator::PermissionlessRecoveryReasonV16;

    #[test]
    fn refresh_account_none_is_the_only_observation_requirer() {
        assert!(needs_observation(&AutoCrankPlanV16::RefreshAccount { asset_index: None }));
        assert!(!needs_observation(&AutoCrankPlanV16::RefreshAccount { asset_index: Some(0) }));
        assert!(!needs_observation(&AutoCrankPlanV16::SettleBChunk { asset_index: 0 }));
        assert!(!needs_observation(&AutoCrankPlanV16::Liquidate { asset_index: 0 }));
        assert!(!needs_observation(&AutoCrankPlanV16::DeclareRecovery {
            reason: PermissionlessRecoveryReasonV16::ActiveBankruptCloseCannotProgress,
        }));
        assert!(!needs_observation(&AutoCrankPlanV16::ReleaseSourceLiens));
        assert!(!needs_observation(&AutoCrankPlanV16::AdvanceClose));
        assert!(!needs_observation(&AutoCrankPlanV16::FinalizeRecovery));
        assert!(!needs_observation(&AutoCrankPlanV16::CloseResolved));
        assert!(!needs_observation(&AutoCrankPlanV16::NoAction));
    }

    #[test]
    fn blind_retry_safe_iff_needs_observation() {
        for plan in [
            AutoCrankPlanV16::RefreshAccount { asset_index: None },
            AutoCrankPlanV16::RefreshAccount { asset_index: Some(0) },
            AutoCrankPlanV16::SettleBChunk { asset_index: 0 },
            AutoCrankPlanV16::Liquidate { asset_index: 0 },
            AutoCrankPlanV16::ReleaseSourceLiens,
            AutoCrankPlanV16::AdvanceClose,
            AutoCrankPlanV16::FinalizeRecovery,
            AutoCrankPlanV16::CloseResolved,
            AutoCrankPlanV16::NoAction,
        ] {
            assert_eq!(needs_observation(&plan), blind_retry_is_safe(&plan));
        }
    }
}
