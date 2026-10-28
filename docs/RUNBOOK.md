# Runbook

Everything here targets a **local `solana-test-validator`**. This keeper is
reference / dry-run / local-validator only.

## 1. Prerequisites

| Tool | Version | Notes |
|---|---|---|
| Rust | stable, 1.80 or newer | `rustup` |
| Solana CLI (Agave) | 2.0.x (tested 2.0.21) | `sh -c "$(curl -sSfL https://release.anza.xyz/v2.0.21/install)"` |
| SBF platform-tools | **v1.52** | Side-loaded on first use by `cargo build-sbf --tools-version v1.52`. The default platform-tools of the 2.0 CLI do not build the wrapper at this pin; always pass the flag. The scripts do. |
| git, python3 | any recent | scripts |

`scripts/env.sh` puts the Solana and cargo bins on `PATH` and sets the
layout variables the scripts use. Source it in every shell.

## 2. Fetch and build the wrapper at the pinned SHA

```bash
./scripts/fetch_upstream.sh
```

This clones `percolator-prog` into `upstream/` (gitignored), checks out
`PERCOLATOR_PROG_SHA` from `scripts/pins.env`, and confirms that the
wrapper's Cargo engine pin equals ours.

- The host crates use the wrapper as a path dependency, for its instruction
  encoder and account decoders.
- The engine is a git dependency pinned by `rev` in `crates/*/Cargo.toml`.

The SBF program is built by the bring-up script below. To build it by hand:

```bash
(cd upstream/percolator-prog && cargo build-sbf --tools-version v1.52)
```

## 3. Local bring-up

```bash
FORCE_SBF_REBUILD=1 ./scripts/m1_bringup.sh
```

The script:
1. resets the ledger in `.solana-test-ledger/` and starts the validator on
   `127.0.0.1:8899`;
2. funds the default keypair `~/.config/solana/id.json`;
3. deploys the program under the build-time keypair id;
4. runs the bring-up smoke test (`crates/bringup`): market, deposit, crank,
   fee routing.

`KEEP_VALIDATOR=1` reuses a running validator. The program id is printed and
exported as `PERCOLATOR_PROGRAM_ID`.

## 4. Run the keeper

```bash
source scripts/env.sh
export PERCOLATOR_PROGRAM_ID=$(solana-keygen pubkey upstream/percolator-prog/target/deploy/percolator_prog-keypair.json)
CRANKER_CYCLES=3 cargo run --release -p drip --bin drip
```

The binary bootstraps its own one-asset market and runs N cycles against
it.

## 5. Configuration (environment)

| Variable | Default | Meaning |
|---|---|---|
| `PERCOLATOR_PROGRAM_ID` | — (required by the binary) | Deployed wrapper program id |
| `CRANKER_CYCLES` | 3 | Loop iterations |
| `CRANKER_CYCLE_MS` | 30000 | Per-cycle wall budget |
| `CRANKER_CYCLE_CU` | 24000000 | Per-cycle CU budget. Each tx reserves its full 1.4M limit up front and is settled to its measured whole-tx CU; unknown CU stays charged at 1.4M. Values below 1.4M are clamped with a warning. |
| `CRANKER_CYCLE_MAX_TX` | 64 | Per-cycle tx budget. Each tx counts, including drain steps. Split 2:1:1 across the non-empty priority classes. |
| `CRANKER_ATTEMPT_MAX_TX` | 4 | Most txs one account may use per cycle (bounds an A6 lien drain) |
| `CRANKER_IDLE_STUCK_CYCLES` | 10 | Idle cycles in a row before an account still owed work is reported as stuck |
| `CRANKER_ENABLE_FEE_SHARES` | off | `1` opts in to fee paths: maintenance fee and 50% cranker shares on the bootstrap market, reward portfolio appended to cranks |
| `CRANKER_WITHDRAWAL_TEST` | off | `1` runs a local withdraw spot-check from the keeper portfolio (requires the fee opt-in) |
| `CRANKER_FAIL_ON_ERRORS` | off | `1` exits nonzero if any attempt fails or ends unconfirmed (CI sets it) |
| `CRANKER_QUARANTINE_THRESHOLD` | 3 | Same-error failures in a row before quarantine |
| `CRANKER_QUARANTINE_BASE_CYCLES` | 2 | First backoff window, in cycles. It doubles on each re-quarantine. |
| `CRANKER_QUARANTINE_MAX_CYCLES` | 64 | Backoff cap |
| `CRANKER_MEASURE_CU` | 1 | `0` skips the per-tx `getTransaction` that reads CU from the logs |
| `CRANKER_PYTH_MODE` | off | `canonical` enables the shard-0 Pyth PDA fallback for Hybrid assets |

**Keys.** The keeper signs with `~/.config/solana/id.json`, a local
validator key. The integration tests read `PERCOLATOR_TEST_PAYER` when it is
set. Never commit keys; `.gitignore` excludes `keys/` and `.env`.

