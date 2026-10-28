#!/usr/bin/env bash
# Idempotent M1 bring-up for the percolator-prog wrapper against a local
# solana-test-validator. Resets the ledger each run so it's repeatable.
#
# Outputs:
#   - validator log:           /tmp/solana-test-validator.log
#   - bringup binary log:      stdout
#   - last transaction sigs:   printed by the bringup binary
#
# Prereqs the script ensures:
#   - Solana CLI 2.0.x + Rust toolchain on PATH (via scripts/env.sh)
#   - solana-test-validator running fresh on :8899
#   - Default keypair funded
#   - Program SBF .so present (rebuilds if missing)
#   - Program deployed under its build-time vanity keypair id
#
# Usage:
#   ./scripts/m1_bringup.sh            # full reset + bringup
#   KEEP_VALIDATOR=1 ./scripts/m1_bringup.sh  # skip kill/reset, reuse running validator

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck disable=SC1091
source "$SCRIPT_DIR/env.sh"

# Layout (env.sh may override): wrapper checkout and bring-up client crate.
PROG_DIR="${PERCOLATOR_PROG_DIR:-$REPO_ROOT/repo-prog}"
BRINGUP_DIR="${BRINGUP_DIR:-$REPO_ROOT/client}"
PROG_KEYPAIR="$PROG_DIR/target/deploy/percolator_prog-keypair.json"
PROG_SO="$PROG_DIR/target/deploy/percolator_prog.so"

if [ ! -f "$PROG_SO" ] || [ "${FORCE_SBF_REBUILD:-0}" = "1" ]; then
  echo "==> building percolator_prog.so (cargo build-sbf --tools-version v1.52)"
  # cargo-build-sbf 2.0.x unwraps read_dir(~/.cache/solana) before it would
  # download platform-tools, so a fresh machine (or CI runner) needs the dir.
  mkdir -p "$HOME/.cache/solana"
  ( cd "$PROG_DIR" && cargo build-sbf --tools-version v1.52 )
fi

PROG_ID="$(solana-keygen pubkey "$PROG_KEYPAIR")"
# Every cluster command names the local validator explicitly: never rely on
# the global Solana config, which may point at devnet or mainnet.
LOCAL_RPC="http://127.0.0.1:8899"
export PERCOLATOR_PROGRAM_ID="$PROG_ID"
echo "program id: $PROG_ID"

if [ "${KEEP_VALIDATOR:-0}" != "1" ]; then
  if [ -f /tmp/solana-test-validator.pid ]; then
    OLD_PID="$(cat /tmp/solana-test-validator.pid)"
    if kill -0 "$OLD_PID" 2>/dev/null; then
      echo "==> killing prior validator pid=$OLD_PID"
      kill "$OLD_PID" 2>/dev/null || true
      sleep 2
    fi
  fi
  echo "==> starting solana-test-validator"
  rm -rf "$REPO_ROOT/.solana-test-ledger"
  solana-test-validator \
    --reset --quiet \
    --ledger "$REPO_ROOT/.solana-test-ledger" \
    --rpc-port 8899 --bind-address 127.0.0.1 \
    > /tmp/solana-test-validator.log 2>&1 &
  echo $! > /tmp/solana-test-validator.pid
  for i in $(seq 1 30); do
    if solana cluster-version --url "$LOCAL_RPC" >/dev/null 2>&1; then
      echo "==> validator up (slot=$(solana epoch-info --url "$LOCAL_RPC" | awk '/^Slot:/ {print $2}'))"
      break
    fi
    sleep 1
  done
fi

if [ ! -f "$HOME/.config/solana/id.json" ]; then
  solana-keygen new --no-bip39-passphrase --silent --outfile "$HOME/.config/solana/id.json"
fi

if ! solana cluster-version --url "$LOCAL_RPC" >/dev/null 2>&1; then
  echo "no local validator on $LOCAL_RPC" >&2
  exit 2
fi
PAYER_BAL_LAMPORTS="$(solana balance --url "$LOCAL_RPC" --lamports | awk '{print $1}')"
if [ "$PAYER_BAL_LAMPORTS" -lt 100000000000 ]; then
  echo "==> airdrop payer"
  solana airdrop --url "$LOCAL_RPC" 100 >/dev/null
fi

# Deploy (re-deploys cheap if program already at this id)
if ! solana program show --url "$LOCAL_RPC" "$PROG_ID" >/dev/null 2>&1; then
  echo "==> deploying program $PROG_ID"
  solana program deploy --url "$LOCAL_RPC" "$PROG_SO" --program-id "$PROG_KEYPAIR" >/dev/null
fi

echo "==> running client/m1_bringup"
( cd "$BRINGUP_DIR" && cargo run --release --quiet )

echo "==> M1 bringup complete"
