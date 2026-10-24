#!/usr/bin/env bash
# Builds postgresql-NN-pg-warren for each of $PG_MAJORS inside a clean
# container of the target distribution (release/packages.py deb). Mounts:
# /source (the release's checkout, read-only), /packaging (this directory),
# /meta (the generated debian/changelog) and /out (the .deb files).
# Env: PG_MAJORS ("15 16 17 18"), PGRX_VERSION (the source's pinned pgrx).
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq --no-install-recommends \
  build-essential ca-certificates clang curl debhelper fakeroot git gnupg libclang-dev \
  pkgconf postgresql-common >/dev/null
# PGDG (apt.postgresql.org), as its packages are built: the server headers
# of each major, and pg_buildext.
/usr/share/postgresql-common/pgdg/apt.postgresql.org.sh -y >/dev/null
majors=()
for major in $PG_MAJORS; do majors+=("postgresql-server-dev-$major"); done
apt-get install -y -qq --no-install-recommends postgresql-common-dev "${majors[@]}" >/dev/null

export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain none

build=/build/pg-warren
mkdir -p "$build"
cp -a /source/. "$build"
rm -rf "$build/debian" "$build/target"
cp -a /packaging/debian "$build/debian"
cp /meta/changelog "$build/debian/changelog"
cd "$build"
rustup toolchain install   # rust-toolchain.toml
cargo install cargo-pgrx --version "$PGRX_VERSION" --locked --quiet

export PG_SUPPORTED_VERSIONS="${PG_MAJORS// /,}"
pg_buildext updatecontrol
# -d: rustc and cargo-pgrx come from rustup, not Build-Depends.
dpkg-buildpackage -b -us -uc -d
cp ../*.deb /out/
ls -l /out
