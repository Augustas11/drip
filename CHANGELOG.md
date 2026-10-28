# Changelog

## v0.1.0 (unreleased)

First public reference release.

- **Pins.** Wrapper `percolator-prog` `5cb331dd`, engine `percolator`
  `4db11a8c` (the wrapper's Cargo pin). Layout 18.
- **Crank cycle.**
  - Market-scoped discovery and Live hints per active leg asset.
  - On Resolved markets: a backing hint plus the owner/payout account
    route.
  - Progress / Idle / Failed classification, with `EngineNonProgress`
    counted as idle.
- **Stuck-account hardening.**
  - Per-account same-error quarantine with capped exponential backoff and
    JSON events.
  - Per-cycle wall, CU and tx budget.
  - Liquidatable-first ordering with per-class rotation and a quiet
    reserve.
- **CU baseline.** No-op crank: 18,363 program CU by default; 18,565
  (18,889 tx total) with the reward portfolio appended (fee opt-in).
- **Known issue.** Unreferenced lapsed backing wedges Live refresh
  (upstream #243); see RUNBOOK → Known limitations.
