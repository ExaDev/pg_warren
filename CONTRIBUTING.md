# Contributing to pg_warren

pg_warren is a Rust workspace with two crates (DESIGN.md §11):

| Crate | What it is | Postgres needed to test? |
|---|---|---|
| `crates/warren-core` | The domain: CSR, traversal, compute kernels. No Postgres dependency. | No |
| `crates/pg_warren` | The pgrx extension: the Postgres adapter around `warren-core`. | Yes |

## Prerequisites

- Rust 1.97.0, pinned exactly in `rust-toolchain.toml`. Run `rustup toolchain install`
  in the repo to get it. For Miri you also need the dated nightly that CI pins
  (`MIRI_NIGHTLY` in `.github/workflows/ci.yml`):
  `rustup toolchain install nightly-2026-10-15 --component miri,rust-src`.
- PostgreSQL 15–18 with server dev headers (`postgresql-server-dev-NN` on
  Debian/Ubuntu), plus `clang` / `libclang-dev` for pgrx's bindgen.
- cargo-pgrx, at exactly the version the extension uses:

  ```sh
  cargo install cargo-pgrx --version 0.19.3 --locked
  cargo pgrx init --pg16 "$(command -v pg_config)"   # or /usr/lib/postgresql/16/bin/pg_config
  ```

  You can init several majors at once (`--pg15 ... --pg17 ...`).

## Running the checks

These are the same checks CI runs (`.github/workflows/ci.yml`). Run them before
you push.

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy -p pg_warren --all-targets --features pg_test -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

# Tier 1: domain unit + property tests (no Postgres); --all-targets also
# compiles the criterion benches and runs each once
cargo test -p warren-core --all-targets

# Tier 2: in-database tests (#[pg_test]) against a real Postgres
cd crates/pg_warren && cargo pgrx test pg16

# Domain tests under Miri (nightly), for unsafe slice code (DESIGN.md §11)
MIRIFLAGS=-Zmiri-disable-isolation PROPTEST_CASES=16 PROPTEST_DISABLE_FAILURE_PERSISTENCE=true cargo +nightly-2026-10-15 miri test -p warren-core

# Segment decoder fuzzing (nightly + `cargo install cargo-fuzz`); fuzz/ is outside the workspace
(cd fuzz && cargo +nightly-2026-10-15 fuzz run decode_csr -- -max_total_time=30)   # also decode_id_map, decode_tombstones, decode_relation_dictionary

# Optional: run the micro-benchmarks (CI only builds and smoke-runs them)
cargo bench -p warren-core

# The generated extension SQL must match the committed snapshot (needs
# cargo pgrx init for pg16). After a deliberate schema change, regenerate it
# with sql/schema-drift.sh --update and commit the result.
sql/schema-drift.sh

# warren-core must never depend on Postgres (DESIGN.md §15.4); this must print nothing
cargo tree -p warren-core -e all | grep -E 'pgrx|pg[-_]sys'
```

`cargo test --workspace` is **not** how to test `pg_warren`: its tests need the
pgrx harness, so always use `cargo pgrx test pgNN` for that crate.

`clippy` and `doc` on `pg_warren` need to find a `pg_config`, either via
`cargo pgrx init` or by setting `PGRX_PG_CONFIG_PATH=/path/to/pg_config`
(CI's lint job uses the latter and skips installing cargo-pgrx).

### Running pgrx tests as root (containers, cloud dev boxes)

`cargo pgrx test` runs `initdb` and `postgres`, and both refuse to run as root
(`initdb: error: cannot be run as root`). Use pgrx's built-in `--runas`, which
runs the test cluster as an unprivileged user via `sudo`, with a data
directory that user owns:

```sh
install -d -o postgres -g postgres /tmp/pgrx-pgdata
cd crates/pg_warren
cargo pgrx test pg16 --runas postgres --pgdata /tmp/pgrx-pgdata
```

`cargo pgrx test` installs the extension into the shared Postgres tree
(`$(pg_config --pkglibdir)/pg_warren.so` and the extension SQL), so two
checkouts or worktrees testing at once overwrite each other's build and get
confusing failures. When several run on one machine, take a global lock:

```sh
flock /tmp/pgrx-test.lock cargo pgrx test pg16 --runas postgres --pgdata /tmp/pgrx-pgdata
```

This needs `sudo` and an existing unprivileged user (the `postgres` user that
the Debian/Ubuntu packages create works). As root, `cargo pgrx init` against a
system `pg_config` prints "Skipping initdb as current user is root user"; that
is expected and harmless.

When **not** root but using a distro-packaged Postgres, `cargo pgrx test`
installs the extension into the system tree, so make it writable once:

```sh
sudo chmod a+rwx "$(pg_config --pkglibdir)" "$(pg_config --sharedir)/extension"
```

### Lints

The lint policy lives in `[workspace.lints]` in the root `Cargo.toml`, and every
crate opts in with `[lints] workspace = true`. It denies `unwrap`, `expect`,
`panic!`, `todo!`, `unimplemented!` and `dbg!`, unsafe blocks without a
`// SAFETY:` comment, and unsafe operations inside `unsafe fn` without their
own `unsafe` block. `clippy.toml` allows `unwrap`, `expect` and `panic!` in
`#[test]` functions and `#[cfg(test)]` code. Clippy does not treat `#[pg_test]`
functions as tests, so they return `Result<(), pgrx::spi::Error>` and use `?`.

