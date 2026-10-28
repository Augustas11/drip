//! Authenticated-observation discovery.
//!
//! Reads the market slab bytes, slices out `AssetOracleProfileV16` for the
//! target asset, and returns the list of oracle account pubkeys the wrapper
//! expects in the tx tail for a `PermissionlessCrank` hint.
//!
//! ## What the wrapper expects (source of truth)
//!
//! For asset `i`, the wrapper's per-asset profile at
//! the wrapper's `src/v16_program.rs:556` (struct `AssetOracleProfileV16`)
//! carries (`ASSET_ORACLE_PROFILE_LEN` = 424 bytes at the current pin):
//! * `oracle_mode: u8` — dispatch discriminant:
//!   * `ORACLE_MODE_MANUAL       = 0` — no oracle account; mark is stored.
//!   * `ORACLE_MODE_HYBRID_AFTER_HOURS = 1` — external oracle path; the
//!     wrapper consumes `oracle_leg_count` accounts from the tail and
//!     dispatches per-account on the account's `owner` (Pyth Receiver /
//!     Switchboard On-Demand / Chainlink Store).
//!   * `ORACLE_MODE_EWMA_MARK    = 2` — mark authority-pushed; no oracle
//!     account in the tail.
//!   * `ORACLE_MODE_AUTH_MARK    = 3` — mark authority-pushed; no oracle
//!     account in the tail.
//! * `oracle_leg_count: u8` — number of oracle accounts the wrapper will
//!   consume per hint for this asset. `CrankObservationHint.oracle_accounts`
//!   MUST equal this or the wrapper returns `InvalidInstruction (0x9)`
//!   (`src/v16_program.rs:10505-10508`).
//! * `oracle_leg_feeds: [[u8; 32]; ORACLE_LEG_CAP]` — per-leg feed
//!   identifiers. The **meaning depends on the source that leg uses**:
//!   * Pyth (Solana Receiver): the `[u8; 32]` is a **feed_id**, matched
//!     against the on-chain `PriceFeedMessage.feed_id` at `read_pyth_price_e6`
//!     (`:3679`). The account pubkey is NOT stored here — the cranker
//!     must discover it off-chain (typically a `feed_id → PriceUpdateV2
//!     account pubkey` map).
//!   * Switchboard On-Demand: the `[u8; 32]` is the **account pubkey**
//!     itself; the wrapper compares `price_ai.key.to_bytes() == expected`
//!     at `read_switchboard_price_e6:3777`.
//!   * Chainlink Store: same as Switchboard — the `[u8; 32]` IS the
//!     account pubkey (`read_chainlink_price_e6:3830`).
//!
//! ## Implication for the reference cranker
//!
//! **A cranker cannot fully derive Pyth account pubkeys from market state
//! alone.** For any HYBRID-mode asset backed by Pyth, the cranker needs an
//! out-of-band `PythFeedResolver` that returns the current PriceUpdateV2
//! account for a given `feed_id`. This mapping is
//! operator-supplied data.
//!
//! For Switchboard and Chainlink the account pubkey is byte-embedded in
//! the profile — no off-chain lookup needed. Their tradeoff is that the
//! account pubkey is baked at market config time and cannot rotate.

use anyhow::Result;
use percolator::MarketGroupV16HeaderAccount;
use percolator_prog::{
    constants::{
        ASSET_ORACLE_PROFILE_LEN, ASSET_ORACLE_WRAPPER_LEN, MARKET_ASSET_SLOT_LEN,
        MARKET_GROUP_LEN, MARKET_GROUP_OFF, ORACLE_LEG_CAP, ORACLE_MODE_AUTH_MARK,
        ORACLE_MODE_EWMA_MARK, ORACLE_MODE_HYBRID_AFTER_HOURS, ORACLE_MODE_MANUAL,
    },
    state::AssetOracleProfileV16,
};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;

/// Fetched oracle accounts for a single `CrankObservationHint`. The cranker
/// concatenates one of these per hint into the tx tail, in hint order,
/// after the `[payer, market, portfolio, ...]` prefix.
#[derive(Debug, Clone, Default)]
pub struct FetchedObservation {
    pub asset_index: u16,
    /// Read-only oracle accounts the wrapper will authenticate. Empty on
    /// MANUAL / EWMA_MARK / AUTH_MARK modes.
    pub oracle_accounts: Vec<Pubkey>,
}

