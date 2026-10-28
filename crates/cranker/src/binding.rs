//! Wrapper incarnation bindings used on the decode/submit path.
//!
//! After wrapper `#135`, several ixs bind `(portfolio_id, expected_sequence,
//! position_epoch)` and market-authority ixs bind `(policy_sequence,
//! authority_epoch)`. Portfolio accounts also carry engine layout
//! discriminator 18 in the provenance header. Helpers here read those
//! fields from the wrapper's own offsets / decoders — no hand-rolled
//! engine view.

use anyhow::{anyhow, Result};
use percolator_prog::constants::{HEADER_LEN, PORTFOLIO_ID_OFF};
use percolator_prog::state::{
    read_asset_control_sequences, read_market, read_portfolio_id,
    read_portfolio_matcher_sequence, read_portfolio_position_epoch,
};

/// Engine `V16_LAYOUT_DISCRIMINATOR` at pins `495a5590`, `4db11a8c` and
/// `68859b94` (unchanged across the launch-fix batch). Not re-exported
/// from `percolator`'s default `lib.rs` surface; keep in lockstep with
/// the engine's `src/v16.rs`.
pub const V16_LAYOUT_DISCRIMINATOR: u16 = 18;

/// ProvenanceHeaderV16Account: 32 + 32 + 32 + version(2) + layout(2).
const LAYOUT_DISCRIMINATOR_OFF: usize = HEADER_LEN + 96 + 2;

/// A6 drain: engine `V16_SOURCE_LIEN_RELEASE_CHUNK_DOMAINS = 1`.
/// Loop this many domains per call until the account stops mutating.
pub const SOURCE_LIEN_RELEASE_CHUNK_DOMAINS: usize = 1;

/// Cap on inner drain steps for one candidate in one cycle. Bounded by
/// the wrapper's max source-domain count so a stuck liened account can
/// actually clear without an unbounded retry.
pub const SOURCE_LIEN_DRAIN_MAX_STEPS: usize =
    percolator_prog::constants::WRAPPER_MAX_BOUNDED_SOURCE_DOMAINS;

#[derive(Debug, Clone, Copy)]
pub struct PortfolioBinding {
    pub portfolio_id: u64,
    pub sequence: u64,
    #[allow(dead_code)] // used by ClosePortfolio in tests via the lib target
    pub position_epoch: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct MarketAuthorityBinding {
    pub authority_epoch: u64,
    pub liquidation_fee_seq: u64,
    pub maintenance_fee_seq: u64,
    #[allow(dead_code)] // used by ResolveMarket in tests via the lib target
    pub next_market_id: u64,
}

pub fn layout_discriminator(data: &[u8]) -> Option<u16> {
    let end = LAYOUT_DISCRIMINATOR_OFF + 2;
    if data.len() < end {
        return None;
    }
    Some(u16::from_le_bytes(
        data[LAYOUT_DISCRIMINATOR_OFF..end].try_into().ok()?,
    ))
}

pub fn layout_is_current(data: &[u8]) -> bool {
    layout_discriminator(data) == Some(V16_LAYOUT_DISCRIMINATOR)
}

pub fn read_portfolio_id_bytes(data: &[u8]) -> Option<u64> {
    let end = PORTFOLIO_ID_OFF + 8;
    if data.len() < end {
        return None;
    }
    let id = u64::from_le_bytes(data[PORTFOLIO_ID_OFF..end].try_into().ok()?);
    if id == 0 {
        None
    } else {
        Some(id)
    }
}

pub fn portfolio_binding(data: &[u8]) -> Result<PortfolioBinding> {
    let portfolio_id =
        read_portfolio_id(data).map_err(|e| anyhow!("read_portfolio_id: {e}"))?;
    let sequence = read_portfolio_matcher_sequence(data)
        .map_err(|e| anyhow!("read_portfolio_matcher_sequence: {e}"))?;
    let position_epoch = read_portfolio_position_epoch(data)
        .map_err(|e| anyhow!("read_portfolio_position_epoch: {e}"))?;
    Ok(PortfolioBinding {
        portfolio_id,
        sequence,
        position_epoch,
    })
}

pub fn market_authority_binding(data: &[u8]) -> Result<MarketAuthorityBinding> {
    let seqs = read_asset_control_sequences(data, 0)
        .map_err(|e| anyhow!("read_asset_control_sequences: {e}"))?;
    let (_cfg, mg) = read_market(data).map_err(|e| anyhow!("read_market: {e:?}"))?;
    Ok(MarketAuthorityBinding {
        authority_epoch: seqs.authority_epoch,
        liquidation_fee_seq: seqs.liquidation_fee,
        maintenance_fee_seq: seqs.maintenance_fee,
        next_market_id: mg.next_market_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_const_is_eighteen() {
        assert_eq!(V16_LAYOUT_DISCRIMINATOR, 18);
        assert_eq!(SOURCE_LIEN_RELEASE_CHUNK_DOMAINS, 1);
    }
}
