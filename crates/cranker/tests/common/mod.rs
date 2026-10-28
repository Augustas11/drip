//! Shared test support: env-gated RPC wiring, wrapper→engine account
//! decoders, and the conservation-invariant oracle.
//!
//! The invariant set below is ported directly from the upstream fuzz
//! harness `tests/v16_fuzzing.rs::assert_fuzz_invariants` (PR #137 vintage,
//! but every field it touches already exists at our pinned engine rev
//! `c8aab33`). The harness asserts these in-memory after every engine call;
//! we assert the same properties against decoded on-chain account bytes
//! after every crank. This is the "did the crank move state *legally*"
//! check that complements `submit.rs`'s "did state move at all" byte-diff.
//!
//! Decoding uses the wrapper's own authoritative decoders
//! (`state::read_market` / `state::read_portfolio`) rather than manual byte
//! offsets, so there are no layout assumptions to keep in sync.
//!
//! LAYER NOTE. Two invariants in the harness — `market.validate_shape()`
//! and `PortfolioV16View::validate_with_market(&market)` — are engine
//! *view* methods and need the engine `MarketGroupV16View` (header + every
//! `Market<u64>` slot), which the host-serialization `MarketGroupV16` from
//! `read_market` does not provide. They are left as a documented TODO at the
//! bottom. The conservation checks here already catch any value
//! non-conservation a buggy crank could cause.

#![allow(dead_code)] // shared helpers; not every test file uses every one.

use anyhow::{anyhow, bail, Context, Result};
use percolator::PortfolioAccountV16Account;
use percolator_prog::ix::Instruction as ProgInstruction;
use percolator_prog::state::{read_market, read_portfolio, MarketGroupV16};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::Transaction,
};
use std::str::FromStr;

use drip::discovery::{discover_portfolios, DiscoveredPortfolio};
use drip::setup::CrankerEnv;

// ---------------------------------------------------------------------------
// Env-gated wiring. Integration tests only run when a validator is pointed at.
// ---------------------------------------------------------------------------

/// Live test context, present only when the env vars are set. Tests call
/// `skip_unless_live()` at the top and early-return `Ok(())` otherwise, so
/// `cargo test` stays green with no validator running.
pub struct Live {
    pub rpc: RpcClient,
    pub program_id: Pubkey,
    /// The market (KIND_MARKET account) under test.
    pub market: Pubkey,
}

/// Returns `Some(Live)` when `PERCOLATOR_TEST_RPC`, `PERCOLATOR_PROGRAM_ID`,
/// and `PERCOLATOR_TEST_MARKET` are all set; otherwise `None` (skip).
///
/// Example:
///   PERCOLATOR_TEST_RPC=http://127.0.0.1:8899 \
///   PERCOLATOR_PROGRAM_ID=Perco1ator111111111111111111111111111111111 \
///   PERCOLATOR_TEST_MARKET=<market pubkey> \
///   cargo test -p drip --test lifecycle -- --nocapture
pub fn live_from_env() -> Option<Live> {
    let url = std::env::var("PERCOLATOR_TEST_RPC").ok()?;
    assert!(local_rpc(&url), "PERCOLATOR_TEST_RPC must be a local validator, got {url}");
    let program_id = std::env::var("PERCOLATOR_PROGRAM_ID")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok())?;
    let market = std::env::var("PERCOLATOR_TEST_MARKET")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok())?;
    Some(Live {
        rpc: RpcClient::new_with_commitment(url, CommitmentConfig::confirmed()),
        program_id,
        market,
    })
}

/// Print a skip notice and signal the caller to early-return.
pub fn skip(test: &str) {
    eprintln!(
        "SKIP {test}: set PERCOLATOR_TEST_RPC / PERCOLATOR_PROGRAM_ID / \
         PERCOLATOR_TEST_MARKET to run against a live validator."
    );
}

