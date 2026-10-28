//! Submission + three-way response classification.
//!
//! After the 2026-06-26 no-observation-progress relaxation + the two
//! 2026-07-01 wrapper hard-reject guards (`6869e58`, `8067a13`), the
//! response shape for `PermissionlessCrank` is three-way:
//!
//! * `Err(hard reject)` — wrapper aborted (stale-selected-mark, stale
//!   window, missing observation on a `RefreshAccount{None}` path).
//!   The tx failed; state is unchanged. Retry only after understanding
//!   why (fetch fresh observation, wait for next slot, etc.).
//! * `Ok(no mutation)` — engine returned `NonProgress` from
//!   `RefreshAccount{None}` because no observation was attached. Safe
//!   to retry once we have an observation to attach.
//! * `Ok(mutation)` — engine dispatched a progress step (`Liquidate`,
//!   `SettleBChunk`, `RefreshAccount{Some}`, `ReleaseSourceLiens`,
//!   `AdvanceClose`, `DeclareRecovery`, `FinalizeRecovery`,
//!   `CloseResolved`). NOT idempotent. Retry only after re-reading state
//!   and confirming the goal isn't already achieved. A6
//!   (`ReleaseSourceLiens`) progresses one domain per call
//!   (`V16_SOURCE_LIEN_RELEASE_CHUNK_DOMAINS = 1`); drain in a loop.
//!
//! For scheduling, those three collapse to `OutcomeClass`: Progress, Idle,
//! Failed. A tx that reverts with `EngineNonProgress` (0x16) is **idle**:
//! the wrapper's resolved branch and the Hybrid same-slot rule return it
//! atomically when there is nothing to do. Only other reverts are failures.

use anyhow::{anyhow, Context, Result};
use percolator_prog::ix::{CrankObservationHint, Instruction as ProgInstruction};
use solana_client::{
    client_error::{ClientError, ClientErrorKind},
    rpc_client::RpcClient,
    rpc_config::RpcTransactionConfig,
    rpc_request::{RpcError, RpcResponseErrorData},
};
use solana_transaction_status::{option_serializer::OptionSerializer, UiTransactionEncoding};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::{Keypair, Signature, Signer},
    transaction::Transaction,
};

use crate::binding::{layout_is_current, V16_LAYOUT_DISCRIMINATOR};
use crate::observation::FetchedObservation;

/// Classification of a submission's outcome. Encapsulates the three-way
/// shape described in the module docs and carries enough context to log
/// or reconcile downstream.
#[derive(Debug)]
pub enum CrankOutcome {
    HardReject {
        error: String,
        /// Friendly name for the wrapper's `Custom(N)` code, if the error
        /// message matched a known variant. `None` for non-wrapper errors
        /// (RPC issues, blockhash timeouts, etc.).
        error_name: Option<&'static str>,
        /// Program CU from the preflight simulation logs, when the RPC
        /// returned them.
        program_cu: Option<u64>,
        /// Whole-tx CU from the preflight simulation, when returned.
        tx_cu: Option<u64>,
    },
    Ok {
        signature: Signature,
        pre_state_hash: u64,
        post_state_hash: u64,
        mutated: bool,
        /// Byte-diff count between pre- and post-state on the target
        /// portfolio. Zero means the engine returned `NonProgress` (or
        /// a fresh refresh that didn't move any observable byte).
        target_bytes_diff: usize,
        /// Byte-diff count on the cranker reward portfolio, if provided.
        /// Non-zero indicates a fee share landed.
        reward_bytes_diff: Option<usize>,
        /// Byte-diff count on the market account (Resolved route only):
        /// non-zero when a backing hint expired a bucket.
        market_bytes_diff: Option<usize>,
        /// CU the percolator program itself consumed (from the tx logs).
        program_cu: Option<u64>,
        /// CU for the whole tx, including the compute-budget ixs and (on the
        /// Resolved route) the ATA create.
        tx_cu: Option<u64>,
        /// The tx landed, but its post-state could not be read (the error).
        /// The diffs above are then unknown, not zero. Classified as
        /// Progress: the wrapper accepted it, and a confirmed tx must never
        /// count as a failure.
        unreconciled: Option<String>,
    },
}

