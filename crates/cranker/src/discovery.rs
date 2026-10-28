//! Portfolio discovery via `getProgramAccounts`.
//!
//! Filters on the wrapper's 11-byte header prefix
//! `[MAGIC(8) | VERSION(2) | KIND_PORTFOLIO(1)]` — see
//! `write_header` / `check_header` at the wrapper's `src/v16_program.rs:684`.
//! This isolates portfolio-kind accounts owned by the program, without
//! having to enumerate everything and filter client-side.

use anyhow::{Context, Result};
use percolator_prog::constants::{HEADER_LEN, KIND_PORTFOLIO};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::{RpcAccountInfoConfig, RpcProgramAccountsConfig},
    rpc_filter::{Memcmp, RpcFilterType},
};
use solana_sdk::{account::Account, commitment_config::CommitmentConfig, pubkey::Pubkey};

use crate::binding::{self, V16_LAYOUT_DISCRIMINATOR};

/// One portfolio row surfaced by discovery. The cranker ranks these before
/// deciding whom to crank.
#[derive(Debug, Clone)]
pub struct DiscoveredPortfolio {
    pub pubkey: Pubkey,
    pub account: Account,
}

impl DiscoveredPortfolio {
    /// The market this portfolio belongs to. Read from
    /// `ProvenanceHeaderV16Account.market_group_id` at bytes
    /// `HEADER_LEN..HEADER_LEN+32` of the account data.
    pub fn market_group_id(&self) -> Option<Pubkey> {
        read_pubkey_at(&self.account.data, HEADER_LEN)
    }

    /// Portfolio owner. Read from `ProvenanceHeaderV16Account.owner` at
    /// bytes `HEADER_LEN + 64..HEADER_LEN + 96` — after market_group_id
    /// (32 bytes) and portfolio_account_id (32 bytes). The engine
    /// duplicates this at a later offset for hot-path checks; we use the
    /// provenance copy since it's stable at a fixed offset regardless
    /// of `V16_MAX_PORTFOLIO_ASSETS_N`.
    pub fn owner(&self) -> Option<Pubkey> {
        read_pubkey_at(&self.account.data, HEADER_LEN + 64)
    }

    /// Engine layout discriminator from `ProvenanceHeaderV16Account`.
    /// Current pin requires `V16_LAYOUT_DISCRIMINATOR = 18`.
    pub fn layout_discriminator(&self) -> Option<u16> {
        binding::layout_discriminator(&self.account.data)
    }

    /// Wrapper-assigned portfolio incarnation. Bind this (with market +
    /// pubkey) on any ix that now takes `portfolio_id`.
    pub fn portfolio_id(&self) -> Option<u64> {
        binding::read_portfolio_id_bytes(&self.account.data)
    }
}

fn read_pubkey_at(data: &[u8], offset: usize) -> Option<Pubkey> {
    let end = offset + 32;
    if data.len() < end {
        return None;
    }
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&data[offset..end]);
    Some(Pubkey::new_from_array(buf))
}

/// Enumerate all program-owned KIND_PORTFOLIO accounts.
///
/// Memcmp filter is 11 bytes: the wrapper header magic + version + kind.
/// This is cheap and matches every initialized portfolio regardless of
/// which market it belongs to. We do not filter by market at RPC level —
/// the caller enumerates all portfolios and prunes locally, which keeps
/// this discovery function reusable across markets.
pub fn discover_portfolios(
    rpc: &RpcClient,
    program_id: &Pubkey,
) -> Result<Vec<DiscoveredPortfolio>> {
    let magic_bytes = percolator_prog::constants::MAGIC.to_le_bytes();
    let version_bytes = percolator_prog::constants::VERSION.to_le_bytes();
    let mut prefix = Vec::with_capacity(11);
    prefix.extend_from_slice(&magic_bytes);
    prefix.extend_from_slice(&version_bytes);
    prefix.push(KIND_PORTFOLIO);
    debug_assert_eq!(prefix.len(), 11);

    let filters = vec![RpcFilterType::Memcmp(Memcmp::new_raw_bytes(0, prefix))];
    let config = RpcProgramAccountsConfig {
        filters: Some(filters),
        account_config: RpcAccountInfoConfig {
            commitment: Some(CommitmentConfig::confirmed()),
            encoding: Some(solana_account_decoder::UiAccountEncoding::Base64),
            ..RpcAccountInfoConfig::default()
        },
        ..RpcProgramAccountsConfig::default()
    };
    let raw = rpc
        .get_program_accounts_with_config(program_id, config)
        .context("getProgramAccounts KIND_PORTFOLIO")?;
    Ok(raw
        .into_iter()
        .map(|(pubkey, account)| DiscoveredPortfolio { pubkey, account })
        .collect())
}

/// Rank candidates for cranking.
///
/// Heuristic (deliberately simple):
///   1. Restrict to portfolios belonging to `market` — otherwise the
///      wrapper rejects with `EngineProvenanceMismatch` (0x10) and we
///      waste tx fees. This is the crucial filter because the RPC
///      returns ALL portfolios owned by the program, across ALL markets
///      the same program has ever seeded — including leftovers from
///      previous bringups on the same ledger.
///   2. Never crank our own reward portfolio (would be self-dealing and
///      the wrapper hard-rejects with `InvalidInstruction` on the
///      `PermissionlessCrank` reward path when `last.key == portfolio_ai.key`,
///      `src/v16_program.rs:10475`).
///   3. Require an initialized owner (non-zero).
///   4. Preserve discovery order deterministically otherwise. In a
///      production cranker this is where we'd score by (last_accrual_slot
///      age × capital) or by distance-from-liquidation-threshold. The
///      plan-classification gate + retry safety in `submit.rs` covers
///      us against bad ranking.
pub fn rank_by_staleness(
    portfolios: Vec<DiscoveredPortfolio>,
    market: &Pubkey,
    self_reward_portfolio: &Pubkey,
) -> Vec<DiscoveredPortfolio> {
    let mut kept: Vec<DiscoveredPortfolio> = portfolios
        .into_iter()
        .filter(|p| p.market_group_id().as_ref() == Some(market))
        .filter(|p| p.pubkey != *self_reward_portfolio)
        .filter(|p| p.owner().map(|o| o != Pubkey::default()).unwrap_or(false))
        .filter(|p| p.layout_discriminator() == Some(V16_LAYOUT_DISCRIMINATOR))
        .filter(|p| p.portfolio_id().is_some())
        .collect();
    kept.sort_by_key(|p| p.pubkey.to_bytes());
    kept
}