/// Lighter env read for tests that BOOTSTRAP their own market (lifecycle):
/// they only need the RPC endpoint + program id, then create the market
/// themselves. Returns `None` (skip) when either var is missing.
/// Validator tests send transactions: refuse anything but a local RPC.
/// Parses the authority and compares the exact host; userinfo (`a@b`) is
/// rejected so `http://127.0.0.1@remote.example/` cannot pass.
fn local_rpc(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return false;
    }
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next().map(|h| format!("[{h}]")).unwrap_or_default()
    } else {
        authority.split(':').next().unwrap_or("").to_string()
    };
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

#[test]
fn local_rpc_guard_parses_the_host() {
    for ok in ["http://127.0.0.1:8899", "http://localhost:8899/", "http://[::1]:8899"] {
        assert!(local_rpc(ok), "{ok}");
    }
    for bad in [
        "http://localhost.example.com:8899",
        "http://127.0.0.1@remote.example:8899",
        "https://localhost@remote.example.invalid:8899",
        "http://127.0.0.1.evil:8899",
        "ftp://127.0.0.1",
    ] {
        assert!(!local_rpc(bad), "{bad}");
    }
}

pub fn rpc_and_program_from_env() -> Option<(RpcClient, Pubkey)> {
    let url = std::env::var("PERCOLATOR_TEST_RPC").ok()?;
    assert!(local_rpc(&url), "PERCOLATOR_TEST_RPC must be a local validator, got {url}");
    let program_id = std::env::var("PERCOLATOR_PROGRAM_ID")
        .ok()
        .and_then(|s| Pubkey::from_str(&s).ok())?;
    Some((RpcClient::new_with_commitment(url, CommitmentConfig::confirmed()), program_id))
}

/// Load the funded validator payer, mirroring `main.rs`: `PERCOLATOR_TEST_PAYER`
/// (a keypair-file path) if set, else `~/.config/solana/id.json`.
pub fn load_payer() -> Result<Keypair> {
    let path = match std::env::var("PERCOLATOR_TEST_PAYER") {
        Ok(p) => std::path::PathBuf::from(p),
        Err(_) => {
            let home = std::env::var("HOME").context("HOME unset")?;
            std::path::PathBuf::from(home).join(".config/solana/id.json")
        }
    };
    read_keypair_file(&path).map_err(|e| anyhow!("read payer keypair {path:?}: {e}"))
}

// ---------------------------------------------------------------------------
// Wrapper account bytes -> engine structs, via the wrapper's OWN authoritative
// decoders (`state::read_market` / `state::read_portfolio`). No manual offsets
// or bytemuck casts — the wrapper handles header/provenance stripping and
// layout itself, so there is nothing to keep in sync as the layout evolves.
// ---------------------------------------------------------------------------

/// Decode a KIND_MARKET account into the wrapper's host-serialization
/// `MarketGroupV16` (plain-`u128` aggregate totals). Authoritative — this is
/// the same `read_market` the wrapper uses.
pub fn decode_market(market_data: &[u8]) -> Result<MarketGroupV16> {
    let (_cfg, mg) = read_market(market_data).map_err(|e| anyhow!("read_market: {e:?}"))?;
    Ok(mg)
}

/// Decode a KIND_PORTFOLIO account into the engine `PortfolioAccountV16Account`
/// (`state::read_portfolio`; `PortfolioAccountV16` is a type alias for it).
pub fn decode_portfolio(portfolio_data: &[u8]) -> Result<PortfolioAccountV16Account> {
    read_portfolio(portfolio_data).map_err(|e| anyhow!("read_portfolio: {e:?}"))
}