## 6. Reading the output

Every submission prints a line with the target, plan class, outcome and
program CU. Every cycle ends with a summary:

```
cycle 12 summary: progressed=4 idle=7 failed=1 unconfirmed=0 quarantined_skipped=2 deferred=0 new_quarantine=1 idle_pending_stuck=0 txs=14 cu=1365077 max_attempt_cu=275934
```

| Field | Meaning |
|---|---|
| `progressed` | The engine took a step (state moved). An A6 lien drain counts once per account. |
| `idle` | Nothing to do: `Ok(no-mut)`, or a revert with `EngineNonProgress` (0x16). On a Resolved market this also counts third-party accounts skipped inside the owner-signer window. |
| `failed` | Any other revert, engine error or RPC error. Only that account is affected. |
| `unconfirmed` | Txs that were sent but whose fate is unknown (confirmation errored and the status query gave no answer). Never counted against the account; each one is also printed as a JSON event with its signature. |
| `quarantined_skipped` | Accounts inside a backoff window. They spend no budget. |
| `deferred` | Accounts not attempted because a budget ran out, including one whose deadline passed before its tx was sent. Each class resumes after the last account it visited, so these come first next cycle. |
| `idle_pending_stuck` | Accounts still owed work (a Resolved claimant with value or an unpaid receipt, or a Live account the estimate calls liquidatable or pending) that stayed idle for `CRANKER_IDLE_STUCK_CYCLES` cycles. |
| `txs`, `cu`, `max_attempt_cu` | Txs begun, CU charged this cycle (measured, or the full reservation when unknown), and the largest single-account charge. |

The wall-clock budget is an **admission** deadline: no tx is sent after it.
A tx already sent waits for its confirmation, which normally ends at
blockhash expiry (about a minute) but can last longer if the validator
stops advancing.

Quarantine events are single JSON lines:

```json
{"event":"quarantine","account":"…","error":"EngineStale","cycle":33,"slot":3895,"plan_class":"live:pending","strike":3,"backoff_cycles":8,"until_cycle":42}
```

`plan_class` is the keeper's own label. The engine's chosen plan isn't
visible from outside the transaction.

## 7. Re-pinning when upstream moves

1. Fetch upstream and diff the wrapper and engine source between the old
   pin and the candidate. The wrapper lives in `src/v16_program.rs`; the
   engine in `src/v16.rs` and `spec.md`.
2. Confirm that the `PermissionlessCrank` wire, the `AutoCrankPlanV16` enum
   and `V16_LAYOUT_DISCRIMINATOR` are unchanged. If any of them changed,
   it's a rewrite, not a re-pin.
3. Update `scripts/pins.env`, plus the engine `rev` in both
   `crates/*/Cargo.toml`. The engine rev must equal the wrapper's own Cargo
   pin, or two `percolator` crates get linked.
4. Run `cargo update -p percolator`, then `./scripts/fetch_upstream.sh`,
   `FORCE_SBF_REBUILD=1 ./scripts/m1_bringup.sh`, a 3-cycle keeper run, the
   unit tests and the validator integration tests (`cargo test --
   --include-ignored` with `PERCOLATOR_TEST_RPC` set to the local
   validator).
5. Re-measure CU and update the README tables.

## 8. Known limitations

- **Unreferenced lapsed backing wedges Live refresh** (engine side, current
  pins and the engine's newer master).
  - A Fresh backing bucket that passes its `expiry_slot` without any
    account referencing its domain is not expired by any Live instruction.
    The account-scoped lapse scan only covers referenced domains, and the
    asset-level scan runs only on Resolved markets.
  - A refresh that would attribute new positive PnL to that domain returns
    `Stale`. The account can't be refreshed or liquidated until the market
    resolves.
  - The keeper quarantines such accounts and keeps the rest progressing.
    Reported upstream as #243.
- **Residual value at CloseSlab.** Backing principal that lapses unused
  goes to the residual pool, and `CloseSlab` burns claim-free residue.
  Operators should withdraw unused backing before it expires.
- **The keeper doesn't decide the plan.** Priority is an off-chain
  estimate; the engine's selector is the only authority on what a crank
  does.
- **No priority fees** and no leader-slot awareness. Pyth accounts for
  Hybrid assets need operator-supplied mapping, or the canonical PDA mode.
- **Integration tests are gated.** They are `#[ignore]`d by default and run
  with `cargo test -- --include-ignored` when `PERCOLATOR_TEST_RPC` (a local
  validator only) and `PERCOLATOR_PROGRAM_ID` are set. `invariants` checks
  `PERCOLATOR_TEST_MARKET` if given and otherwise bootstraps its own market;
  the others always bootstrap their own.
- **`PERCOLATOR_SKIP_PIN_CHECK=1`** downgrades the build-time wrapper pin
  check to a warning. Use it only for a deliberate build against a wrapper
  other than the pin.
