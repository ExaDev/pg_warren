#!/usr/bin/env bash
# Runs COMMAND with the RUSTFLAGS and CFLAGS of a reproducible build of the
# source at SOURCE (CONTRIBUTING.md, "Reproducible builds"):
#
#   packaging/reproducibly.sh SOURCE COMMAND [ARGS...]
#
# rustc writes source paths into the library (panic locations), and so does
# the C compiler, through __FILE__, in the C shim pgrx-pg-sys builds in its
# OUT_DIR. These map the build's own paths to fixed ones, so where the
# source, its target directory and CARGO_HOME are does not change a byte:
#   SOURCE (and its target directory)  -> /pg_warren
#   SOURCE/vendor (the vendored tarball) -> /crates
#   CARGO_HOME's crates.io sources     -> /crates
# Vendored and downloaded crates both sit in NAME-VERSION directories
# (`cargo vendor --versioned-dirs`, release/source.py), so a build from the
# vendored tarball and one from crates.io see the same paths. rustc and gcc
# apply the last matching map, so vendor/ follows SOURCE. Flags already in
# RUSTFLAGS and CFLAGS are kept, after these. CARGO_HOME's sources are
# those present now: run `cargo fetch --locked` first.
set -euo pipefail

if [[ $# -lt 2 || ! -d "$1" ]]; then
  echo "usage: $0 SOURCE_DIRECTORY COMMAND [ARGS...]" >&2
  exit 2
fi
root="$(cd "$1" && pwd -P)"
shift
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
maps=("$root=/pg_warren" "$root/vendor=/crates")
for index in "$cargo_home"/registry/src/*/; do
  if [[ -d "$index" ]]; then maps+=("${index%/}=/crates"); fi
done
rustflags=() cflags=()
for map in "${maps[@]}"; do
  rustflags+=("--remap-path-prefix=$map")
  cflags+=("-ffile-prefix-map=$map")
done
RUSTFLAGS="${rustflags[*]}${RUSTFLAGS:+ $RUSTFLAGS}"
CFLAGS="${cflags[*]}${CFLAGS:+ $CFLAGS}"
export RUSTFLAGS CFLAGS
exec "$@"