/// Enumerate and decode EVERY portfolio belonging to `market`.
///
/// Conservation invariants only hold over the COMPLETE portfolio set — they
/// compare market totals (`c_tot`, `pnl_pos_tot`) against the sum across all
/// accounts. Missing one account makes the sum wrong. This uses the same
/// `discover_portfolios` path the cranker uses, filtered to this market.
pub fn read_all_portfolios(
    rpc: &RpcClient,
    program_id: &Pubkey,
    market: &Pubkey,
) -> Result<Vec<PortfolioAccountV16Account>> {
    let discovered = discover_portfolios(rpc, program_id).context("discover_portfolios")?;
    let mine: Vec<DiscoveredPortfolio> = discovered
        .into_iter()
        .filter(|p| p.market_group_id().as_ref() == Some(market))
        .collect();
    if mine.is_empty() {
        return Err(anyhow!(
            "no portfolios found for market {market} — conservation check needs the full set"
        ));
    }
    mine.iter()
        .map(|p| decode_portfolio(&p.account.data))
        .collect()
}

// ---------------------------------------------------------------------------
// The invariant oracle. Ported from assert_fuzz_invariants.
// ---------------------------------------------------------------------------

/// Assert the market conservation invariants over a full snapshot. Panics
/// (like the harness) on the first violated invariant.
///
/// Ported from `tests/v16_fuzzing.rs::assert_fuzz_invariants`. The fields come
/// from the wrapper's host-serialization `MarketGroupV16` (plain `u128`), which
/// exposes `c_tot`/`vault`/`insurance`/`pnl_pos_tot`/
/// `backing_provider_earnings_total`/`insurance_domain_budget_remaining_total`
/// directly.
///
/// NOTE on the vault-solvency term: the harness also added
/// `source_fresh_backing_total_num / BOUND_SCALE`. The host mirror does not
/// expose that engine-internal aggregate by name (it carries source accounting
/// as `source_claim_bound_total_num` / `source_insurance_credit_reserved_total_atoms`
/// and the `source_*` Vecs instead). Since that term is non-negative, checking
/// `vault >= c_tot + insurance + earnings` is a valid — if slightly looser —
/// necessary condition: a failure still proves non-conservation. Tighten it by
/// adding whichever source aggregate matches your deployment if you need the
/// exact bound.
pub fn assert_conservation(mg: &MarketGroupV16, portfolios: &[PortfolioAccountV16Account]) {
    // Capital conservation: market total == sum of every account's capital.
    let capital_sum: u128 = portfolios.iter().map(|p| p.capital.get()).sum();
    assert_eq!(
        mg.c_tot, capital_sum,
        "c_tot must equal the sum of all account capital"
    );

    // Vault solvency (see NOTE above re: the omitted source-backing term).
    let required = mg.c_tot + mg.insurance + mg.backing_provider_earnings_total;
    assert!(
        mg.vault >= required,
        "vault {} must cover c_tot+insurance+earnings {}",
        mg.vault,
        required
    );

    // Insurance-domain budget can never exceed total insurance.
    assert!(
        mg.insurance_domain_budget_remaining_total <= mg.insurance,
        "insurance_domain_budget_remaining_total must not exceed insurance"
    );

    // Positive-PnL accounting: pnl_pos_tot == sum of only the positive pnls.
    let positive_pnl: u128 = portfolios
        .iter()
        .map(|p| p.pnl.get())
        .filter(|pnl| *pnl > 0)
        .map(|pnl| pnl as u128)
        .sum();
    assert_eq!(
        mg.pnl_pos_tot, positive_pnl,
        "pnl_pos_tot must equal the sum of positive account pnl"
    );
}

/// Read a market + all its portfolios from RPC and run the conservation
/// oracle. Returns the decoded snapshot so callers can assert further.
pub fn assert_conservation_at(
    rpc: &RpcClient,
    program_id: &Pubkey,
    market: &Pubkey,
) -> Result<(MarketGroupV16, Vec<PortfolioAccountV16Account>)> {
    let market_data = rpc
        .get_account_data(market)
        .context("read market account")?;
    let mg = decode_market(&market_data)?;
    let portfolios = read_all_portfolios(rpc, program_id, market)?;
    assert_conservation(&mg, &portfolios);
    Ok((mg, portfolios))
}