impl FetchedObservation {
    /// Count that goes into `CrankObservationHint.oracle_accounts` — must
    /// match `oracle_accounts.len()` byte-for-byte or the wrapper will
    /// consume the wrong number of tail accounts for this hint.
    pub fn oracle_account_count(&self) -> u8 {
        u8::try_from(self.oracle_accounts.len()).expect("oracle_accounts.len() ≤ 255")
    }
}

/// Pyth Solana Receiver mainnet program id — hardcoded in the wrapper at
/// the wrapper's `src/v16_program.rs:3523` as a 32-byte literal. This is the
/// program that owns `PriceUpdateV2` accounts; the wrapper checks
/// `price_ai.owner == PYTH_RECEIVER_PROGRAM_ID` at `:3662`.
///
/// Repeating the bytes here (rather than re-exporting from the wrapper)
/// so this file stands alone and the value is auditable inline.
pub const PYTH_RECEIVER_PROGRAM_ID: Pubkey = Pubkey::new_from_array([
    0x0c, 0xb7, 0xfa, 0xbb, 0x52, 0xf7, 0xa6, 0x48, 0xbb, 0x5b, 0x31, 0x7d, 0x9a, 0x01, 0x8b,
    0x90, 0x57, 0xcb, 0x02, 0x47, 0x74, 0xfa, 0xfe, 0x01, 0xe6, 0xc4, 0xdf, 0x98, 0xcc, 0x38,
    0x58, 0x81,
]);

/// Canonical shard id used by Pyth's own price posters on mainnet.
/// Convention (documented in `pyth-solana-receiver-js`'s
/// `getPriceFeedAccountAddress`), NOT protocol-enforced — a market
/// deployer could point at any account whose owner is the receiver
/// program and whose feed_id matches. Left overridable so a real
/// operator can switch to non-canonical shards (2, 3, ...) if they
/// track a different posting stream. Verify against the Pyth version
/// your target market's deployer uses.
pub const PYTH_MAINNET_CANONICAL_SHARD: u16 = 0;

/// Off-chain `feed_id → PriceUpdateV2 account` resolver for Pyth-backed
/// HYBRID assets. Combines two mechanisms:
///
/// * **`overrides`**: an explicit `feed_id → account` map. Populated by
///   the operator from their own tracking (a Pyth-receiver-crawling
///   indexer, a hand-curated table, or a Pyth-hosted API). Takes
///   precedence when set. This is operator-supplied data: market state
///   alone does not name the Pyth account.
///
/// * **`pda_shard`**: fallback derivation using the canonical
///   `PDA(seeds=[shard_le_bytes, feed_id], program_id=RECEIVER)`
///   scheme. When `Some(shard)` and the override map misses, the
///   resolver derives the PDA. When `None`, the resolver returns
///   `None` for misses and the observation walk falls back to
///   treating the 32-byte value as a raw SB/Chainlink account pubkey
///   (which is honest — the wrapper will hard-reject at submit if the
///   guess was wrong).
///
/// A cold-start resolver — no overrides, no shard — is safe: hybrid
/// assets whose legs need a Pyth account will hit a hard-reject at
/// submit, which is the correct signal for a caller running without
/// oracle config.
#[derive(Debug, Clone, Default)]
pub struct PythFeedResolver {
    pub overrides: HashMap<[u8; 32], Pubkey>,
    pub pda_shard: Option<u16>,
}

impl PythFeedResolver {
    /// Empty — no overrides, no PDA fallback. Safe default for markets
    /// that never route through Pyth (MANUAL / EWMA / AUTH_MARK or
    /// HYBRID with SB/Chainlink only).
    pub fn empty() -> Self {
        Self::default()
    }

    /// Fallback-only — no explicit overrides, canonical shard PDA
    /// derivation for every feed_id lookup. Approximates what a
    /// zero-config cranker sees on mainnet against Pyth's own poster
    /// stream (shard 0).
    pub fn canonical_pda_only() -> Self {
        Self {
            overrides: HashMap::new(),
            pda_shard: Some(PYTH_MAINNET_CANONICAL_SHARD),
        }
    }

    /// Explicit overrides + canonical-PDA fallback for anything the
    /// operator hasn't hand-mapped. Recommended production shape.
    #[allow(dead_code)] // exercised by tests; kept as the recommended
                        // production constructor for callers.
    pub fn with_overrides_and_canonical_pda(overrides: HashMap<[u8; 32], Pubkey>) -> Self {
        Self {
            overrides,
            pda_shard: Some(PYTH_MAINNET_CANONICAL_SHARD),
        }
    }

