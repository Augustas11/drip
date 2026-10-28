//! Resolved-mode crank hints.
//!
//! Since wrapper `5cb331dd`, a `PermissionlessCrank` (tag 5) on a Resolved
//! market may carry **at most one** hint, with `oracle_accounts == 0` and
//! `asset_index < max_market_slots` (the wrapper's `src/v16_program.rs`,
//! `handle_permissionless_crank` → `handle_close_resolved`). The hint is
//! discovery-only: the wrapper turns it into
//! `AutoCrankObservationV16 { asset_index, effective_price: 0, funding_rate_e9: 0 }`
//! and the engine either advances that asset's backing bucket (expiry) or
//! returns `EngineNonProgress` atomically. `CloseResolved` is a compatibility
//! alias for the same crank.
//!
//! So the cranker's job is only to name the asset whose backing bucket is
//! due. A wrong guess costs a fee and comes back as `EngineNonProgress`,
//! which `submit::OutcomeClass` treats as idle, never as a failure.

use percolator::{BackingBucketStatusV16, MarketModeV16};
use percolator_prog::state::MarketGroupV16;

use crate::observation::FetchedObservation;

/// Effective expiry of a backing bucket once the market is Resolved.
///
/// Engine #224 (in `68859b94`, not yet in the wrapper-main pin `4db11a8c`)
/// caps every bucket at `resolved_slot + H` with
/// `H = max(max_accrual_dt_slots, h_max, max_bankrupt_close_lifetime_slots, 1)`.
/// Taking the min is safe on both pins: on `4db11a8c` an early hint just
/// returns NonProgress.
pub fn resolved_bucket_deadline(group: &MarketGroupV16, expiry_slot: u64) -> u64 {
    let c = &group.config;
    let horizon = c
        .max_accrual_dt_slots
        .max(c.h_max)
        .max(c.max_bankrupt_close_lifetime_slots)
        .max(1);
    expiry_slot.min(group.resolved_slot.saturating_add(horizon))
}

/// The asset to name in the single Resolved-mode hint at `now_slot`, if any.
///
/// Domains are `asset_index * 2 + side`. Only `Fresh` buckets qualify: the
/// engine's hint path (`first_lapsed_source_backing_for_asset_at_slot`) acts
/// on lapsed Fresh buckets only. Expiry turns a liened Fresh bucket into
/// `Impaired` and keeps its old `expiry_slot`, so counting Impaired here
/// would pin the hint on a bucket the engine has already handled.
///
/// Which deadline? Engine `4db11a8c` (the default pin) checks the **raw**
/// `expiry_slot <= now` (the engine's `src/v16.rs` ~9546). Engine `68859b94` also
/// caps it at `resolved_slot + H` (#224). So:
/// 1. assets with a bucket lapsed by its raw expiry come first: every
///    engine rev acts on those;
/// 2. only if there are none, assets due by the horizon cap (the preview
///    engine acts on these; the default one answers NonProgress, i.e. idle);
/// 3. within the chosen set, `rotation` (the cycle number) picks the asset
///    round-robin, so several due assets are served in turn and a wrong
///    guess on one can never starve another.
pub fn resolved_backing_hint(group: &MarketGroupV16, now_slot: u64, rotation: u64) -> Option<u16> {
    if group.mode != MarketModeV16::Resolved {
        return None;
    }
    let slots = group.config.max_market_slots as usize;
    let fresh: Vec<(usize, u64)> = group
        .source_backing_buckets
        .iter()
        .enumerate()
        .filter(|(domain, b)| domain / 2 < slots && b.status == BackingBucketStatusV16::Fresh)
        .map(|(domain, b)| (domain / 2, b.expiry_slot))
        .collect();
    let mut due: Vec<usize> = fresh
        .iter()
        .filter(|(_, expiry)| *expiry <= now_slot)
        .map(|(a, _)| *a)
        .collect();
    if due.is_empty() {
        due = fresh
            .iter()
            .filter(|(_, expiry)| resolved_bucket_deadline(group, *expiry) <= now_slot)
            .map(|(a, _)| *a)
            .collect();
    }
    due.sort_unstable();
    due.dedup();
    if due.is_empty() {
        return None;
    }
    u16::try_from(due[(rotation as usize) % due.len()]).ok()
}