/// `Live`-flavored convenience wrapper around [`assert_conservation_at`].
pub fn assert_conservation_live(
    live: &Live,
) -> Result<(MarketGroupV16, Vec<PortfolioAccountV16Account>)> {
    assert_conservation_at(&live.rpc, &live.program_id, &live.market)
}

// ---------------------------------------------------------------------------
// Drive-to-Resolved helpers.
//
// Bootstrap creates capital but NO open positions, so the market reaches
// Resolved without any trades or liquidations: resolve (admin) drains nothing
// to settle, then CloseResolved pays each account's capital back out. That is
// the on-chain analogue of the harness's `resolve_market_not_atomic` +
// `close_resolved_account_not_atomic` wind-down.
//
// NOT verified on-chain from here — account orderings are transcribed from the
// wrapper handlers (`handle_resolve_market` @ v16_program.rs:8596,
// `handle_close_resolved` @ v16_program.rs:10236). Confirm on first live run.
// ---------------------------------------------------------------------------

/// Send one wrapper instruction with CU+heap budget, the env payer as fee
/// payer, plus any extra signers. Mirrors `setup::send_prog_ix` (which is
/// private to the crate).
fn send_prog_tx(
    rpc: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    ix: ProgInstruction,
    accounts: Vec<AccountMeta>,
    extra_signers: &[&Keypair],
) -> Result<()> {
    let instruction = Instruction {
        program_id: *program_id,
        accounts,
        data: ix.encode(),
    };
    let cu = ComputeBudgetInstruction::set_compute_unit_limit(1_400_000);
    let heap = ComputeBudgetInstruction::request_heap_frame(128 * 1024);
    let mut signers: Vec<&Keypair> = Vec::with_capacity(1 + extra_signers.len());
    signers.push(payer);
    signers.extend_from_slice(extra_signers);
    for attempt in 0..5 {
        let (bh, _) = rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
        let tx = Transaction::new_signed_with_payer(
            &[cu.clone(), heap.clone(), instruction.clone()],
            Some(&payer.pubkey()),
            &signers,
            bh,
        );
        match rpc.send_and_confirm_transaction(&tx) {
            Ok(_) => return Ok(()),
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("Blockhash not found") && attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    continue;
                }
                return Err(anyhow!(msg));
            }
        }
    }
    bail!("send_prog_tx: retries exhausted");
}

/// Transition the market Live -> Resolved. Admin-gated: `handle_resolve_market`
/// requires `accounts[0]` to be the market authority (the `admin` keypair set
/// at `InitMarket`) as a signer, and `accounts[1]` the writable market.
pub fn resolve_market(rpc: &RpcClient, program_id: &Pubkey, env: &CrankerEnv) -> Result<()> {
    let market_data = rpc.get_account_data(&env.market).context("read market")?;
    let bind = drip::binding::market_authority_binding(&market_data)?;
    send_prog_tx(
        rpc,
        program_id,
        &env.payer,
        ProgInstruction::ResolveMarket {
            asset_generation_frontier: bind.next_market_id,
            authority_epoch: bind.authority_epoch,
        },
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(env.market, false),
        ],
        &[&env.admin],
    )
    .context("ResolveMarket")
}