    /// Resolve `feed_id`. Override map wins; falls through to shard PDA
    /// if configured; returns `None` when neither route applies.
    pub fn resolve(&self, feed_id: &[u8; 32]) -> Option<Pubkey> {
        if let Some(pk) = self.overrides.get(feed_id) {
            return Some(*pk);
        }
        if let Some(shard) = self.pda_shard {
            return Some(pyth_price_update_v2_pda(shard, feed_id));
        }
        None
    }
}

/// Canonical `PriceUpdateV2` PDA derivation.
///
/// Seeds: `[shard_id.to_le_bytes(), feed_id]`. Matches Pyth's own
/// `getPriceFeedAccountAddress` (js) / `PythSolanaReceiver`'s Rust
/// helper. The wrapper does NOT check the account's derivation — only
/// that `owner == PYTH_RECEIVER_PROGRAM_ID` and the deserialized
/// `PriceFeedMessage.feed_id` matches. So a non-canonical account with
/// the right owner+feed_id inside also authenticates; the PDA scheme
/// is a discovery convention, not an admission gate.
pub fn pyth_price_update_v2_pda(shard: u16, feed_id: &[u8; 32]) -> Pubkey {
    let shard_bytes = shard.to_le_bytes();
    let seeds: &[&[u8]] = &[&shard_bytes, feed_id];
    Pubkey::find_program_address(seeds, &PYTH_RECEIVER_PROGRAM_ID).0
}

/// Discover the oracle accounts the wrapper will expect for `asset_index`
/// in market `market_data`.
///
/// * On MANUAL / EWMA_MARK / AUTH_MARK: returns empty. The engine reads
///   the mark from committed state (`obs_or_current_asset` closure, 2026-06
///   upstream relaxation) and no tail account is needed.
/// * On HYBRID_AFTER_HOURS: reads `oracle_leg_count` and iterates
///   `oracle_leg_feeds[0..leg_count]`. For each leg, checks the pyth
///   resolver first; if present, uses the resolved pubkey. Otherwise
///   assumes the leg is Switchboard or Chainlink and treats the 32-byte
///   feed value as the account pubkey directly. In production, the
///   cranker should track which legs are which source per-asset in its
///   own configuration; by default we take the byte-embedded interpretation
///   as the default (correct for SB / Chainlink) and let Pyth entries
///   route through the resolver.
pub fn fetch_observation_for_asset(
    market_data: &[u8],
    asset_index: u16,
    pyth_resolver: &PythFeedResolver,
) -> Result<FetchedObservation> {
    let profile = match read_asset_oracle_profile(market_data, asset_index as usize) {
        Some(p) => p,
        None => {
            // Market too short or asset out of range — return an empty tail.
            // The wrapper will produce a clear error at submit time.
            return Ok(FetchedObservation {
                asset_index,
                oracle_accounts: vec![],
            });
        }
    };

    let oracle_accounts = match profile.oracle_mode {
        ORACLE_MODE_MANUAL | ORACLE_MODE_EWMA_MARK | ORACLE_MODE_AUTH_MARK => Vec::new(),
        ORACLE_MODE_HYBRID_AFTER_HOURS => {
            let leg_count = profile.oracle_leg_count as usize;
            let leg_count = leg_count.min(ORACLE_LEG_CAP);
            let mut out = Vec::with_capacity(leg_count);
            for leg in 0..leg_count {
                let feed_bytes = profile.oracle_leg_feeds[leg];
                // Order of preference:
                //   1. Pyth resolver hit (override or PDA fallback) — for
                //      Pyth-backed legs. Only returns Some if the operator
                //      configured either the overrides or the shard PDA.
                //   2. Bytes-as-pubkey — for Switchboard/Chainlink legs,
                //      the account pubkey IS embedded in oracle_leg_feeds.
                // If the operator hasn't declared the source and the
                // resolver returns None, we treat as SB/Chainlink and let
                // the wrapper hard-reject at submit if wrong (IllegalOwner
                // or the discriminator check will fire cleanly).
                let pk = pyth_resolver
                    .resolve(&feed_bytes)
                    .unwrap_or_else(|| Pubkey::new_from_array(feed_bytes));
                out.push(pk);
            }
            out
        }
        _ => Vec::new(), // unknown mode — return empty and let the wrapper error at submit
    };

    Ok(FetchedObservation {
        asset_index,
        oracle_accounts,
    })
}

