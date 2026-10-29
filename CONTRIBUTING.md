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

# Domain tests under Miri (nightly; CI runs it nightly on main and on release PRs, not on other PRs or pushes; DESIGN.md §11)
MIRIFLAGS=-Zmiri-disable-isolation PROPTEST_CASES=16 PROPTEST_DISABLE_FAILURE_PERSISTENCE=true cargo +nightly-2026-10-15 miri test -p warren-core

# Segment decoder fuzzing (nightly + `cargo install cargo-fuzz`); fuzz/ is outside the workspace
(cd fuzz && cargo +nightly-2026-10-15 fuzz run decode_csr -- -max_total_time=30)   # also decode_id_map, decode_text_id_map, decode_tombstones, decode_relation_dictionary, decode_membership

# Optional: run the micro-benchmarks (CI only builds and smoke-runs them)
cargo bench -p warren-core

# The generated extension SQL must match the committed snapshot (needs
# cargo pgrx init for pg16). After a deliberate schema change, regenerate it
# with sql/schema-drift.sh --update and commit the result.
sql/schema-drift.sh

# warren-core must never depend on Postgres (DESIGN.md §15.4); this must print nothing
cargo tree -p warren-core -e all | grep -E 'pgrx|pg[-_]sys'

# The workflows (.github/workflows), with the versions CI pins: actionlint 1.7.12 runs
# shellcheck 0.11.0 (https://github.com/koalaman/shellcheck/releases/tag/v0.11.0) on each run: script
go install github.com/rhysd/actionlint/cmd/actionlint@v1.7.12 && ~/go/bin/actionlint
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
pip install -r bench/requirements.txt      # numpy, networkx, scipy, igraph, pytest (pinned)
```

```sh
# Generator, harness, oracle and release-tooling (release/) unit tests
# (no Postgres needed; release/tests clones this repository's history)
pytest

# SQL tests in test/sql/: installs pg_warren (cargo pgrx install, debug) and
# runs each file in a fresh database of a scratch, socket-only cluster.
# Holds /tmp/pgrx-test.lock for the whole run. As root, the cluster runs as
# --runas (default postgres) via sudo.
test/run.py --pg-config "$(command -v pg_config)"
test/run.py --no-install test/sql/staged_build_rollback.sql   # one test, current install
# The same suite with a trusted install: each database's pg_warren is created
# by a non-superuser that owns the database (DESIGN.md §7). CI runs both.
test/run.py --no-install --trusted
# The shared cache (DESIGN.md §5.6): the default mode, `on`, is the default
# configuration (warren.shared_cache_memory = 4GB: shared on PG 17 and 18,
# private on PG 15 and 16, which need the preload); `preload` adds
# shared_preload_libraries = 'pg_warren' and a cap, so every major shares;
# `off` sets the cap to 0. CI runs the shared-cache tests in each mode on
# every major, and the whole suite preloaded on PG 16.
test/run.py --no-install --shared-cache preload
test/run.py --no-install --shared-cache off test/sql/shared_cache_*.sql

# A fixture on its own (Appendix A scales by default; see --help per kind)
bench/generate.py sbm --communities 4 --community-size 50 --out /tmp/sbm
(cd /tmp/sbm && psql -v ON_ERROR_STOP=1 -f load.sql)

# Benchmarks: every graph in bench/matrix.json (installs a release build);
# results in bench/results/<pg_warren version>/<graph>.json
bench/run.sh
bench/run.sh smoke --no-install --results bench/tmp   # quick check, as CI does

# The shared cache under a pooler (needs pgbouncer and pgbench on PATH, and a
# release build installed in --pg-config's tree): results in bench/results/pooler/
bench/pooler.py a --scale 1000 --connections 1 4 --seconds 3 \
    --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release

# Graph reads per row in parallel plans, labels RESTRICTED against SAFE,
# preloaded and not (release build in --pg-config's tree): results in
# bench/results/parallel/
bench/parallel.py --scale 1000 --rows 10000 --repeat 1 \
    --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release
# The real-graph suite (bench/corpus/README.md): keepset datasets, fetched
# with the pinned keepset CLI (needs Node 22 and network) into
# /var/tmp/warren-corpus, checked against NetworkX and igraph (--oracle picks
# one) and timed (release build in --pg-config's tree); results in
# bench/results/corpus/. CI checks the tiny tier nightly (corpus.yml).
bench/corpus/suite.py fetch --tier tiny
bench/corpus/suite.py check --tier tiny --pg-config $T/usr/lib/postgresql/16/bin/pg_config \
    --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release