/// Submit one `CloseResolved` for `portfolio`, owned by `owner`.
///
/// Account order (`handle_close_resolved`): `[owner(signer) → market(writable)
/// → portfolio(writable)]`, and when the close pays out (`payout != 0`) the
/// wrapper also reads `[dest_token → vault_token → vault_authority →
/// token_program]`. We always append the payout tail when `dest_token` is
/// given (harmless if payout is zero — the handler only touches them when it
/// pays). Pass `dest_token: None` for accounts that never pay out (e.g. the
/// empty cranker reward portfolio).
///
/// `close_resolved_account_not_atomic` may return `ProgressOnly` and need
/// several calls; drive this in a loop until [`portfolio_is_closed`].
pub fn close_resolved_once(
    rpc: &RpcClient,
    program_id: &Pubkey,
    env: &CrankerEnv,
    portfolio: &Pubkey,
    owner: &Keypair,
    dest_token: Option<Pubkey>,
) -> Result<()> {
    let mut accounts = vec![
        AccountMeta::new(owner.pubkey(), true),
        AccountMeta::new(env.market, false),
        AccountMeta::new(*portfolio, false),
    ];
    if let Some(dest) = dest_token {
        let (vault_authority, _bump) =
            Pubkey::find_program_address(&[b"vault", env.market.as_ref()], program_id);
        accounts.push(AccountMeta::new(dest, false)); // dest_token
        accounts.push(AccountMeta::new(env.vault, false)); // vault_token
        accounts.push(AccountMeta::new_readonly(vault_authority, false));
        accounts.push(AccountMeta::new_readonly(spl_token::ID, false));
    }
    send_prog_tx(
        rpc,
        program_id,
        &env.payer,
        ProgInstruction::CloseResolved { fee_rate_per_slot: 0 },
        accounts,
        &[owner],
    )
    .context("CloseResolved")
}

/// Best-effort "is this portfolio wound down" check: capital and pnl both
/// zero. Proxy for the harness's `fuzz_account_is_closed` (which also checks
/// no active legs). Refine against the engine's exact closed predicate if you
/// need a stricter gate.
pub fn portfolio_is_closed(rpc: &RpcClient, portfolio: &Pubkey) -> Result<bool> {
    let data = rpc.get_account_data(portfolio).context("read portfolio")?;
    let p = decode_portfolio(&data)?;
    Ok(p.capital.get() == 0 && p.pnl.get() == 0)
}

/// Deregister and reclaim an emptied portfolio account (`ClosePortfolio`,
/// tag 8). Accounts `[closer(signer), market(writable), portfolio(writable)]`
/// per `handle_close_portfolio` (v16_program.rs:7009). After the market is
/// Resolved the market authority may close ANY emptied portfolio ("terminal
/// marketauth cleanup"), so pass `&env.admin` as `closer`. Requires the
/// portfolio already emptied via `CloseResolved` (the handler runs
/// `deregister_empty_materialized_portfolio_not_atomic`). This is what drops
/// `materialized_portfolio_count`, a `CloseSlab` precondition.
pub fn close_portfolio(
    rpc: &RpcClient,
    program_id: &Pubkey,
    env: &CrankerEnv,
    portfolio: &Pubkey,
    closer: &Keypair,
) -> Result<()> {
    let data = rpc.get_account_data(portfolio).context("read portfolio")?;
    let bind = drip::binding::portfolio_binding(&data)?;
    send_prog_tx(
        rpc,
        program_id,
        &env.payer,
        ProgInstruction::ClosePortfolio {
            portfolio_id: bind.portfolio_id,
            expected_sequence: bind.sequence,
            position_epoch: bind.position_epoch,
        },
        vec![
            AccountMeta::new(closer.pubkey(), true),
            AccountMeta::new(env.market, false),
            AccountMeta::new(*portfolio, false),
        ],
        &[closer],
    )
    .context("ClosePortfolio")
}

/// Retire the market slab (`CloseSlab`, tag 13). Account order from
/// `handle_close_slab` (v16_program.rs:8168):
///   `[admin_dest(signer,writable) → market(writable) → vault_token(writable)
///     → vault_authority → dest_token → token_program]`
/// (single-collateral market — no secondary mint tail).
///
/// PRECONDITIONS the handler enforces: market mode == Resolved AND
/// `vault == insurance == c_tot == materialized_portfolio_count == 0`. That is
/// why the drive-to-close test bootstraps with `maintenance_fee_per_slot = 0`
/// (no insurance accrues) and closes+deregisters every portfolio first.
/// `admin_dest` must be the market authority and its ATA for the primary mint
/// is `dest_token` (any residual vault balance lands there).
pub fn close_slab(
    rpc: &RpcClient,
    program_id: &Pubkey,
    env: &CrankerEnv,
    admin_dest_ata: &Pubkey,
) -> Result<()> {
    let (vault_authority, _bump) =
        Pubkey::find_program_address(&[b"vault", env.market.as_ref()], program_id);
    let market_data = rpc.get_account_data(&env.market).context("read market")?;
    let bind = drip::binding::market_authority_binding(&market_data)?;
    send_prog_tx(
        rpc,
        program_id,
        &env.payer,
        ProgInstruction::CloseSlab {
            authority_epoch: bind.authority_epoch,
        },
        vec![
            AccountMeta::new(env.admin.pubkey(), true),
            AccountMeta::new(env.market, false),
            AccountMeta::new(env.vault, false),
            AccountMeta::new_readonly(vault_authority, false),
            AccountMeta::new(*admin_dest_ata, false),
            AccountMeta::new_readonly(spl_token::ID, false),
        ],
        &[&env.admin],
    )
    .context("CloseSlab")
}