/// Slice `AssetOracleProfileV16` bytes out of the market slab. Returns
/// `None` when the account is too small or `asset_index` is out of range.
///
/// Layout confirmed against wrapper constants:
///   slot_offset = MARKET_GROUP_OFF + MARKET_GROUP_LEN
///                                  + asset_index * MARKET_ASSET_SLOT_LEN
///   wrapper_blob = [slot_offset .. slot_offset + ASSET_ORACLE_WRAPPER_LEN]
///   profile      = wrapper_blob[0 .. ASSET_ORACLE_PROFILE_LEN]
///
/// The engine's `MarketGroupV16HeaderAccount::dynamic_asset_slot_offset::<[u8;
/// ASSET_ORACLE_WRAPPER_LEN]>(asset_index)` helper computes the same
/// `MARKET_GROUP_LEN + i * MARKET_ASSET_SLOT_LEN` term; we cross-check
/// against it in debug builds so a layout change surfaces at test time
/// rather than silently mis-slicing.
pub fn read_asset_oracle_profile(
    market_data: &[u8],
    asset_index: usize,
) -> Option<AssetOracleProfileV16> {
    let base = MARKET_GROUP_OFF + MARKET_GROUP_LEN;
    let slot_offset = base.checked_add(asset_index.checked_mul(MARKET_ASSET_SLOT_LEN)?)?;
    debug_assert_eq!(
        MARKET_GROUP_OFF
            + MarketGroupV16HeaderAccount::dynamic_asset_slot_offset::<
                [u8; ASSET_ORACLE_WRAPPER_LEN],
            >(asset_index)
            .ok()?,
        slot_offset,
        "engine helper and wrapper constants disagree on slot offset — layout drift"
    );
    let profile_end = slot_offset.checked_add(ASSET_ORACLE_PROFILE_LEN)?;
    if market_data.len() < profile_end {
        return None;
    }
    let bytes = &market_data[slot_offset..profile_end];
    // `AssetOracleProfileV16` is `bytemuck::Pod`. Use `pod_read_unaligned`
    // to match the wrapper's own decode (`read_oracle_profile_from_view`).
    Some(bytemuck::pod_read_unaligned::<AssetOracleProfileV16>(bytes))
}

/// Convenience helper for callers that don't have a resolver handy.
pub fn empty_pyth_resolver() -> PythFeedResolver {
    PythFeedResolver::empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_resolver_never_hits() {
        let r = PythFeedResolver::empty();
        assert!(r.resolve(&[0x11; 32]).is_none());
        assert!(r.resolve(&[0xff; 32]).is_none());
    }

    #[test]
    fn override_wins_over_pda() {
        let mut overrides = HashMap::new();
        let feed = [0xab; 32];
        let sentinel = Pubkey::new_from_array([0xcc; 32]);
        overrides.insert(feed, sentinel);
        let r = PythFeedResolver::with_overrides_and_canonical_pda(overrides);
        assert_eq!(r.resolve(&feed), Some(sentinel));
    }

    #[test]
    fn canonical_pda_fallback_is_deterministic() {
        let r = PythFeedResolver::canonical_pda_only();
        let feed = [0x01; 32];
        let a = r.resolve(&feed).expect("canonical PDA available");
        let b = r.resolve(&feed).expect("canonical PDA available");
        assert_eq!(a, b, "PDA derivation must be deterministic");
    }

    #[test]
    fn canonical_pda_matches_helper() {
        let feed = [0x33; 32];
        let r = PythFeedResolver::canonical_pda_only();
        let via_resolver = r.resolve(&feed).unwrap();
        let via_helper = pyth_price_update_v2_pda(PYTH_MAINNET_CANONICAL_SHARD, &feed);
        assert_eq!(via_resolver, via_helper);
    }

    #[test]
    fn different_shards_derive_different_pdas() {
        let feed = [0x44; 32];
        let s0 = pyth_price_update_v2_pda(0, &feed);
        let s3 = pyth_price_update_v2_pda(3, &feed);
        assert_ne!(s0, s3, "shard is part of the seed");
    }

    #[test]
    fn different_feeds_derive_different_pdas() {
        let a = pyth_price_update_v2_pda(0, &[0x55; 32]);
        let b = pyth_price_update_v2_pda(0, &[0x66; 32]);
        assert_ne!(a, b, "feed_id is part of the seed");
    }
}
