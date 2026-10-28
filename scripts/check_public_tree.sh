#!/usr/bin/env bash
# Leak helper for the exported public tree: a plain pattern check.
#
#   ./scripts/check_public_tree.sh [--draft] [--allow-email ADDR] <dir> [denylist-file]
#
# This is an advisory helper, not a security boundary. What leaves the
# private repo is decided by the allowlist export (only manifest files are
# copied) and by the owner reading the exported tree before the first push.
# This script catches the obvious mistakes:
#   - private-key shapes: a 64-number JSON/Rust array, a PEM private-key
#     header, keypair / id.json / .env file names;
#   - home-directory paths;
#   - email addresses (except --allow-email ADDR, the security contact, and
#     the reserved documentation domains example.*, .invalid, .test, .localhost);
#   - every fixed string in the denylist (owner handles, the private repo
#     name and URL, private doc names), in contents and file names;
#   - unreplaced template placeholders (at-sign NAME at-sign), unless --draft.
# Text files only: grep -I skips binaries. Top-level target/, upstream/ and
# .git/ are skipped. Exits 1 on any hit, 2 on bad usage.
set -euo pipefail

draft=0
allow_email=""
args=()
while [ $# -gt 0 ]; do
  case "$1" in
    --draft) draft=1 ;;
    --allow-email) allow_email="${2:-}"; shift ;;
    *) args+=("$1") ;;
  esac
  shift
done
if [ "${#args[@]}" -lt 1 ] || [ ! -d "${args[0]}" ]; then
  echo "usage: check_public_tree.sh [--draft] [--allow-email ADDR] <dir> [denylist]" >&2
  exit 2
fi
root="${args[0]}"
deny="${args[1]:-}"
if [ -n "$deny" ] && [ ! -f "$deny" ]; then
  echo "denylist not found: $deny" >&2
  exit 2
fi
if [ -n "$deny" ]; then
  deny="$(cd "$(dirname "$deny")" && pwd)/$(basename "$deny")"  # absolute: we cd below
fi

echo "== check_public_tree: $root"
cd "$root"
hits=0
report() { # label, then matching lines on stdin
  local label="$1" line
  while IFS= read -r line; do
    echo "  $label: $line"
    hits=$((hits + 1))
  done
}
scan() { # label, grep flags and pattern...
  local label="$1"
  shift
  report "$label" < <(grep -rnI --exclude-dir=.git "$@" . | grep -Ev '^\./(target|upstream)/' || true)
}

files() { find . -path ./.git -prune -o -path ./target -prune -o -path ./upstream -prune -o -type f -print; }

report "secret-looking file" < <(files | grep -Ei '/(\.env[^/]*|id\.json[^/]*|[^/]*keypair[^/]*\.json|[^/]*\.pem)$' || true)
scan "64-number key array" -E '\[[[:space:]]*([0-9]{1,3}[[:space:]]*,[[:space:]]*){63}[0-9]{1,3}[[:space:]]*,?[[:space:]]*\]'
scan "private key header" -E 'BEGIN [A-Z ]*PRIVATE KEY'
scan "home path" -E '/(Users|home)/[^/[:space:]]'
email_hits() {
  grep -rnIoE --exclude-dir=.git '[A-Za-z0-9._%+-]+@[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,}' . \
    | grep -Ev '^\./(target|upstream)/' \
    | grep -Ev '@([A-Za-z0-9-]+\.)*(example(\.(com|org|net))?|invalid|test|localhost)$' || true
}
# grep -o lines are ./file:line:address; the approved contact may appear in SECURITY.md only.
report "email" < <(email_hits | awk -F: -v ok="$allow_email" '!(ok != "" && $1 == "./SECURITY.md" && $3 == ok)')
if [ "$draft" = 0 ]; then
  scan "unreplaced placeholder" -E '@[A-Z][A-Z_]+@'
fi
if [ -n "$deny" ]; then
  patterns="$(sed 's/#.*//; s/^[[:space:]]*//; s/[[:space:]]*$//; /^$/d' "$deny")" \
    || { echo "cannot read denylist: $deny" >&2; exit 2; }
  if [ -n "$patterns" ]; then
    scan "denylisted" -iF -e "$patterns"
    report "denylisted file name" < <(files | grep -iF -e "$patterns" || true)
  fi
else
  echo "  (no denylist given: generic checks only)"
fi

if [ "$hits" -gt 0 ]; then
  echo "== FAIL ($hits hits)"
  exit 1
fi
echo "== OK"