/// Scheduling view of a `CrankOutcome`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeClass {
    Progress,
    Idle,
    Failed,
}

impl CrankOutcome {
    pub fn class(&self) -> OutcomeClass {
        match self {
            CrankOutcome::Ok { mutated: true, .. }
            | CrankOutcome::Ok { unreconciled: Some(_), .. } => OutcomeClass::Progress,
            CrankOutcome::Ok { mutated: false, .. } => OutcomeClass::Idle,
            CrankOutcome::HardReject { error_name, .. } if is_non_progress(*error_name) => {
                OutcomeClass::Idle
            }
            CrankOutcome::HardReject { .. } => OutcomeClass::Failed,
        }
    }

    /// Short error key for quarantine streaks: the wrapper variant name,
    /// or the first line of the RPC error when the code is unknown.
    pub fn error_key(&self) -> Option<String> {
        match self {
            CrankOutcome::HardReject { error_name: Some(n), .. } => Some((*n).to_string()),
            CrankOutcome::HardReject { error, .. } => {
                Some(error.lines().next().unwrap_or("unknown").chars().take(120).collect())
            }
            CrankOutcome::Ok { .. } => None,
        }
    }

    /// Whole-tx CU (what the cycle budget is charged), when known.
    pub fn tx_cu(&self) -> Option<u64> {
        match self {
            CrankOutcome::Ok { tx_cu, .. } | CrankOutcome::HardReject { tx_cu, .. } => *tx_cu,
        }
    }

    pub fn program_cu(&self) -> Option<u64> {
        match self {
            CrankOutcome::Ok { program_cu, .. } | CrankOutcome::HardReject { program_cu, .. } => {
                *program_cu
            }
        }
    }
}

pub fn is_non_progress(error_name: Option<&str>) -> bool {
    error_name == Some("EngineNonProgress")
}

