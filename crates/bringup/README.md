# bring-up smoke test

Run by `scripts/m1_bringup.sh` after the deploy. It covers:
- creating a mint and market;
- a deposit;
- one `PermissionlessCrank`;
- a `SyncMaintenanceFee` with a keeper portfolio appended, which proves
  fee-share routing.

It prints the market layout facts the keeper relies on.
