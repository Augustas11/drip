# drip

An independent, unofficial reference keeper for the Percolator protocol.
Not affiliated with the upstream authors. Not a paid service.

It is a keeper (cranker) for the Percolator perpetuals risk engine on Solana.

> **Status: reference / dry-run / local-validator only.** Nothing here is
> deployed on any public cluster. Fee paths are off by default
> (`CRANKER_ENABLE_FEE_SHARES=1` opts in on a local market). There's no fee
> claiming, no bounty path and no mainnet path. The
> upstream program has no published program ID yet; until it does, this code
> runs against a local `solana-test-validator`.

Upstream (read-only dependencies):
[`aeyakovenko/percolator`](https://github.com/aeyakovenko/percolator) (the
engine library) and
[`aeyakovenko/percolator-prog`](https://github.com/aeyakovenko/percolator-prog)
(the Solana wrapper program).

## What it does

Each cycle, the keeper:

1. **Discovers** every portfolio of a market (`getProgramAccounts`, header
   memcmp) and decodes it with the wrapper's own decoders.
2. **Builds the crank hints.** On a Live market, one hint per active leg
   asset. On a Resolved market, at most one oracle-less hint naming the
   asset whose backing bucket has lapsed. The Resolved crank routes the
   payout to the owner's token account.
3. **Schedules the crank.** Liquidatable accounts, accounts with pending
   work and quiet ones share each cycle's tx budget 2:1:1. The class order
   and each class's position carry over between cycles, so no class and no
   account is starved. The cycle spends a wall-clock, compute-unit and tx
   budget that is checked before every send. One failing account never
   blocks the queue.
4. **Submits `PermissionlessCrank`** and classifies each result:
   - **Progress:** the target's state changed.
   - **Idle:** the target didn't change, or the engine answered
     `EngineNonProgress`.
   - **Failed:** any other revert.
   - **Deferred / unconfirmed:** budget ran out before the send, or a sent
     tx's fate is unknown. Neither counts against the account.
5. **Quarantines** accounts that fail with the same engine error three times
   in a row. Backoff grows exponentially and is capped, and each quarantine
   prints one JSON line you can grep for.

## Pins

| Surface | Pin |
|---|---|
| Wrapper `percolator-prog` | `5cb331dde354517c6371a8acf92cecb194f3bb73` |
| Engine `percolator` | `4db11a8cb0053815e23a35d3a7d3edc265d8d866` (the wrapper's own Cargo pin) |
| Engine layout discriminator | 18 |

`scripts/pins.env` is the single source of truth for scripts.
`scripts/fetch_upstream.sh` refuses to continue if the wrapper's engine pin
and ours disagree.

## Compute-unit baselines (program CU, tx total in parentheses)

| Path | CU |
|---|---|
| No-op `PermissionlessCrank`, 1 asset (default, no reward portfolio) | 18,363 |
| Same, with the reward portfolio appended (fee opt-in) | 18,565 (18,889) |

## Quickstart

```bash
./scripts/fetch_upstream.sh                 # wrapper @ pinned SHA → upstream/
FORCE_SBF_REBUILD=1 ./scripts/m1_bringup.sh # local validator + deploy + smoke test
source scripts/env.sh
export PERCOLATOR_PROGRAM_ID=$(solana-keygen pubkey upstream/percolator-prog/target/deploy/percolator_prog-keypair.json)
CRANKER_CYCLES=3 cargo run --release -p drip --bin drip
```

Prerequisites, configuration and how to read the output are covered in
[docs/RUNBOOK.md](docs/RUNBOOK.md).

## Known issue on the current pins

Under load, some accounts can become unrefreshable in Live. It happens when
a backing bucket lapses (passes its expiry) without any account referencing
it, and a later refresh would attribute new positive PnL to that domain.
Every crank on those accounts then reverts with `EngineStale` until the
market resolves.

This is an engine-side liveness gap, reported upstream as
[aeyakovenko/percolator#243](https://github.com/aeyakovenko/percolator/issues/243).
The keeper quarantines affected accounts and keeps cranking everyone else.
See [docs/RUNBOOK.md](docs/RUNBOOK.md) → Known limitations.

## CI

`.github/workflows/ci.yml` has two jobs:

- **build + unit tests:** runs on pushes to `main`, on pull requests and on
  manual dispatch, against the pinned wrapper.
- **e2e:** runs on pull requests and manual dispatch. It does the SBF build
  (platform-tools v1.52), a local-validator bring-up, a 3-cycle dry run and
  the validator integration tests.

Nothing in either job is non-gating.

## Docs

- [docs/RUNBOOK.md](docs/RUNBOOK.md): prerequisites, build, bring-up, running, configuration, re-pinning, limitations
- [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md): modules and the crank cycle
- [CHANGELOG.md](CHANGELOG.md), [SECURITY.md](SECURITY.md), [CONTRIBUTING.md](CONTRIBUTING.md)

## License

Apache-2.0. See [LICENSE](LICENSE).