/// Parse `Program <id> consumed N of M compute units` for `program_id`.
/// Takes the last match: the top-level invocation logs its total last.
pub fn program_cu_from_logs(program_id: &Pubkey, logs: &[String]) -> Option<u64> {
    let prefix = format!("Program {program_id} consumed ");
    logs.iter().rev().find_map(|l| {
        let rest = l.strip_prefix(&prefix)?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

/// `(program_cu, tx_cu)` from a preflight-simulation failure, when the RPC
/// returned them.
fn preflight_cu(e: &ClientError, program_id: &Pubkey) -> (Option<u64>, Option<u64>) {
    match e.kind() {
        ClientErrorKind::RpcError(RpcError::RpcResponseError {
            data: RpcResponseErrorData::SendTransactionPreflightFailure(sim),
            ..
        }) => (
            sim.logs.as_deref().and_then(|l| program_cu_from_logs(program_id, l)),
            sim.units_consumed,
        ),
        _ => (None, None),
    }
}

/// `(program_cu, tx_cu)` for a confirmed signature.
pub fn fetch_cu(rpc: &RpcClient, program_id: &Pubkey, sig: &Signature) -> (Option<u64>, Option<u64>) {
    let cfg = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };
    let Ok(tx) = rpc.get_transaction_with_config(sig, cfg) else {
        return (None, None);
    };
    let Some(meta) = tx.transaction.meta else {
        return (None, None);
    };
    let tx_cu = match meta.compute_units_consumed {
        OptionSerializer::Some(v) => Some(v),
        _ => None,
    };
    let program_cu = match &meta.log_messages {
        OptionSerializer::Some(logs) => program_cu_from_logs(program_id, logs),
        _ => None,
    };
    (program_cu, tx_cu)
}

/// Match the `Custom(N)` code in a wrapper error message to a
/// `PercolatorError` variant name. Codes are the u32-cast enum
/// discriminants at the wrapper's `src/v16_program.rs:96` — see
/// `PercolatorError` there. Keep in sync manually or the friendly names
/// silently drift; not worth pulling in a proc-macro for this.
pub fn wrapper_error_name(msg: &str) -> Option<&'static str> {
    let hex_marker = "custom program error: 0x";
    let idx = msg.find(hex_marker)?;
    let after = &msg[idx + hex_marker.len()..];
    let hex_str: String = after
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    if hex_str.is_empty() {
        return None;
    }
    let code = u32::from_str_radix(&hex_str, 16).ok()?;
    Some(match code {
        0x00 => "InvalidMagic",
        0x01 => "InvalidVersion",
        0x02 => "AlreadyInitialized",
        0x03 => "NotInitialized",
        0x04 => "InvalidAccountKind",
        0x05 => "InvalidAccountLen",
        0x06 => "ExpectedSigner",
        0x07 => "ExpectedWritable",
        0x08 => "Unauthorized",
        0x09 => "InvalidInstruction",
        0x0A => "InvalidMint",
        0x0B => "InvalidTokenAccount",
        0x0C => "InvalidVaultAccount",
        0x0D => "InvalidTokenProgram",
        0x0E => "EngineInvalidConfig",
        0x0F => "EngineArithmeticOverflow",
        0x10 => "EngineProvenanceMismatch",
        0x11 => "EngineHiddenLeg",
        0x12 => "EngineInvalidLeg",
        0x13 => "EngineStale",
        0x14 => "EngineBStale",
        0x15 => "EngineLockActive",
        0x16 => "EngineNonProgress",
        0x17 => "EngineRecoveryRequired",
        0x18 => "EngineCounterOverflow",
        0x19 => "EngineCounterUnderflow",
        0x1A => "OracleInvalid",
        0x1B => "OracleStale",
        0x1C => "OracleConfTooWide",
        0x1D => "InvalidOracleKey",
        0x1E => "AssetGenerationMismatch",
        0x1F => "RentExemptRequired",
        _ => return None,
    })
}

/// Submit a single `PermissionlessCrank` with the given hint list.
///
/// Account order (verified in M1):
///   `[payer(signer) → market(writable) → portfolio(writable)
///     → oracle_accounts(readonly, hint-ordered)
///     → cranker_reward_portfolio(writable, optional last)]`
///
/// `pre_state_hash` / `post_state_hash` are cheap non-crypto fingerprints
/// used only for local mutation detection. Not authoritative.
pub fn submit_crank(
    rpc: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    market: &Pubkey,
    target_portfolio: &Pubkey,
    reward_portfolio: Option<&Pubkey>,
    observations: &[FetchedObservation],
    now_slot: u64,
) -> Result<CrankOutcome> {
    submit_crank_routed(
        rpc,
        program_id,
        payer,
        market,
        target_portfolio,
        &CrankRoute::Live { reward_portfolio },
        observations,
        now_slot,
        None,
    )
}

/// Account layout for a tag-5 crank. The wire is the same; the wrapper
/// reads the accounts differently once the market is Resolved.
#[derive(Debug, Clone)]
pub enum CrankRoute<'a> {
    /// `[payer(signer), market, portfolio, oracle tail…, reward portfolio?]`.
    Live { reward_portfolio: Option<&'a Pubkey> },
    /// Resolved (`handle_close_resolved`):
    /// `[owner, market, portfolio, owner_ata, vault, vault_authority, token_program]`.
    /// Account 0 must be the portfolio **owner** key. It needs to sign only
    /// while `slot - resolved_slot < force_close_delay_slots`; after that the
    /// crank is permissionless and the cranker only pays the fee. Slots 3–6
    /// carry the payout (to the owner's ATA, which the wrapper checks). An
    /// idempotent ATA create runs first so a missing ATA can't block the
    /// payout; the cranker pays that rent. No reward portfolio on this route.
    Resolved {
        owner: Pubkey,
        mint: Pubkey,
        vault: Pubkey,
        vault_authority: Pubkey,
    },
}

/// Like [`submit_crank`] with an explicit account route.
#[allow(clippy::too_many_arguments)]
pub fn submit_crank_routed(
    rpc: &RpcClient,
    program_id: &Pubkey,
    payer: &Keypair,
    market: &Pubkey,
    target_portfolio: &Pubkey,
    route: &CrankRoute<'_>,
    observations: &[FetchedObservation],
    now_slot: u64,
    deadline: Option<std::time::Instant>,
) -> Result<CrankOutcome> {
    let reward_portfolio = match route {
        CrankRoute::Live { reward_portfolio } => *reward_portfolio,
        CrankRoute::Resolved { .. } => None,
    };
    let (ixs, crank_ix_index) = build_crank_instructions(
        program_id,
        &payer.pubkey(),
        market,
        target_portfolio,
        route,
        observations,
        now_slot,
    );
    // On the Resolved route a backing hint can make market-only progress
    // (expiring a bucket) without touching the target, so the market
    // account is diffed too. This cannot turn an unrelated market write into
    // a false "progress": in `handle_close_resolved` any engine selection
    // other than `CloseResolved` reverts with EngineNonProgress, and
    // `CloseResolved` is always one state transition (`ProgressOnly` or
    // `Closed`). A landed Resolved crank therefore always made progress; a
    // no-progress one never lands, so it is classified Idle, never Ok.
    let track_market = matches!(route, CrankRoute::Resolved { .. });

    // Snapshot pre-state. Layout 18 is required on the decode path;
    // a stale-layout account is a hard reject, not a crank.
    let pre_target = rpc
        .get_account_data(target_portfolio)
        .context("read target portfolio pre-state")?;
    if !layout_is_current(&pre_target) {
        return Ok(CrankOutcome::HardReject {
            error: format!(
                "target portfolio layout_discriminator != {V16_LAYOUT_DISCRIMINATOR}"
            ),
            error_name: Some("InvalidAccountLen"),
            program_cu: None,
            tx_cu: None,
        });
    }
    let pre_reward = match reward_portfolio {
        Some(r) => Some(rpc.get_account_data(r).context("read reward portfolio pre-state")?),
        None => None,
    };
    let pre_market = if track_market {
        Some(rpc.get_account_data(market).context("read market pre-state")?)
    } else {
        None
    };
    let pre_state_hash = fnv_hash(&pre_target);

    // Submit with retry-on-blockhash-not-found (the test validator's RPC
    // occasionally races the bank). A preflight "Blockhash not found" means
    // the previous attempt was never forwarded, so a retry re-signs the same
    // instructions with a fresh blockhash and stays within the one budget
    // reservation the caller took. The deadline is re-checked before every
    // send.
    let mut last_err: Option<String> = None;
    for attempt in 0..5 {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return Err(DeadlineReached.into());
        }
        let (bh, _) = rpc
            .get_latest_blockhash_with_commitment(CommitmentConfig::processed())
            .context("get_latest_blockhash")?;
        let tx = Transaction::new_signed_with_payer(
            &ixs,
            Some(&payer.pubkey()),
            &[payer],
            bh,
        );
        // Re-check right before the send: the blockhash fetch itself may have
        // crossed the deadline. The deadline is an ADMISSION deadline: once a
        // tx is sent, its confirmation is bounded by blockhash expiry (the
        // client's own wait), not by the cycle deadline.
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            return Err(DeadlineReached.into());
        }
        match rpc.send_and_confirm_transaction(&tx) {
            Ok(signature) => {
                // The tx LANDED. From here on, read failures must not turn it
                // into a failure (that would strike a healthy account and drop
                // the record): keep a confirmed outcome and note what could
                // not be reconciled.
                let (program_cu, tx_cu) = if measure_cu() {
                    fetch_cu(rpc, program_id, &signature)
                } else {
                    (None, None)
                };
                let post = (|| -> Result<(Vec<u8>, Option<usize>, Option<usize>)> {
                    let post_target = rpc
                        .get_account_data(target_portfolio)
                        .context("read target portfolio post-state")?;
                    let reward_bytes_diff = match (reward_portfolio, &pre_reward) {
                        (Some(r), Some(a)) => Some(byte_diff_count(
                            a,
                            &rpc.get_account_data(r).context("read reward portfolio post-state")?,
                        )),
                        _ => None,
                    };
                    let market_bytes_diff = match &pre_market {
                        Some(a) => Some(byte_diff_count(
                            a,
                            &rpc.get_account_data(market).context("read market post-state")?,
                        )),
                        None => None,
                    };
                    Ok((post_target, reward_bytes_diff, market_bytes_diff))
                })();
                return Ok(match post {
                    Ok((post_target, reward_bytes_diff, market_bytes_diff)) => {
                        let target_bytes_diff = byte_diff_count(&pre_target, &post_target);
                        let mutated = target_bytes_diff > 0
                            || reward_bytes_diff.is_some_and(|d| d > 0)
                            || market_bytes_diff.is_some_and(|d| d > 0);
                        CrankOutcome::Ok {
                            signature,
                            pre_state_hash,
                            post_state_hash: fnv_hash(&post_target),
                            mutated,
                            target_bytes_diff,
                            reward_bytes_diff,
                            market_bytes_diff,
                            program_cu,
                            tx_cu,
                            unreconciled: None,
                        }
                    }
                    Err(e) => CrankOutcome::Ok {
                        signature,
                        pre_state_hash,
                        post_state_hash: pre_state_hash,
                        mutated: false,
                        target_bytes_diff: 0,
                        reward_bytes_diff: None,
                        market_bytes_diff: None,
                        program_cu,
                        tx_cu,
                        unreconciled: Some(format!("{e:#}")),
                    },
                });
            }
            Err(e) => {
                let (program_cu, tx_cu) = preflight_cu(&e, program_id);
                let msg = format!("{e}");
                if msg.contains("Blockhash not found") && attempt < 4 {
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    last_err = Some(msg);
                    continue;
                }
                // Definitive: the preflight simulation rejected it, or the tx
                // executed and failed. Anything else (transport error, status
                // poll failure, "unable to confirm") is UNCERTAIN: the tx may
                // have landed. Ask for its status once before assigning blame.
                let definitive = matches!(
                    e.kind(),
                    ClientErrorKind::TransactionError(_)
                        | ClientErrorKind::RpcError(RpcError::RpcResponseError {
                            data: RpcResponseErrorData::SendTransactionPreflightFailure(_),
                            ..
                        })
                );
                if !definitive {
                    let signature = tx.signatures[0];
                    match rpc.get_signature_status(&signature) {
                        Ok(Some(Ok(()))) => {
                            return Ok(CrankOutcome::Ok {
                                signature,
                                pre_state_hash,
                                post_state_hash: pre_state_hash,
                                mutated: false,
                                target_bytes_diff: 0,
                                reward_bytes_diff: None,
                                market_bytes_diff: None,
                                program_cu: None,
                                tx_cu: None,
                                unreconciled: Some(format!("landed, but confirmation errored: {msg}")),
                            })
                        }
                        Ok(Some(Err(te))) => {
                            let error = format!("{te}");
                            let error_name = wrapper_error_name_for_ix(&error, crank_ix_index);
                            return Ok(CrankOutcome::HardReject { error, error_name, program_cu, tx_cu });
                        }
                        _ => return Err(Unconfirmed { signature, error: msg }.into()),
                    }
                }
                let error_name = wrapper_error_name_for_ix(&msg, crank_ix_index);
                return Ok(CrankOutcome::HardReject { error: msg, error_name, program_cu, tx_cu });
            }
        }
    }
    Err(anyhow!(last_err.unwrap_or_else(|| "unknown submit error".to_string())))
}

