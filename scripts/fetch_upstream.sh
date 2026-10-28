#!/usr/bin/env bash
# Check out the upstream wrapper (percolator-prog) at the pinned SHA into
# $PERCOLATOR_PROG_DIR (see scripts/env.sh). The crates depend on it by path;
# the engine is a git dependency pinned by rev in Cargo.toml. Read-only:
# clone/fetch + checkout. Refuses to continue if the wrapper's own engine
# pin disagrees with scripts/pins.env.
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/env.sh"
# shellcheck disable=SC1091
source "$SCRIPT_DIR/pins.env"
DEST="$PERCOLATOR_PROG_DIR"
if [ ! -d "$DEST/.git" ]; then
  mkdir -p "$(dirname "$DEST")"
  git clone -q "$PERCOLATOR_PROG_REPO" "$DEST"
fi
git -C "$DEST" fetch -q origin
git -C "$DEST" checkout -q --detach "$PERCOLATOR_PROG_SHA"
echo "percolator-prog @ $(git -C "$DEST" rev-parse --short HEAD) in $DEST"
got="$(grep -o 'rev = "[0-9a-f]*"' "$DEST/Cargo.toml" | head -1 | cut -d'"' -f2)"
if [ "$got" != "$PERCOLATOR_ENGINE_REV" ]; then
  echo "pin mismatch: wrapper pins engine $got, scripts/pins.env says $PERCOLATOR_ENGINE_REV" >&2
  exit 1
fi
for m in "$CRANKER_MANIFEST" "$BRINGUP_DIR/Cargo.toml"; do
  if ! grep -q "rev = \"$PERCOLATOR_ENGINE_REV\"" "$m"; then
    echo "pin mismatch: $m does not pin engine $PERCOLATOR_ENGINE_REV" >&2
    exit 1
  fi
done
echo "engine pin $PERCOLATOR_ENGINE_REV matches the wrapper and both crates"
