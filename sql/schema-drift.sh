#!/usr/bin/env bash
# Schema drift check (DESIGN.md §12, §13): the extension SQL that pgrx
# generates must match the committed sql/pg_warren--<version>.sql, so that
# every schema change is deliberate and reviewed.
#
#   sql/schema-drift.sh            # check (exit 1 and print a diff on drift)
#   sql/schema-drift.sh --update   # regenerate the committed file
#
# Needs cargo-pgrx initialised for $PG (default pg16). pgrx orders entities
# unstably and annotates them, so both sides are normalised before
# comparing: entity blocks are sorted, pgrx's `-- crates/...` and
# `-- pg_warren::...` source lines are dropped, and `/* ... */` spans (pgrx's
# block markers, header and Rust-type annotations) are removed. Assumption:
# the extension's own SQL never puts `/*` inside a string literal; other
# `--` comments are kept and compared.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
pg="${PG:-pg16}"
version="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
committed="$root/sql/pg_warren--$version.sql"
generated="$(mktemp)"
trap 'rm -f "$generated"' EXIT

(cd "$root/crates/pg_warren" && cargo pgrx schema "$pg" -o "$generated" >/dev/null)

if [[ "${1:-}" == "--update" ]]; then
    cp "$generated" "$committed"
    echo "wrote $committed"
    exit 0
fi

normalise() {
    python3 -I -c '
import re, sys
text = open(sys.argv[1]).read()
blocks = []
for block in text.split("/* <begin connected objects> */"):
    lines = [line for line in block.splitlines()
             if not re.match(r"\s*-- (crates/|pg_warren::)", line)]
    body = re.sub(r"/\*.*?\*/", "", "\n".join(lines), flags=re.S)
    body = "\n".join(line.rstrip() for line in body.splitlines() if line.strip())
    if body:
        blocks.append(body)
print("\n\n".join(sorted(blocks)))
' "$1"
}

if [[ ! -f "$committed" ]]; then
    echo "missing $committed; run sql/schema-drift.sh --update" >&2
    exit 1
fi
if ! diff -u <(normalise "$committed") <(normalise "$generated"); then
    echo "schema drift: the generated SQL differs from $committed." >&2
    echo "If the change is intended, run sql/schema-drift.sh --update and commit it." >&2
    exit 1
fi
echo "no schema drift against $committed"