/// The tx's instruction list: compute-budget ixs, (Resolved route) an
/// idempotent owner-ATA create, then the crank. Returns the crank's index,
/// which error attribution needs. Pure: no RPC.
pub fn build_crank_instructions(
    program_id: &Pubkey,
    payer: &Pubkey,
    market: &Pubkey,
    target_portfolio: &Pubkey,
    route: &CrankRoute<'_>,
    observations: &[FetchedObservation],
    now_slot: u64,
) -> (Vec<Instruction>, usize) {
    // Hint counts must match the fetched oracle accounts, otherwise the
    // wrapper consumes the wrong number of tail accounts per hint.
    let hints: Vec<CrankObservationHint> = observations
        .iter()
        .map(|obs| CrankObservationHint {
            asset_index: obs.asset_index,
            oracle_accounts: obs.oracle_account_count(),
        })
        .collect();
    let mut ixs = vec![cu_ix(), heap_ix()];
    let accounts = match route {
        // Live: payer, market, portfolio, then oracle accounts in hint
        // order, then optional reward portfolio.
        CrankRoute::Live { reward_portfolio } => {
            let mut accounts = vec![
                AccountMeta::new(*payer, true),
                AccountMeta::new(*market, false),
                AccountMeta::new(*target_portfolio, false),
            ];
            for obs in observations {
                for oracle_ai in &obs.oracle_accounts {
                    accounts.push(AccountMeta::new_readonly(*oracle_ai, false));
                }
            }
            if let Some(reward) = reward_portfolio {
                accounts.push(AccountMeta::new(**reward, false));
            }
            accounts
        }
        CrankRoute::Resolved {
            owner,
            mint,
            vault,
            vault_authority,
        } => {
            let owner_ata = spl_associated_token_account::get_associated_token_address(owner, mint);
            ixs.push(
                spl_associated_token_account::instruction::create_associated_token_account_idempotent(
                    payer,
                    owner,
                    mint,
                    &spl_token::ID,
                ),
            );
            vec![
                AccountMeta::new_readonly(*owner, owner == payer),
                AccountMeta::new(*market, false),
                AccountMeta::new(*target_portfolio, false),
                AccountMeta::new(owner_ata, false),
                AccountMeta::new(*vault, false),
                AccountMeta::new_readonly(*vault_authority, false),
                AccountMeta::new_readonly(spl_token::ID, false),
            ]
        }
    };
    let crank_index = ixs.len();
    ixs.push(Instruction {
        program_id: *program_id,
        accounts,
        data: ProgInstruction::PermissionlessCrank {
            now_slot,
            observations: hints,
        }
        .encode(),
    });
    (ixs, crank_index)
}