/// Hints to attach to a crank on a Resolved market: zero or one
/// oracle-less observation.
pub fn resolved_crank_hints(group: &MarketGroupV16, now_slot: u64, rotation: u64) -> Vec<FetchedObservation> {
    resolved_backing_hint(group, now_slot, rotation)
        .map(|asset_index| FetchedObservation {
            asset_index,
            oracle_accounts: Vec::new(),
        })
        .into_iter()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use percolator::BackingBucketV16;
    use percolator_prog::ix::{CrankObservationHint, Instruction as ProgInstruction};

    fn group_with(buckets: &[(usize, BackingBucketStatusV16, u64)]) -> MarketGroupV16 {
        let mut config = percolator::V16Config::public_user_fund(4, 1, 8);
        config.max_bankrupt_close_lifetime_slots = 16;
        let mut g = MarketGroupV16::new([7u8; 32], config).expect("group");
        g.mode = MarketModeV16::Resolved;
        g.resolved_slot = 1_000;
        for &(domain, status, expiry_slot) in buckets {
            g.source_backing_buckets[domain] = BackingBucketV16 {
                market_id: 1,
                fresh_unliened_backing_num: 1,
                status,
                expiry_slot,
                ..BackingBucketV16::EMPTY
            };
        }
        g
    }

    #[test]
    fn no_hint_outside_resolved() {
        let mut g = group_with(&[(2, BackingBucketStatusV16::Fresh, 10)]);
        g.mode = MarketModeV16::Live;
        assert_eq!(resolved_backing_hint(&g, 1_000_000, 0), None);
    }

    #[test]
    fn raw_lapsed_assets_are_served_in_rotation() {
        // asset 1 short (domain 3) lapsed at 50, asset 2 long (domain 4) at 40,
        // asset 0 long (domain 0) far in the future.
        let g = group_with(&[
            (0, BackingBucketStatusV16::Fresh, 5_000_000),
            (3, BackingBucketStatusV16::Fresh, 50),
            (4, BackingBucketStatusV16::Fresh, 40),
        ]);
        assert_eq!(resolved_backing_hint(&g, 60, 0), Some(1));
        assert_eq!(resolved_backing_hint(&g, 60, 1), Some(2));
        assert_eq!(resolved_backing_hint(&g, 60, 2), Some(1));
        assert_eq!(resolved_backing_hint(&g, 45, 0), Some(2));
        assert_eq!(resolved_backing_hint(&g, 45, 1), Some(2));
        // Nothing raw-lapsed and the horizon cap (resolved_slot 1000 + H) not reached.
        assert_eq!(resolved_backing_hint(&g, 39, 0), None);
    }

    /// Review finding (default pin 4db11a8c checks the RAW expiry): once
    /// past the horizon cap, a far-dated bucket on a lower asset ties with
    /// a genuinely lapsed one. The lapsed one must win on every cycle, or
    /// the hint lands NonProgress forever and the lapsed bucket never expires.
    #[test]
    fn raw_lapsed_bucket_beats_horizon_tie_on_default_pin() {
        let g = group_with(&[
            (0, BackingBucketStatusV16::Fresh, 5_000_000), // asset 0, not lapsed
            (5, BackingBucketStatusV16::Fresh, 1_500),     // asset 2, lapsed
        ]);
        let now = 2_000;
        assert!(resolved_bucket_deadline(&g, 5_000_000) <= now, "both are horizon-due");
        for rotation in 0..4 {
            assert_eq!(resolved_backing_hint(&g, now, rotation), Some(2));
        }
    }

    #[test]
    fn expired_and_empty_buckets_are_ignored() {
        let g = group_with(&[
            (0, BackingBucketStatusV16::Expired, 10),
            (2, BackingBucketStatusV16::Empty, 10),
        ]);
        assert_eq!(resolved_backing_hint(&g, 1_000_000, 0), None);
    }

    /// Review finding: an Impaired bucket keeps its old (earliest) expiry.
    /// It must not shadow a Fresh bucket that is actually due.
    #[test]
    fn impaired_bucket_does_not_shadow_due_fresh_bucket() {
        let g = group_with(&[
            (1, BackingBucketStatusV16::Impaired, 10),
            (7, BackingBucketStatusV16::Fresh, 50),
        ]);
        assert_eq!(resolved_backing_hint(&g, 60, 0), Some(3));
    }

    #[test]
    fn resolved_horizon_caps_far_expiry() {
        let g = group_with(&[(6, BackingBucketStatusV16::Fresh, u64::MAX)]);
        let deadline = resolved_bucket_deadline(&g, u64::MAX);
        assert!(deadline > g.resolved_slot && deadline < u64::MAX);
        // Never raw-lapsed: only the horizon-cap fallback names it (the
        // preview engine acts on it; the default pin answers NonProgress).
        assert_eq!(resolved_backing_hint(&g, deadline - 1, 0), None);
        assert_eq!(resolved_backing_hint(&g, deadline, 0), Some(3));
    }

    /// Wire: the resolved hint is an ordinary tag-5 hint with zero oracle
    /// accounts, which is exactly what the wrapper's resolved branch accepts.
    #[test]
    fn resolved_hint_encodes_as_single_oracle_less_tag5_hint() {
        let g = group_with(&[(3, BackingBucketStatusV16::Fresh, 10)]);
        let hints = resolved_crank_hints(&g, 20, 0);
        assert_eq!(hints.len(), 1);
        let ix = ProgInstruction::PermissionlessCrank {
            now_slot: 20,
            observations: hints
                .iter()
                .map(|h| CrankObservationHint {
                    asset_index: h.asset_index,
                    oracle_accounts: h.oracle_account_count(),
                })
                .collect(),
        };
        let bytes = ix.encode();
        // tag | now_slot | n=1 | asset_index u16 = 1 | oracle_accounts = 0
        assert_eq!(bytes[0], 5);
        assert_eq!(bytes[9], 1);
        assert_eq!(u16::from_le_bytes([bytes[10], bytes[11]]), 1);
        assert_eq!(bytes[12], 0);
        assert_eq!(bytes.len(), 13);
    }
}
