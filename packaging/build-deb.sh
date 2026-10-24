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

# rustup-init 1.29.2, checked against its sha256 (static.rust-lang.org's
# .sha256 beside each); rustup then checks each toolchain download.
export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
case "$(uname -m)" in
  x86_64) rustup_sha=5be191e8c70a5007074254cb74d78dbfdd25cd5a61c27de7b8e5410122ec24f4 ;;
  aarch64) rustup_sha=a9c168dbea81a84671c6b7a5c1795cb0eb699020110d107ddfca427bd797e697 ;;
  *) echo "no rustup-init pinned for $(uname -m)"; exit 1 ;;
esac
curl -fsSLo /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/1.29.2/$(uname -m)-unknown-linux-gnu/rustup-init"
echo "$rustup_sha  /tmp/rustup-init" | sha256sum --check --quiet
chmod +x /tmp/rustup-init
/tmp/rustup-init -y -q --profile minimal --default-toolchain none --no-modify-path

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