/// Like [`wrapper_error_name`], but only when the error came from the
/// instruction at `ix_index` (the crank). Best-effort: when the message
/// carries no "Error processing Instruction N" text, the bare code is
/// mapped as before and could in principle belong to another instruction. A custom error from another
/// instruction in the tx (the ATA create, a token program) is not a
/// wrapper error and gets no wrapper name.
pub fn wrapper_error_name_for_ix(msg: &str, ix_index: usize) -> Option<&'static str> {
    let marker = "Error processing Instruction ";
    if let Some(i) = msg.find(marker) {
        let n: String = msg[i + marker.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if n.parse::<usize>().ok() != Some(ix_index) {
            return None;
        }
    }
    wrapper_error_name(msg)
}

/// The cycle deadline passed before the tx was sent: nothing went on chain.
/// The caller treats it as budget deferral, never as an account failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineReached;

impl std::fmt::Display for DeadlineReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("cycle deadline reached before send")
    }
}

impl std::error::Error for DeadlineReached {}

/// The tx was sent but its fate is unknown (confirmation errored and the
/// status query gave no answer). Not an account failure: the caller keeps
/// the CU reservation charged and does not strike the account.
#[derive(Debug, Clone)]
pub struct Unconfirmed {
    pub signature: Signature,
    pub error: String,
}

