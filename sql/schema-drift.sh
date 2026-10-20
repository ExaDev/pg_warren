#!/usr/bin/env bash
# Schema drift check (DESIGN.md §12, §13): the extension SQL that pgrx
# generates must match the committed sql/pg_warren--<version>.sql of the
# version in Cargo.toml (the one in development), so that every schema
# change is deliberate and reviewed. Released install scripts are frozen:
# each is listed in sql/released.sha256 and must match it byte for byte,
# the version in development must not be one of them, every released
# version must have a chain of upgrade scripts to it, and a commit recorded
# in sql/released.commits whose install script is the released one
# (CONTRIBUTING.md, "Releases and upgrades"). test/upgrade.py checks what
# the scripts do.
#
#   sql/schema-drift.sh            # check (exit 1 and print a diff on drift)
#   sql/schema-drift.sh --update   # regenerate the current version's file
#                                  # (never a released one)
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

released() {
    (cd "$root/sql" && sha256sum --check --quiet released.sha256) || {
        echo "a released install script changed: released files are immutable (sql/released.sha256)" >&2
        exit 1
    }
    if grep -q " pg_warren--$version\.sql\$" "$root/sql/released.sha256"; then
        echo "version $version is released; bump the workspace version before changing the schema" >&2
        exit 1
    fi
    python3 -I - "$root/sql" "$version" <<'PY'
import hashlib, re, subprocess, sys
from pathlib import Path
sql, current = Path(sys.argv[1]), sys.argv[2]
commits = {}
if (sql / "released.commits").exists():
    for line in (sql / "released.commits").read_text().splitlines():
        if line.strip():
            sha, released = line.split()
            commits[released] = sha
edges = {}
for path in sql.glob("pg_warren--*--*.sql"):
    start, end = re.fullmatch(r"pg_warren--(.+)--(.+)\.sql", path.name).groups()
    edges.setdefault(start, []).append(end)
for line in (sql / "released.sha256").read_text().splitlines():
    version = re.search(r"pg_warren--(.+)\.sql$", line).group(1)
    seen, todo = set(), [version]
    while todo:
        at = todo.pop()
        seen.add(at)
        todo += [n for n in edges.get(at, []) if n not in seen]
    if current not in seen:
        sys.exit(f"no upgrade path from released {version} to {current}: add sql/pg_warren--{version}--{current}.sql")
    sha = commits.get(version)
    if sha is None:
        sys.exit(f"released {version} has no commit in sql/released.commits: record the release's commit")
    shown = subprocess.run(["git", "show", f"{sha}:sql/pg_warren--{version}.sql"], cwd=sql, capture_output=True)
    frozen = line.split()[0]
    if shown.returncode != 0 or hashlib.sha256(shown.stdout).hexdigest() != frozen:
        sys.exit(f"released {version}'s recorded commit {sha} does not hold the released "
                 f"sql/pg_warren--{version}.sql (or is not in this clone: fetch the full history)")
PY
}
released

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
