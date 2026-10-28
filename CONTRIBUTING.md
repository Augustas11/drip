# Contributing

- **Before sending a change:** run `cargo build` and `cargo test` from the
  workspace root. If you touched the crank path, also run
  `./scripts/m1_bringup.sh`, a 3-cycle dry run and the validator
  integration tests (`cargo test -- --include-ignored` with
  `PERCOLATOR_TEST_RPC` and `PERCOLATOR_PROGRAM_ID` set).
- **Keep the keeper's rules upstream-derived.** Use the engine predicate
  and the wrapper's decoders; don't hand-roll a match on plan variants or
  byte offsets.
- **Fee, bounty and mainnet paths are out of scope.**
- **No keys, mnemonics or RPC tokens in commits.**
- **License:** Apache-2.0 (see `LICENSE`). Contributions are accepted
  under the same license.