impl std::fmt::Display for Unconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tx {} unconfirmed: {}", self.signature, self.error)
    }
}

impl std::error::Error for Unconfirmed {}

/// Compute-unit limit requested by every crank tx. The scheduler reserves
/// this much CU budget before each tx.
pub const TX_CU_LIMIT: u32 = 1_400_000;

/// `CRANKER_MEASURE_CU=0` skips the extra `getTransaction` per crank.
fn measure_cu() -> bool {
    std::env::var("CRANKER_MEASURE_CU").ok().as_deref() != Some("0")
}

fn cu_ix() -> Instruction {
    ComputeBudgetInstruction::set_compute_unit_limit(TX_CU_LIMIT)
}

fn heap_ix() -> Instruction {
    ComputeBudgetInstruction::request_heap_frame(128 * 1024)
}

fn byte_diff_count(a: &[u8], b: &[u8]) -> usize {
    let common = a.len().min(b.len());
    let mut diff = a[..common]
        .iter()
        .zip(b[..common].iter())
        .filter(|(x, y)| x != y)
        .count();
    diff += a.len().abs_diff(b.len());
    diff
}

/// Cheap non-crypto 64-bit fingerprint (FNV-1a). Just for local
/// pre/post mutation detection; never authoritative for anything.
fn fnv_hash(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use percolator_prog::ix::{CrankObservationHint, Instruction as ProgInstruction};

    fn reject(name: Option<&'static str>) -> CrankOutcome {
        CrankOutcome::HardReject {
            error: "x".into(),
            error_name: name,
            program_cu: None,
            tx_cu: None,
        }
    }

    #[test]
    fn non_progress_revert_is_idle_not_failed() {
        assert_eq!(
            wrapper_error_name("custom program error: 0x16"),
            Some("EngineNonProgress")
        );
        assert_eq!(reject(Some("EngineNonProgress")).class(), OutcomeClass::Idle);
        assert_eq!(reject(Some("EngineCounterUnderflow")).class(), OutcomeClass::Failed);
        assert_eq!(reject(None).class(), OutcomeClass::Failed);
        let ok = |mutated| CrankOutcome::Ok {
            signature: Signature::default(),
            pre_state_hash: 0,
            post_state_hash: 0,
            mutated,
            target_bytes_diff: 0,
            reward_bytes_diff: None,
            market_bytes_diff: None,
            program_cu: None,
            tx_cu: None,
            unreconciled: None,
        };
        assert_eq!(ok(false).class(), OutcomeClass::Idle);
        assert_eq!(ok(true).class(), OutcomeClass::Progress);
    }

    /// Review finding: the Resolved route had no account-meta test. The
    /// wrapper's close_resolved branch reads exactly this layout.
    #[test]
    fn resolved_route_account_layout() {
        let (program, payer, market, target) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let (owner, mint, vault, vault_auth) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let route = CrankRoute::Resolved { owner, mint, vault, vault_authority: vault_auth };
        let hint = FetchedObservation { asset_index: 2, oracle_accounts: vec![] };
        let (ixs, idx) = build_crank_instructions(&program, &payer, &market, &target, &route, &[hint], 9);
        assert_eq!(idx, 3, "cu, heap, ATA create, crank");
        assert_eq!(ixs[2].program_id, spl_associated_token_account::ID);
        let crank = &ixs[idx];
        assert_eq!(crank.program_id, program);
        let keys: Vec<_> = crank.accounts.iter().map(|m| (m.pubkey, m.is_signer, m.is_writable)).collect();
        let owner_ata = spl_associated_token_account::get_associated_token_address(&owner, &mint);
        assert_eq!(
            keys,
            vec![
                (owner, false, false),
                (market, false, true),
                (target, false, true),
                (owner_ata, false, true),
                (vault, false, true),
                (vault_auth, false, false),
                (spl_token::ID, false, false),
            ]
        );
        // Payer is not a signer of the crank unless it owns the portfolio.
        let route_self = CrankRoute::Resolved { owner: payer, mint, vault, vault_authority: vault_auth };
        let (ixs, idx) = build_crank_instructions(&program, &payer, &market, &target, &route_self, &[], 9);
        assert!(ixs[idx].accounts[0].is_signer);
        // Wire: one oracle-less hint for asset 2.
        let (ixs, idx) = build_crank_instructions(
            &program, &payer, &market, &target, &route,
            &[FetchedObservation { asset_index: 2, oracle_accounts: vec![] }], 9,
        );
        assert_eq!(&ixs[idx].data[9..], &[1, 2, 0, 0]);
    }

    #[test]
    fn live_route_account_layout() {
        let (program, payer, market, target, reward, oracle) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let route = CrankRoute::Live { reward_portfolio: Some(&reward) };
        let obs = FetchedObservation { asset_index: 0, oracle_accounts: vec![oracle] };
        let (ixs, idx) = build_crank_instructions(&program, &payer, &market, &target, &route, &[obs], 1);
        assert_eq!(idx, 2);
        let keys: Vec<_> = ixs[idx].accounts.iter().map(|m| m.pubkey).collect();
        assert_eq!(keys, vec![payer, market, target, oracle, reward]);
        assert!(ixs[idx].accounts[0].is_signer);
    }

    #[test]
    fn error_names_only_for_the_crank_instruction() {
        let ours = "Error processing Instruction 3: custom program error: 0x16";
        let ata = "Error processing Instruction 2: custom program error: 0x1";
        assert_eq!(wrapper_error_name_for_ix(ours, 3), Some("EngineNonProgress"));
        assert_eq!(wrapper_error_name_for_ix(ata, 3), None);
        // No instruction index in the message: fall back to the code.
        assert_eq!(wrapper_error_name_for_ix("custom program error: 0x13", 3), Some("EngineStale"));
    }

    #[test]
    fn program_cu_parses_top_level_line() {
        let pid = Pubkey::new_unique();
        let logs = vec![
            "Program ComputeBudget111111111111111111111111111111 consumed 150 of 1400000 compute units".to_string(),
            format!("Program {pid} invoke [1]"),
            format!("Program {pid} consumed 17975 of 1399700 compute units"),
            format!("Program {pid} success"),
        ];
        assert_eq!(program_cu_from_logs(&pid, &logs), Some(17_975));
        assert_eq!(program_cu_from_logs(&Pubkey::new_unique(), &logs), None);
    }

    #[test]
    fn permissionless_crank_wire_has_no_close_q() {
        let ix = ProgInstruction::PermissionlessCrank {
            now_slot: 1,
            observations: vec![CrankObservationHint {
                asset_index: 0,
                oracle_accounts: 0,
            }],
        };
        let bytes = ix.encode();
        // tag=5 (1) | now_slot (8) | n (1) | hint (asset_index 2 + oracle_accounts 1)
        assert_eq!(bytes.len(), 1 + 8 + 1 + 3);
        assert_eq!(bytes[0], 5);
        let decoded = ProgInstruction::decode(&bytes).expect("round-trip");
        match decoded {
            ProgInstruction::PermissionlessCrank {
                now_slot,
                observations,
            } => {
                assert_eq!(now_slot, 1);
                assert_eq!(observations.len(), 1);
            }
            other => panic!("unexpected decode: {other:?}"),
        }
    }
}