bench/corpus/suite.py time --tier medium --pg-config ... --extension-revision ... --extension-profile release
bench/corpus/suite.py summary
```

`bench/pooler.py`, `bench/parallel.py`, `bench/soak.py` and `bench/corpus/suite.py` start their own scratch cluster from
`--pg-config`'s binaries and load the `pg_warren` installed there. To
benchmark a release build without replacing the debug build that `cargo pgrx
test` and `test/run.py` install into the system tree (and that other
checkouts may be using), copy the server's tree somewhere the cluster user
can read, and install the build there: Postgres finds its libraries and
extensions relative to its binaries.

```sh
T=/var/tmp/pgtree; mkdir -p $T/usr/lib/postgresql/16 $T/usr/share/postgresql
cp -a /usr/lib/postgresql/16/bin /usr/lib/postgresql/16/lib $T/usr/lib/postgresql/16/
cp -a /usr/share/postgresql/16 $T/usr/share/postgresql/
(cd crates/pg_warren && cargo pgrx package --pg-config "$(command -v pg_config)")
cp -r target/release/pg_warren-pg16/usr/* $T/usr/ && chmod -R a+rX $T
bench/pooler.py … --pg-config $T/usr/lib/postgresql/16/bin/pg_config
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
  development is a released one. `sql/released.commits` records each
  release's commit (`<sha>  <version>`); `schema-drift.sh` fails if a
  release has none, or if that commit's install script is not the released
  one.
- **Every schema change after a release updates the upgrade script in the
  same commit**: `sql/pg_warren--<released>--<version>.sql`, hand-written
  (new or changed functions, tables and columns with a backfill, storage,
  grants). `test/upgrade.py` (in CI, on every major) builds and installs
  each release from its commit in `sql/released.commits` (so the clone
  needs the full history; a tag `vX.Y.Z`, where present, must name the same
  commit; it builds with the cargo-pgrx of the pgrx the release's
  `Cargo.lock` pins, installed under the builds if it is not the one on
  `PATH`, and the toolchain its `rust-toolchain.toml` pins), checks the
  library a backend then loads is that release's, writes some state with that release's own library
  and functions (several versions, a staged one, a vector, a weighted
  multi-relation graph and its results), puts the version in development
  back, runs `ALTER EXTENSION pg_warren UPDATE`, fails on any catalog
  difference from a fresh install, and checks the backfills, that the
  release's state reads as it did, and a retention pass on the upgraded
  catalog. `--release-builds DIR` keeps the release builds (one target
  directory per release and major; CI caches it). `crates/pg_warren/sql` links to `sql/`, so
  `cargo pgrx install` installs the upgrade scripts.
- Users upgrade by installing the new build and then running
  `ALTER EXTENSION pg_warren UPDATE;` in every database that has the
  extension, **before using the new version**: the new `pg_warren.so` is
  loaded at once, but the catalog (new columns, functions, storage) only
  changes with the update, so calls in between can fail. Release notes
  (CHANGELOG.md) say so for every release with a schema change.
- A release is two commits, made by `release/prepare.py X.Y.Z NEXT`
  (see its docstring): **the commit to tag** `vX.Y.Z`, `release: X.Y.Z`,
  which dates CHANGELOG's section in development (`--title` names it) and
  points README's install at `vX.Y.Z` after `sql/schema-drift.sh` passes;
  then **the freeze commit**, `release: freeze X.Y.Z, develop NEXT`, which
  adds the install script's sha256 to `sql/released.sha256`, records the
  commit to tag in `sql/released.commits`, bumps the workspace version
  (`Cargo.toml`, `Cargo.lock`), generates NEXT's install script
  (`sql/schema-drift.sh --update`), adds the header-only
  `sql/pg_warren--X.Y.Z--NEXT.sql` and opens CHANGELOG's NEXT section. In
  the other order the tagged commit would fail `schema-drift.sh` (its
  version would already be released). `test/upgrade.py` reads the releases
  from those two files, so it needs no edit. The script refuses a dirty
  tree, a released or tagged version and one that is not in development;
  run again, it changes nothing, and after a failed freeze it resumes.
- **To release** (DESIGN.md §13, M11 "Distribution"):
  1. Merge what a person writes first, in an ordinary PR: CHANGELOG's
     section as release notes, README's Status (the release as released,
     NEXT as in development) and DESIGN §13's done list. The release
     commits change none of it.
  2. Run the **Release (prepare)** workflow from main (Actions → Release
     (prepare) → Run workflow) with the version, and optionally the next
     version (default `X.(Y+1).0`) and the release's name. Its first job
     runs `release/prepare.py` on `release-X.Y.Z` with a read-only token
     and hands the two commits on as a git bundle; its second job, which
     runs no repository code, checks the bundle is exactly those two
     commits on the dispatched main commit, touching only the files a
     release touches and only as `release/prepare.py` does (CHANGELOG's
     headings, README's install line, the workspace packages' version
     lines, the frozen script's sha256, the commit to tag, the upgrade
     script's header byte for byte), pushes the branch and dispatches CI
     on it, Miri and
     `test/upgrade.py` included (a dispatch made with `GITHUB_TOKEN` starts
     a run, and its checks show on the PR).
  3. Open the release PR yourself from the compare link in the run's
     summary: title `release: X.Y.Z`, body CHANGELOG's section (also in
     the summary). The workflow does not open it: a token that may open
     PRs could also approve them. Review the PR and its CI. The review
     must cover `sql/pg_warren--NEXT.sql`, which pgrx generates and no
     check can compare line by line (it should hold the same entities as
     `sql/pg_warren--X.Y.Z.sql`, in pgrx's order; CI's drift check
     regenerates it), and `Cargo.toml` and `Cargo.lock`. To change it,
     close it, delete the branch, fix main and run the workflow again: a
     commit pushed on top would not be in the tagged commit.
  4. **Merge it with a merge commit**, never a squash or a rebase: the
     recorded commit to tag must be in main's history.
  5. The merge changes `sql/released.commits`, which starts **Release
     (publish)**. Its first job verifies every frozen install script and
     that no version or commit is recorded twice, and shows
     `vX.Y.Z → <commit>` and the notes in its summary; the publish job,
     named after both, waits for a reviewer of the `release` environment.
     Check that the run was started by the release PR's merge commit and
     that the commit shown is the PR's `release: X.Y.Z` commit, and
     approve. Both jobs read the commit that started the run (`github.sha`),
     so what is shown is what is tagged. Before the gate, read-only jobs
     build and test the packages (below); the publish job waits for them. It
     then pushes the annotated tag `vX.Y.Z` with the environment's deploy
     key, creates the GitHub Release with CHANGELOG's section as its
     notes, and publishes the packages. A re-run succeeds if the tag is already on the recorded commit
     and fails if it is on another; run it by hand (with a version, or
     none for the latest) to publish again. A release already published
     (its GitHub Release has `SHA256SUMS` and `SHA256SUMS.asc`) is not
     built again, since a rebuild's bytes would differ from the attached
     packages, bottle and images: the run takes the `.deb` files its
     signed `SHA256SUMS` lists, checked, and only completes what is
     missing, such as its apt snapshot.
- **Manual fallback**, without the workflows: on a branch from main,
  `release/prepare.py X.Y.Z NEXT --title "..."` (it needs cargo-pgrx
  initialised for `$PG`, default pg16), push it and open the PR, run
  `test/upgrade.py` and Miri (`cargo +$MIRI_NIGHTLY miri test -p
  warren-core`) on it, and merge with a merge commit. Release (publish)
  still runs on that merge; if it cannot, an admin, on an up-to-date
  main: `release/publish.py verify && release/publish.py tag X.Y.Z &&
  git push origin vX.Y.Z`, then `release/changelog.py notes X.Y.Z` as the
  Release's notes (`gh release create vX.Y.Z --verify-tag --notes-file`).
  Only the deploy key and admins can push a `v*` tag (below).
- **Residual risk**: the tag ruleset protects tags, not GitHub Releases.
  Anyone with write access can still edit or delete a Release's notes and
  assets, or create a draft Release, on an existing tag; the tag itself,
  and so the released source, cannot be moved. Check a Release against
  CHANGELOG.md and its tag if in doubt.
- pg_warren is a **trusted** extension, so a non-superuser's `CREATE` or
  `ALTER EXTENSION` runs every install and upgrade script as the bootstrap
  superuser. Scripts create objects only in schema `warren`, schema-qualify
  names, and use only exact-match `pg_catalog` operators and functions in
  anything evaluated while the script runs (defaults, constraints,
  backfills). The rules and the audit are in DESIGN.md §7.

```sh
test/upgrade.py --pg-config "$(command -v pg_config)"   # builds each release from its recorded commit, then checks its path
test/upgrade.py --no-install --trusted   # the same, installed and updated by a non-superuser
test/dump.py --pg-config "$(command -v pg_config)"   # pg_dump and restore of graphs (plain, custom, trusted, managed: no superuser)
# docs/beside-managed.md as written: a primary without pg_warren, a graph
# server subscribed to it (logical replication) and postgres_fdw both ways;
# graphs on the graph server answer as one built from the primary's rows
test/logical_replication.py --no-install --pg-config "$(command -v pg_config)"
```

Robustness under concurrency, crashes, replication and hostile input
(DESIGN.md §13, M11), each in a scratch cluster per shared-cache mode
(default `preload`, then `on`), and holding `/tmp/pgrx-test.lock` for the
whole run:

```sh
# 8 readers, 3 refreshers and a vector churner on one graph for --seconds
# (default 20 per mode); fails on a read that matches no complete version,
# an undocumented SQLSTATE, anything left behind, or trouble in the log
test/concurrent_refresh.py --pg-config "$(command -v pg_config)" --seconds 20
# Refreshes, pagerank and loads into the shared cache stopped mid-way by
# pg_terminate_backend, pg_cancel_backend, SIGKILL and an immediate
# shutdown (--kinds, --points to choose; --edges sizes the graph)
test/crash_recovery.py --no-install --pg-config "$(command -v pg_config)"
# A primary, a hot standby (pg_basebackup -R) and a cascading standby:
# reads answer on the standbys as on the primary, writes are refused with
# 25006, a version retired under a standby query is read consistently or
# the query cancelled (40001), and the standby, promoted, writes
test/replicas.py --no-install --pg-config "$(command -v pg_config)"
# SQL-surface fuzzing: every warren.* function with generated arguments,
# as four roles, --seconds per seed and mode (CI: 20 s; seeds 1 and 2 on
# pull requests, 1 and one from the clock on main and nightly); each
# failure is written to --out as a psql script, with its seed, the first
# --max-shrinks (default 3) shrunk first. A long run, e.g. before a release:
test/sql_fuzz.py --no-install --pg-config "$(command -v pg_config)" \
    --seeds 1 2 3 4 5 time --seconds 300 --shared-cache preload on off
# A soak behind PgBouncer (needs pgbouncer: /usr/sbin/pgbouncer or
# --pgbouncer), in transaction and session pooling at once, 48 clients over
# 8 server connections each: reads, compute, persisted and transient
# vectors, prepared statements, introspection, timeouts, cancels and
# clients that leave mid-transaction, with writes that retire versions.
# Fails on memory growth in the steady state, anything left behind, an
# undocumented SQLSTATE, session state that session pooling loses or
# leaks, or trouble in the log; samples.csv and summary.json in --out.
# CI runs 2 minutes on PG 16, preloaded:
test/pooler_soak.py --no-install --pg-config "$(command -v pg_config)"
```

What a managed service does to a cluster (DESIGN.md §13, 0.9.0
"Lifecycle"), the same way. Each builds the replica check's graphs and
compares every pg_warren catalog table's rows and every read before and
after, then refreshes and writes vectors:

```sh
# The preload added to a cluster with graphs and removed, a restart each
test/preload.py --no-install --pg-config "$(command -v pg_config)"
# pg_basebackup, then changes, each marked by a restore point; the backup
# restored and recovered to each restore point and to the archive's end
test/point_in_time.py --no-install --pg-config "$(command -v pg_config)"
# pg_upgrade from an older major, with the shared cache loaded, copying the
# files and then linking them (--transfer copy link, the default). It installs
# pg_warren into both majors' trees (both in pgrx's config.toml; unset
# PGRX_PG_CONFIG_PATH, or both builds use its pg_config)
test/major_upgrade.py --old-pg-config /usr/lib/postgresql/16/bin/pg_config \
    --pg-config /usr/lib/postgresql/18/bin/pg_config
```

A long fuzz run or soak need not hold the lock if it uses a Postgres tree
of its own (as for the benchmarks, above): install the build there and pass
`--no-lock` (which implies `--no-install`) with that tree's `pg_config`.
A long soak runs for `--hours` and should, run detached so that it
outlives the shell, with a release build if it is to say anything about
a release:

```sh
setsid nohup test/pooler_soak.py --no-lock --hours 48 \
    --pg-config $T/usr/lib/postgresql/16/bin/pg_config \
    --out /var/tmp/pooler-soak > /var/tmp/pooler-soak.log 2>&1 &
```

It prints a line a minute, writes every sample to `samples.csv` as it is
taken, and judges growth only at the end, so a run that is stopped keeps
its samples but has no verdict. Its tolerances do not scale with time, so
a longer run is stricter. A GitHub-hosted job stops after 6 hours, so a
soak of several days runs on a machine of our own (a self-hosted runner, or
by hand, as above); a 6-hour scheduled job on hosted runners checks less
but needs nothing set up (DESIGN.md §13, M11).

### Packages

Every release is published as (README.md, "Install"):

- **`.deb` packages in a signed apt repository** on GitHub Pages
  (`https://exadev.github.io/pg_warren/apt`, key at `.../apt/pg_warren.asc`):
  `postgresql-NN-pg-warren` for PG 15–18, for Debian 12 (bookworm) and 13
  (trixie) and Ubuntu 22.04 (jammy) and 24.04 (noble), on amd64 and arm64.
  They follow PGDG's conventions, to make a later PGDG submission easy:
  `packaging/debian` is a debhelper source package with `control.in`,
  `debian/pgversions` and `pg_buildext`, one binary package per major,
  `Depends: postgresql-NN`, built against `postgresql-server-dev-NN` from
  apt.postgresql.org, files in PGDG's paths
  (`/usr/lib/postgresql/NN/lib`, `/usr/share/postgresql/NN/extension`),
  and the shared-library dependencies (`libc6 (>= …)`) computed by
  `dh_shlibdeps` in each distribution's own container. The version is
  `X.Y.Z-1~deb12` (`~deb13`, `~ubuntu22.04`, `~ubuntu24.04`): one pool holds
  every distribution's, and an official PGDG build of the same release
  (`X.Y.Z-1.pgdg…`) sorts above it. The postinst prints what to do next
  (preload, restart, `CREATE EXTENSION`); it never edits `postgresql.conf`.
  Unlike PGDG's offline builds, ours take Rust and cargo-pgrx from rustup
  and crates.io (`packaging/build-deb.sh`, `dpkg-buildpackage -d`); a PGDG
  submission would build from the vendored source tarball (below and
  "Supply chain").
- **Tarballs** `pg_warren-X.Y.Z-pgNN-linux-{x86_64,aarch64}.tar.gz`, the
  jammy packages' files laid out as `pkglibdir/` and `sharedir/extension/`.
- **Docker images** `ghcr.io/exadev/pg_warren:X.Y.Z-pgNN` (and `pgNN` for
  the latest release), amd64 and arm64: `postgres:NN-trixie` with the
  trixie package's files (unpacked, not installed with apt: the build runs
  nothing, so either arch builds on any host) and pg_warren preloaded
  (`packaging/docker/Dockerfile`).
- **A Homebrew formula** in the tap `exadev/homebrew-tap`
  (`brew install exadev/tap/pg_warren`), rendered from
  `packaging/homebrew/pg_warren.rb.in` by `release/homebrew.py`, modelled
  on homebrew-core's pgvector (built for `postgresql@17` and `@18`), with a
  bottle for Apple silicon (`arm64_sequoia`, built on macOS 15) and a build
  from the release's source tarball elsewhere.
- **The source tarball** `pg_warren-X.Y.Z.tar.gz` (`git archive` of the
  release commit), which the formula builds from; the **vendored source
  tarball** `pg_warren-X.Y.Z-vendored.tar.gz` (that, with every locked
  crate, for an offline build); the **SBOM** `pg_warren-X.Y.Z.cdx.json`
  (CycloneDX); and `SHA256SUMS` of them all ("Supply chain", below).

`release-publish.yml` builds them in jobs that have only `contents: read`
and no secrets, from the recorded release commit's source with the
tooling (`release/`, `packaging/`) of the commit that started the run:
`debs` (one job per distribution and arch, the four majors each, every
package installed with apt into a clean container with PGDG's PostgreSQL,
preloaded, restarted and smoke-tested; on trixie, the images made from
them, each smoke-tested on its own arch), `homebrew` (`brew style`, `brew audit
--strict`, `brew install --build-bottle`, `brew test`, `brew bottle`) and
`source-assets` (the vendored source tarball and the SBOM, from the
source tarball, by `release/source.py`). The
Docker Hub images they build in and on (`debian`, `ubuntu`, `postgres`)
are pinned by digest in `release/packages.py`, and rustup-init by version
and sha256 in `packaging/build-deb.sh`; bump them deliberately, in their
own change.

Only after the `release` environment's approval does the publish job, which
runs no build code, take what they built, **as data**:

- it downloads the expected artifacts by name, and checks every `.deb`
  (`release/assets.py check-deb`): its control archive holds only
  `control`, `md5sums` and a `postinst` byte-equal to
  `packaging/debian/pg-warren.postinst.in` as debhelper renders it; its
  control file has only the fields `dh_gencontrol` writes for
  `packaging/debian/control.in` (no `Pre-Depends`, `Conflicts`, `Breaks`,
  `Replaces`, `Provides`, `Essential` or `Protected`), with control.in's
  values, and `Depends` is `postgresql-NN` and `libc6`/`libgcc-s1` at a
  minimum version (what `dh_shlibdeps` finds a pgrx library links); its
  files are only `usr/lib/postgresql/NN/lib/pg_warren.so`,
  `usr/share/postgresql/NN/extension/pg_warren*` and its docs, regular
  files of mode 0644 (as `dh_fixperms` leaves them, the library too), in
  directories of mode 0755 on those files' paths only, each owned by
  root:root by name and id (dpkg takes the owner by name where the host
  knows it); its Version is `X.Y.Z-1~<distribution>` and its file name its
  own;
- it makes the tarballs (`release/packages.py tarball`: `dpkg-deb -x`
  and Python) from the checked jammy packages, and the Docker images
  (`release/packages.py image --no-smoke`, both arches, the build running
  nothing) from the checked trixie packages on the digest-pinned
  `postgres` images, rather than taking either from the build jobs;
- it renders the formula again from the source tarball
  (`release/homebrew.py`), taking from the bottle job only a bottle block
  of this release's `root_url` and sha256 lines;
- it checks the vendored source tarball and the SBOM against the source
  tarball (`release/assets.py check-vendored`, `check-sbom`: every source
  file byte for byte and only `Cargo.lock`'s crates, each matching its
  checksum; a CycloneDX BOM of pg_warren at this version whose crates are
  `Cargo.lock`'s, with their checksums);
- it writes `SHA256SUMS` of every asset and signs it, `SHA256SUMS.asc`, with
  `RELEASE_APT_SIGNING_KEY`, after checking that key's fingerprint is the
  one README.md states (`<!-- apt key -->`), and puts the fingerprint in the
  Release's notes;
- it attaches the assets to the GitHub Release (never replacing one already
  attached, so a re-run is safe);
- it assembles the apt repository with aptly (noble's reprepro keeps one
  version per package) from this release's checked files and, for each
  earlier release recorded in `sql/released.commits` that is not a draft,
  only the `.deb` files its `SHA256SUMS.asc`-verified `SHA256SUMS` lists,
  each checked as above, so a file anyone with write access attaches to a
  Release never reaches the repository (releases before signed sums
  contribute nothing: the first packaged release starts the repository);
  and signs it;
- it pushes the images to GHCR with the environment's `GHCR_TOKEN` (the job
  has no `packages: write`), after checking with GitHub's API that the
  package is linked to no repository, and commits the formula to the tap
  with `HOMEBREW_TAP_DEPLOY_KEY`;
- after the image and tap steps succeed, it attaches an immutable apt snapshot:
  `pg_warren-apt-X.Y.Z.tar.gz`, its single-file checksum manifest
  `pg_warren-apt-X.Y.Z.sha256`, and the detached signature
  `pg_warren-apt-X.Y.Z.sha256.asc`. The signature is attached last. A rerun
  verifies and retains a complete existing snapshot rather than replacing it.
  A partial upload fails: review and remove that release's three snapshot
  assets before rerunning publication;
- successful publication triggers `.github/workflows/pages.yml`, which builds
  the website and combines it with the verified apt snapshot before deploying
  the whole tree behind the `github-pages` environment's approval. The release
  workflow itself no longer deploys to Pages.

Republishing an older release attaches and rebuilds the same way, but moves
neither the images' `pgNN` tags nor the tap. How far a second,
independent build can confirm the published packages today is in "Supply
chain", "Reproducible builds" (DESIGN.md §13).

CI keeps the path from rotting: `packages` builds `postgresql-15-pg-warren`
and `postgresql-16-pg-warren` in one build for bookworm on every pull
request with the same `release/packages.py`, checks them as the publish
job does (`release/assets.py check-deb`), installs the PG 16 one with apt
into a clean container, preloads, restarts and runs `packaging/smoke.sql`,
makes the tarball, builds and checks the Docker image, and builds the
packages a second time, which must give the same bytes;
`packages-release` builds what a release builds, PG 15-18 for every
distribution and arch, checks and installs each, and on trixie builds and
checks the images, off the pull-request path (as Miri: on pushes to main,
dispatches and release PRs); `macos` runs `brew style` and `brew audit` on
the formula. Locally, with Docker:

```sh
release/packages.py deb bookworm . dist --majors 16     # build (about 15 min)
release/packages.py check dist/postgresql-16-pg-warren_*.deb
release/packages.py tarball dist/postgresql-16-pg-warren_*.deb dist
release/packages.py image dist/postgresql-16-pg-warren_*.deb pg_warren:local-pg16
```

Not built yet: RPM packages (yum/dnf) and a PGXN release (a `META.json`
and an upload with a PGXN credential kept in the `release` environment);
both are later steps.

GitHub Pages serves at most 1 GB. The website and apt repository share that
budget, and assembly refuses a combined tree over 1,000,000,000 bytes. Every
release adds its 32 packages. Plan a reviewed retention policy or a separate
package host before reaching the limit; deleting assets from an immutable
snapshot does not prune its published contents and can break verification.

### Website and apt deployment

`pages.yml` is the sole Pages deployment owner. It runs on website/source
changes on `main`, manually on `main`, and after a successful `Release
(publish)` workflow on `main`. A `workflow_run` trigger is used because
release events created with `GITHUB_TOKEN` do not trigger another workflow.
Pull requests only build review artifacts; they cannot deploy.

One workflow-level `github-pages` concurrency group serialises the entire
build, snapshot selection, environment approval, and deployment. After
acquiring that lock, each run checks out current `main` and resolves current
published releases. An older queued push or an older release republished
later therefore cannot deploy an older source checkout or select an older
software release by timestamp.
Skipped failed-release events and dispatches on other branches use isolated
concurrency groups so they cannot displace a useful pending deployment.

The build job has `contents: read`, no release secrets, and no Pages write
permission. `release/pages.py` selects the highest numeric recorded,
published, non-prerelease packaged software version. Releases from 0.7.0
require a snapshot; earlier source-only releases are ignored, while earlier
releases with package/snapshot assets also require one. If no packaged
release has been published, the first website deployment contains no `/apt/`.
A newer packaged release with a missing or partial snapshot blocks deployment
instead of falling back to older packages or dropping `/apt/`. A failed GitHub
API request is an error, not evidence that no packages exist.

`release/pages_archive.py` checks the snapshot's exact checksum entry, archive
digest, the public key's primary fingerprint against README, the detached
manifest signature, and all four distributions' `InRelease` signatures. It
accepts only bounded regular files and directories under `apt/`, rejecting
links, traversal paths, duplicate members, and unexpected archive metadata.
Verification and assembly happen in private staging; the output becomes
available only after every check succeeds. Website files cannot claim the
reserved `apt/` or `deployment.json` paths. `deployment.json` records the
checked-out source commit and apt software version/digest.

The deploy job runs no repository code. It has only `pages: write` and
`id-token: write`, uses the existing `github-pages` environment, and deploys
the combined artifact. Approving that environment publishes both the website
and its selected apt snapshot.

If publication fails before attaching the snapshot, finish or rerun the
release workflow, then rerun the Pages workflow on `main`. A release published
without a snapshot (0.7.0, by the workflow before this one) gets one when
Release (publish) is run by hand for it: it builds nothing, and assembles the
snapshot from the packages its signed `SHA256SUMS` lists. Do not deploy a
website-only artifact as a workaround. For a first deployment, enable Pages
from Actions and approve the waiting `github-pages` deployment below. Local
website commands and canonical-content maintenance are in [site/README.md](site/README.md).

### Release settings (owner setup)

Set once in the repository's settings; the workflows assume them.

1. **Deploy key for tags.** Generate a key pair used for nothing else:
   `ssh-keygen -t ed25519 -N "" -C "pg_warren release tags" -f
   release_deploy_key`. In Settings → Deploy keys → Add deploy key, add
   `release_deploy_key.pub` with the title `release tags` and tick
   **Allow write access**. It must be the repository's **only deploy key
   with write access**: the ruleset's *Deploy keys* bypass (step 3) covers
   every write deploy key, so remove write access from any other, or
   remove it.
2. **Environment `release`** (Settings → Environments → New environment):
   *Required reviewers*: the people who may publish a release (with
   *Prevent self-review* if more than one); *Deployment branches and
   tags*: *Selected branches and tags*, with the branch rule `main`;
   *Environment secrets*: add `RELEASE_DEPLOY_KEY` with the whole private
   key file `release_deploy_key`. Then delete both key files locally. The
   secret reaches a job only after a reviewer approves it, and only on
   `main`. Approve only a publish run whose triggering commit is the
   release PR's merge commit (the run's page shows the commit, and the
   verify job's summary `vX.Y.Z → <commit>`); reject any other.
3. **Tag ruleset for `v*`** (Settings → Rules → Rulesets → New ruleset →
   New tag ruleset): enforcement *Active*; target tags matching the
   pattern `v*`; rules *Restrict creations*, *Restrict updates* and
   *Restrict deletions*; bypass list: **Deploy keys** ("Always allow") and
   *Repository admin* (for the manual fallback). Do not add the GitHub
   Actions app: any branch workflow with `contents: write` pushes as it.
   If your plan's ruleset bypass list does not offer *Deploy keys*, use a
   GitHub App of your own instead: create it with *Contents: read and
   write* on this repository only, add it as the bypass, keep its app id
   and private key as secrets of the `release` environment, and have the
   publish job push the tag with its installation token
   (`actions/create-github-app-token`, pinned by commit) instead of the
   deploy key.
4. **Actions → General → Workflow permissions**: keep *Read repository
   contents and packages permissions* (each job asks for what it needs),
   and leave **Allow GitHub Actions to create and approve pull requests**
   *unticked*: the release PR is opened by a person.
5. **The apt repository's signing key** (a key used for nothing else, no
   passphrase, as the job signs unattended):

   ```sh
   export GNUPGHOME="$(mktemp -d)"
   gpg --batch --passphrase '' --quick-gen-key "pg_warren apt repository" ed25519 sign never
   gpg --armor --export-secret-keys "pg_warren apt repository" >apt-signing-key.asc
   gpg --fingerprint "pg_warren apt repository"   # the 40 hex digits, for README.md
   # Optional: a revocation certificate, kept offline (not in the repository or a secret)
   gpg --output pg_warren-apt-revoke.asc --gen-revoke "pg_warren apt repository"
   ```

   In the `release` environment, add the secret `RELEASE_APT_SIGNING_KEY`
   with the whole file `apt-signing-key.asc`. In README.md's Install, replace
   `(set by the owner before the first release)` on the line marked
   `<!-- apt key -->` with the fingerprint (as `gpg --fingerprint` prints it, or
   without spaces), and merge that before the first
   release: the publish job refuses a key whose fingerprint README.md does
   not state. Then delete `apt-signing-key.asc` and `$GNUPGHOME`. The job
   exports the public key to
   `https://exadev.github.io/pg_warren/apt/pg_warren.asc` on every release.
   Keep the same key: users trust it once, and a new key (with README.md
   changed in the same PR) breaks their `apt update` until they fetch it
   again.
6. **GitHub Pages**: Settings → Pages → *Build and deployment* → Source
   **GitHub Actions**. This creates the `github-pages` environment. In
   Settings → Environments → `github-pages`: add yourself as a *Required
   reviewer*, and check *Deployment branches and tags* admits only `main`.
   The `deploy` job in `pages.yml` waits for this approval before publishing
   the website and `/apt/` together. Package snapshots can only be signed by
   the `release` environment, whose approval remains separate. Website-only
   updates reuse the signed snapshot without needing the release signing
   secret. The first website can deploy before any packages are published.
7. **The Docker images**: the publish job pushes with a credential only the
   `release` environment holds, not `GITHUB_TOKEN`, so no other workflow
   can push. GHCR takes neither GitHub App nor fine-grained tokens, so
   create a **classic personal access token** with only the
   `write:packages` scope (it implies `read:packages`), on an account that
   may publish packages for the org (a dedicated machine account if you
   have one), with an expiry you will renew. In the `release` environment,
   add it as the secret `GHCR_TOKEN`, and that account's login as the
   environment *variable* `GHCR_USERNAME`. The images carry no
   `org.opencontainers.image.source` label, so the first push creates the
   org's package `pg_warren` linked to no repository: a linked package
   inherits the repository's access by default, which would let any
   workflow here that asks for `packages: write` push the images. Keep it
   unlinked: the publish job refuses to push to a package linked to a
   repository (GitHub's API shows the link, not the inheritance setting,
   so the link is what it checks). If it refuses, delete the package
   (Package settings → *Delete this package*) rather than disconnect it:
   whatever pushed to it while it was linked may have left tags a release
   does not overwrite. After the first release, in Packages →
   pg_warren → Package settings: *Change visibility* → **Public**; do not
   *Connect repository*; under *Manage Actions access*, add no repository
   (or this one with **Read** at most). The token can write to every
   package that account can: keep the account's package rights to this
   one.
8. **The Homebrew tap**: create the public repository
   `ExaDev/homebrew-tap` (with a README, so that `main` exists), and protect
   its `main` against force-pushes and deletion (a branch ruleset). Generate
   a key pair used for nothing else:
   `ssh-keygen -t ed25519 -N "" -C "pg_warren homebrew tap" -f tap_deploy_key`.
   In **the tap's** Settings → Deploy keys, add `tap_deploy_key.pub` with
   **Allow write access**; in this repository's `release` environment, add
   the secret `HOMEBREW_TAP_DEPLOY_KEY` with the private key file
   `tap_deploy_key`; delete both files. The key can write only to the tap,
   so it widens nothing here: this repository's tag ruleset bypass (step 3)
   is for this repository's own deploy keys. Without the secret, the publish
   job skips the tap with a warning.
9. **Protect `main`** (a branch ruleset or classic protection): require
   a pull request (and status checks if wanted); block force-pushes and
   deletions. This also keeps the deploy key, which can write, from
   pushing to `main`: a direct push is refused. If anyone other than the
   owner has write access or can merge, also require one approval and
   **approval of the most recent reviewable push**. (While every PR is
   opened and merged under the owner's account, a required approval
   would block every merge, since GitHub does not let an author approve
   their own PR.) A dispatched CI run's checks satisfy required status
   checks on a release PR: they have the same names as a
   `pull_request` run's. Keep *Allow merge commits* enabled (Settings →
   General → Pull requests): release PRs are merged with one.

## Supply chain

What a packager or a managed-PostgreSQL provider needs to audit pg_warren,
rebuild it with its own build system and check a release against its own
build (DESIGN.md §13, 0.9.0; issue #103).

### Dependencies (cargo-deny)

`deny.toml` is the policy for every crate in `Cargo.lock`, on every target
platform, dev- and build-dependencies included; CI's `deny` job runs it on
every push, pull request and night:

- **advisories**: a RustSec vulnerability, or an unmaintained, unsound or
  yanked crate, fails. An exemption is by advisory id, with the reason and
  what would end it (today one: `serde_cbor`, unmaintained, which pgrx
  0.19.3 depends on). Fix by updating within the locked requirements
  (`cargo update -p CRATE`) where a fixed version exists.
- **licenses**: an allow-list of the licences the tree uses (Apache-2.0,
  MIT, BSD-3-Clause, ISC, Unicode-3.0, Zlib). A crate offering a choice
  passes on any one of them. A new licence is a reviewed change to the
  list.
- **bans**: duplicate versions warn (each is two semver-incompatible
  majors pulled in upstream); a wildcard requirement fails.
- **sources**: crates.io only; no git dependency, no other registry.

```sh
cargo install cargo-deny --version 0.20.2 --locked
cargo deny --locked check
```

### Building from source

Inputs, all pinned by the release's source:

- **Rust**, the exact toolchain in `rust-toolchain.toml` (1.97.0); rustup
  installs it from that file.
- **The crates** in `Cargo.lock` (pgrx `=0.19.3`), all from crates.io; the
  vendored source tarball carries them.
- **PostgreSQL's server headers and `pg_config`** for the major
  (`postgresql-server-dev-NN`), **clang and libclang** (pgrx's bindgen
  generates the bindings from those headers at build time), and a C
  toolchain and linker (`cc`, binutils).
- **cargo-pgrx 0.19.3**, only for `cargo pgrx package`/`install` (README,
  "From source"; the release's packages use it). The offline build below
  does without it: each release commit carries its generated install
  script, `sql/pg_warren--X.Y.Z.sql` (CI checks it is what pgrx generates),
  and every upgrade script.

Each GitHub Release has `pg_warren-X.Y.Z-vendored.tar.gz`: the source
tarball's files, `vendor/` with every crate `Cargo.lock` pins (`cargo
vendor --locked --versioned-dirs`, all platforms') and a
`.cargo/config.toml` that points crates.io there. It builds with no
network access:

```sh
tar -xzf pg_warren-X.Y.Z-vendored.tar.gz && cd pg_warren-X.Y.Z
rustup toolchain install                     # rust-toolchain.toml, if not already there
export PGRX_PG_CONFIG_PATH=/usr/lib/postgresql/16/bin/pg_config
packaging/reproducibly.sh . \
  cargo rustc --frozen --release -p pg_warren --lib --no-default-features --features pg16 \
    -- -C link-arg=-Wl,--no-gc-sections
# Install where that pg_config says:
install -m 0755 target/release/libpg_warren.so "$("$PGRX_PG_CONFIG_PATH" --pkglibdir)/pg_warren.so"
ext="$("$PGRX_PG_CONFIG_PATH" --sharedir)/extension"
sed 's/@CARGO_VERSION@/X.Y.Z/' crates/pg_warren/pg_warren.control >"$ext/pg_warren.control"
install -m 0644 sql/pg_warren--X.Y.Z.sql sql/pg_warren--*--*.sql "$ext/"
```

`--frozen` is `--locked --offline`: cargo refuses to touch the network or
change `Cargo.lock`, and checks every vendored file against its crate's
`.cargo-checksum.json`, whose package checksum is `Cargo.lock`'s. The
`cargo rustc` line is the build `cargo pgrx package` runs on Linux
(cargo-pgrx 0.19.3 links with `--no-gc-sections`). For another major,
change `pg16` and the `pg_config`. `packaging/reproducibly.sh` only sets
`RUSTFLAGS` and `CFLAGS` to make the build reproducible (below); the build
works without it. The files installed are the packages' (the install
script and the upgrade scripts). To vendor from a checkout instead:
`cargo vendor --locked --versioned-dirs vendor` and the
`.cargo/config.toml` that `release/source.py` writes (`CARGO_CONFIG`).

### Verifying a release

Each GitHub Release's `SHA256SUMS` lists every asset, and
`SHA256SUMS.asc` signs it with the apt repository's key, whose fingerprint
README.md states (`<!-- apt key -->`) and the Release's notes repeat:

```sh
gpg --show-keys pg_warren.asc                # the fingerprint README.md states
gpg --no-default-keyring --keyring ./pg_warren.gpg --import pg_warren.asc
gpgv --keyring ./pg_warren.gpg SHA256SUMS.asc SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
```

(`pg_warren.asc` is `https://exadev.github.io/pg_warren/apt/pg_warren.asc`.)
The signed sums cover:

- `pg_warren-X.Y.Z.tar.gz`, `git archive` of the release commit (its
  global pax comment names the commit; compare it with the tag);
- `pg_warren-X.Y.Z-vendored.tar.gz`, that and the locked crates. Check it
  against the source tarball as the publish job does:
  `release/assets.py check-vendored pg_warren-X.Y.Z-vendored.tar.gz
  pg_warren-X.Y.Z.tar.gz` (every source file byte for byte; only
  `Cargo.lock`'s crates, each matching its checksum; nothing else). Or
  make it again, `release/source.py vendored pg_warren-X.Y.Z.tar.gz OUT`
  with the release's toolchain: the same bytes;
- `pg_warren-X.Y.Z.cdx.json`, pg_warren's **SBOM**: CycloneDX 1.5 JSON
  from cargo-cyclonedx 0.5.9 (every platform and feature), each crate with
  its version, licence, purl and `Cargo.lock`'s sha256, dated by the
  release commit, which it names. CycloneDX rather than SPDX: the Rust
  ecosystem's maintained generator (the CycloneDX project's own) writes
  it, it records each crate's purl, checksum and dependency graph, and
  scanners (Dependency-Track, Grype, Trivy) read it. `release/assets.py
  check-sbom` checks it against the source tarball; `release/source.py
  sbom` makes it again, the same bytes;
- the packages, tarballs and bottle ("Packages", above).

### Reproducible builds

Built with the same inputs (above) through `packaging/reproducibly.sh
SOURCE`, `pg_warren.so` is the same bytes wherever the source, its target
directory and `CARGO_HOME` are, and whether the crates come from crates.io
or from `vendor/`. Compilers write source paths into the library: rustc in
panic locations, and the C compiler through `__FILE__` in the C shim that
pgrx-pg-sys builds in its `OUT_DIR`. The script maps the source to
`/pg_warren` and both crate directories to `/crates`
(`--remap-path-prefix` in `RUSTFLAGS`, `-ffile-prefix-map` in `CFLAGS`).
The rest is already deterministic: `codegen-units = 1` and fat LTO in the
release profile, a GNU build-id that hashes the contents, no timestamps.
CI's `source-build` job builds the library twice, in the checkout from
crates.io and elsewhere from the vendored tarball offline, and fails if
the two differ. (Measured: a PG 16 release build both ways gave the same
sha256; without the maps the library held the build directory.)

The packages ship the committed install script, `sql/pg_warren--X.Y.Z.sql`
(frozen in `sql/released.sha256` once released), rather than the one
`cargo pgrx package` generates: pgrx orders the script's entities
differently from one build to the next, which alone was enough to make
every rebuilt `.deb` differ. Measured: two builds of
`postgresql-16-pg-warren` for bookworm from the same source, back to back,
gave the same `pg_warren.so` and differed only in that script; with the
committed script, two more gave the same `.deb`, byte for byte (and the
same library as the first two). CI's `packages` job builds its packages a
second time and fails if any byte differs. `sql/schema-drift.sh` checks
the committed script holds the entities pgrx generates. The Homebrew
formula ships the committed script too.

The vendored source tarball and the SBOM are the same bytes each time
they are made from the same source tarball (CI checks both).

Not reproducible, or not yet checked:

- **The same library from different inputs.** Another `rustc`, other
  PostgreSQL headers (a minor release can change them, and bindgen's
  output with them), another clang, C compiler or linker give other bytes.
  A provider's build is comparable with ours only from the same toolchain
  and headers, and a plain `cargo` build is not the `.deb`'s library even
  then: the package is built in its distribution's container, with that
  distribution's C compiler and linker and `dpkg-buildflags`' `CFLAGS`,
  and `dh_strip` strips the library. To compare with a `.deb`, rebuild the
  `.deb` (`release/packages.py deb`, "Packages") from the release's
  source.
- **The `.deb` packages, rebuilt later.** The build container is pinned
  by digest, but `packaging/build-deb.sh` installs the current PGDG server
  headers, debhelper, binutils and clang with apt at build time, and
  records none of their versions, so a later rebuild may see other
  headers or tools. Recording them (Debian's `.buildinfo`, or `dpkg -l`
  of the build container as a release asset) and installing those
  versions again from snapshot archives is not done yet. CI compares two
  builds made minutes apart, not a build with a release's.
- **The Homebrew bottle** (macOS's toolchain, Homebrew's PostgreSQL and
  its bottle tarball) and **the Docker images** (their layers carry build
  times) are not checked.

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