## Other PostgreSQL majors

Feature flags `pg15`, `pg16` (default), `pg17`, `pg18` select the major.
`cargo pgrx test pgNN` switches features for you. CI runs the pgrx tests on all
four majors; locally, one is usually enough.

## Integration tests, fixtures and benchmarks (`test/`, `bench/`)

The out-of-crate harness is Python 3 (DESIGN.md §12). Set it up once, in a
virtualenv if you like:

```sh
pip install -r bench/requirements.txt      # numpy, networkx, scipy, pytest (pinned)
```

```sh
# Generator, harness and oracle unit tests (no Postgres needed)
pytest

# SQL tests in test/sql/: installs pg_warren (cargo pgrx install, debug) and
# runs each file in a fresh database of a scratch, socket-only cluster.
# Holds /tmp/pgrx-test.lock for the whole run. As root, the cluster runs as
# --runas (default postgres) via sudo.
test/run.py --pg-config "$(command -v pg_config)"
test/run.py --no-install test/sql/staged_build_rollback.sql   # one test, current install

# A fixture on its own (Appendix A scales by default; see --help per kind)
bench/generate.py sbm --communities 4 --community-size 50 --out /tmp/sbm
(cd /tmp/sbm && psql -v ON_ERROR_STOP=1 -f load.sql)

# Benchmarks: every graph in bench/matrix.json (installs a release build);
# results in bench/results/<pg_warren version>/<graph>.json
bench/run.sh
bench/run.sh smoke --no-install --results bench/tmp   # quick check, as CI does
```

To add a SQL test, write `test/sql/<name>.sql` with `assert.*` calls and
optional `-- fixture:` / `-- oracle:` directives; `test/README.md` has the
format. To benchmark a new function, add a workload with `requires` to
`bench/matrix.json`.

## Releases and upgrades

- The workspace version in `Cargo.toml` is the version **in development**;
  `sql/pg_warren--<version>.sql` is its install script, regenerated with
  `sql/schema-drift.sh --update`.
- **Released install scripts are immutable.** Each released
  `sql/pg_warren--<version>.sql` is listed in `sql/released.sha256`;
  `sql/schema-drift.sh` fails if one changes, or if the version in
  development is a released one.
- **Every schema change after a release updates the upgrade script in the
  same commit**: `sql/pg_warren--<released>--<version>.sql`, hand-written
  (new or changed functions, tables and columns with a backfill, storage,
  grants). `test/upgrade.py` (in CI, on every major) installs each
  release, writes some state with it (several versions, a staged one, a
  vector), runs `ALTER EXTENSION pg_warren UPDATE`, fails on any catalog
  difference from a fresh install, and checks the backfills and a
  retention pass on the upgraded catalog. `crates/pg_warren/sql` links to `sql/`, so
  `cargo pgrx install` installs the upgrade scripts.
- Users upgrade by installing the new build and then running
  `ALTER EXTENSION pg_warren UPDATE;` in every database that has the
  extension, **before using the new version**: the new `pg_warren.so` is
  loaded at once, but the catalog (new columns, functions, storage) only
  changes with the update, so calls in between can fail. Release notes
  (CHANGELOG.md) say so for every release with a schema change.
- To release: finalise CHANGELOG.md (and README's Status) in a commit
  that is otherwise the version in development; that commit gets the tag
  `vX.Y.Z`. Then, in one commit: add the version's line to
  `sql/released.sha256` (`cd sql && sha256sum pg_warren--X.Y.Z.sql >>
  released.sha256`), bump the workspace version, regenerate its install
  script (`sql/schema-drift.sh --update`) and add the header-only upgrade
  script `sql/pg_warren--X.Y.Z--<next>.sql`. In the other order the tagged
  commit fails `schema-drift.sh` (its version would already be released).

```sh
test/upgrade.py --pg-config "$(command -v pg_config)"   # installs, then checks every release's path
```

## Docker

`docker/Dockerfile` builds the extension against one major and installs it into
the official `postgres` image for that major:

```sh
docker build -f docker/Dockerfile --build-arg PG_MAJOR=17 -t pg_warren:pg17 .
```

## Engineering conventions

Test-first (red, green, refactor), package-by-feature, and ports and adapters,
with `warren-core` as the domain. The full conventions are in
[CLAUDE.md](CLAUDE.md); they apply to human contributors too. Keep
[DESIGN.md](DESIGN.md) current when a design decision changes (§15.5).

## License

By contributing you agree that your contributions are licensed under the
Apache License 2.0 ([LICENSE](LICENSE)).
