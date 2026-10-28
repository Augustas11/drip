# Security

This is a **reference keeper for a local validator**. It holds
no funds, runs no service, and has no mainnet path.

## Reporting

Report suspected vulnerabilities in this repository privately, through
GitHub private vulnerability reporting: **https://github.com/Augustas11/drip/security/advisories/new**. Please
don't open a public issue. Expect an
acknowledgement within a few days.

Issues in the upstream engine or wrapper belong upstream, under their own
disclosure process. This repository does not handle them.

## Scope

- **In scope:** key handling in scripts, transaction construction, and
  anything that could make the keeper sign something other than what it
  logs.
- **Out of scope:** upstream program behavior, local-validator-only
  denial of service, and the bootstrap market's synthetic parameters.
