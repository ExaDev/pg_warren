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

# Domain tests under Miri (nightly; CI runs it on pushes to main, not on PRs; DESIGN.md §11)
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
# /var/tmp/warren-corpus, checked against NetworkX and timed (release build
# in --pg-config's tree); results in bench/results/corpus/
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
     so what is shown is what is tagged. It
     then pushes the annotated tag `vX.Y.Z` with the environment's deploy
     key and creates the GitHub Release with CHANGELOG's section as its
     notes. A re-run succeeds if the tag is already on the recorded commit
     and fails if it is on another; run it by hand (with a version, or
     none for the latest) to publish again.
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
test/dump.py --pg-config "$(command -v pg_config)"   # pg_dump and restore of graphs (plain, custom, trusted)
```

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
5. **Protect `main`** (a branch ruleset or classic protection): require
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
