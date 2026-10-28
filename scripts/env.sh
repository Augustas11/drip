# Source this for any session that needs cargo + the Solana toolchain.
export PATH="$HOME/.local/share/solana/install/active_release/bin:$HOME/.cargo/bin:$PATH"

# Repository layout used by the scripts and CI (public tree layout).
_ENV_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export PERCOLATOR_PROG_DIR="${PERCOLATOR_PROG_DIR:-$_ENV_ROOT/upstream/percolator-prog}"
export BRINGUP_DIR="${BRINGUP_DIR:-$_ENV_ROOT/crates/bringup}"
export CRANKER_MANIFEST="${CRANKER_MANIFEST:-$_ENV_ROOT/crates/cranker/Cargo.toml}"
unset _ENV_ROOT
