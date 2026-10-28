# Architecture

```
crates/
  cranker/            library + the `drip` binary
    src/
      discovery.rs    getProgramAccounts (header memcmp) → market-scoped candidates
      binding.rs      portfolio_id / sequence / position_epoch bindings, layout check
      observation.rs  per-asset oracle-profile walk (Manual/Hybrid/EWMA/AuthMark), Pyth resolver
      plan_gate.rs    delegates to the engine's auto_crank_plan_requires_caller_observation
      resolved.rs     Resolved-market backing hint (lapsed Fresh buckets, round-robin)
      work.rs         off-chain priority estimate; sub-atom funding remainder = settled
      scheduler.rs    class quotas and turns, per-class cursors, per-tx budget, outcome tally
      quarantine.rs   same-error streaks → capped exponential backoff, JSON events
      submit.rs       tx build (Live / Resolved account routes), send, classify, CU from logs
      cycle.rs        one discover → hint → schedule → submit cycle (shared by everything)
      setup.rs        one-asset market bootstrap for the binary and tests
      main.rs         binary `drip`
    tests/            validator-gated integration tests (conservation, rollback, lifecycle)
  bringup/            local smoke test: init market → deposit → crank → fee routing
```

## The crank cycle

1. **Discover.** All `KIND_PORTFOLIO` accounts of the program. Keep the ones
   that are in the target market, initialized, on layout 18, and not
   excluded.
2. **Read the market once.** Use `state::read_market` to get the mode,
   backing buckets, assets and config.
3. **Plan per candidate.**
   - **Live / Recovery:** one hint per active leg asset, each with the
     oracle accounts its profile needs.
   - **Resolved:** at most one hint `{asset, oracle_accounts: 0}` naming an
     asset with a lapsed Fresh bucket. Buckets lapsed by their raw expiry
     come first (what the pinned engine checks). Only when there are none
     does the keeper fall back to the resolved-horizon cap (a newer engine
     rule; on the pinned engine such a hint is answered NonProgress, i.e.
     idle). Several due assets are served round-robin by cycle. The wrapper reads Resolved accounts
     as `[owner, market, portfolio, owner ATA, vault, vault authority,
     token program]`. The owner has to sign only inside
     `force_close_delay_slots`; there the keeper skips third-party accounts
     as idle.
4. **Order.** Three priority classes (liquidatable, pending, quiet) share
   the cycle's tx budget 2:1:1, taking turns in a fixed pattern. The turn
   and each class's position (the last pubkey it visited) persist across
   cycles, so a deferred account is first next time even as accounts come
   and go. While another class still needs its share, an account's drain is
   capped at its own class's remaining quota.
5. **Submit and classify.** One scheduler attempt can send several
   transactions, because an A6 source-lien release moves one domain per
   call. Draining continues only while the target account changes. Every
   transaction is admitted by the budget before it is sent (wall clock, tx
   count, CU reserve) and counted whatever its outcome.
   - A landed step stays progress even if a later read or step fails.
   - A deadline that passes before the send is a deferral, which ends the
     cycle.
   - A sent tx whose fate is unknown is reported as unconfirmed, with its
     signature.
6. **Quarantine.** A failure feeds the same-error streak, and progress
   clears it. Idle clears it too, while counting the idle streak used for
   stuck reporting. Deferred and unconfirmed attempts touch neither. After
   one strike, the next same-error failure re-quarantines with a doubled
   window.

## Why the keeper never hand-rolls engine rules

The plan truth table, the layout and the instruction encoding all come from
upstream (`percolator::auto_crank_plan_requires_caller_observation` and
`percolator_prog::{ix, state}`). The keeper's own heuristics, such as
priority and plan class, only order work and label logs. They never decide
that an account is done.