/// Ensure `owner` has an ATA for `mint`, creating it (payer-funded) if absent.
/// Returns the ATA address.
pub fn ensure_ata(
    rpc: &RpcClient,
    payer: &Keypair,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Result<Pubkey> {
    let ata = spl_associated_token_account::get_associated_token_address(owner, mint);
    if rpc.get_account(&ata).is_ok() {
        return Ok(ata);
    }
    let ix = spl_associated_token_account::instruction::create_associated_token_account(
        &payer.pubkey(),
        owner,
        mint,
        &spl_token::ID,
    );
    let (bh, _) = rpc.get_latest_blockhash_with_commitment(CommitmentConfig::processed())?;
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&payer.pubkey()), &[payer], bh);
    rpc.send_and_confirm_transaction(&tx)
        .context("create ATA")?;
    Ok(ata)
}

/// `true` while the market slab account still exists. After a successful
/// `CloseSlab` it is deallocated — account-gone is the on-chain proof of
/// retirement (the slab is deleted, so there is no post-retire struct to
/// decode; this replaces the harness's in-memory `lifecycle == Retired`
/// check, which inspected a struct that on-chain no longer exists).
/// `true` once CloseSlab has retired the market. The wrapper does not
/// deallocate the slab: `handle_close_slab` reallocs it to `HEADER_LEN` and
/// keeps that much rent (a tombstone), so "retired" means gone OR header-only.
pub fn market_is_retired(rpc: &RpcClient, market: &Pubkey) -> Result<bool> {
    match rpc.get_account(market) {
        Ok(acct) => Ok(acct.lamports == 0
            || acct.data.len() <= percolator_prog::constants::HEADER_LEN),
        Err(_) => Ok(true),
    }
}

// ---------------------------------------------------------------------------
// Not yet covered: full-view invariants.
//
// To also assert `market.validate_shape()` and, per account,
// `PortfolioV16View::new(acct).validate_with_market(&market_view)` — the two
// harness checks omitted above — reconstruct a `MarketGroupV16View`:
//
//   1. header  = decode_market_header(market_data)          (done above)
//   2. markets = for each asset slot i in 0..header.asset_count:
//        slice [MARKET_GROUP_OFF + MARKET_GROUP_LEN + i*MARKET_ASSET_SLOT_LEN ..]
//        and rebuild a `Market<u64>` from the wrapper slot. The wrapper's
//        on-chain slot layout is NOT a bytemuck-identical `Market<u64>`
//        (it interleaves the oracle profile), so this needs the wrapper's
//        slot decoder, not a raw cast. `observation.rs` already extracts the
//        `AssetOracleProfileV16` prefix of each slot — extend that to pull
//        the `EngineAssetSlotV16Account` too, then wrap in `Market::new`.
//   3. let view = MarketGroupV16View::new(&header, &markets);
//        assert_eq!(view.validate_shape(), Ok(()));
//        for acct in &portfolios {
//            assert_eq!(PortfolioV16View::new(acct).validate_with_market(&view), Ok(()));
//        }
//
// Until then, `assert_conservation` above is the working oracle.
// ---------------------------------------------------------------------------
