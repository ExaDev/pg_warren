# Security and operations

For an administrator, a DBA or a managed-PostgreSQL provider deciding
whether pg_warren is safe to install. This page collects what the rest of
the documentation and the code say about privileges, failures, resources,
testing and releases, and links to the details. It describes `main`; where
something is planned rather than built, it says so.

## In short

- **It is a compiled library with a fixed SQL surface.** It creates one
  schema, `warren`, and runs no code a user supplies other than SQL that the
  user, or a graph's owner, could run anyway.
- **No background workers, no network, no files of its own.** It uses
  Postgres's dynamic shared memory for a cache shared across backends, and
  threads inside a backend for computation. Only the backend's main thread
  calls into Postgres.
- **Non-superusers can install it** (`trusted = true`). The installer gets no
  privilege on pg_warren's catalog tables. Every role, the installer
  included, reaches graphs only through `warren.*` functions, which check
  access on every call.
- **No function is `SECURITY DEFINER`.** pg_warren's own catalog statements
  run as the catalog's owner inside a narrow identity switch, under a pinned
  `search_path`. User-written SQL never runs elevated.
- **Errors are SQL errors.** Panics, cancellation, timeouts, memory limits
  and allocation failures of large buffers end the statement with an
  `ERROR`, not the backend.
- **Writes are refused on standbys** with SQLSTATE 25006, and every read
  works there.
- **Known gaps** are listed at the end: per-session resource settings that
  any role can raise, `pg_dump` by a non-superuser, and supply-chain items
  (reproducible builds, an SBOM, dependency audits, a security policy).

## What it installs

### Database objects

`CREATE EXTENSION pg_warren` runs one script that creates, all in schema
`warren` (the extension is not relocatable):

- **Catalog tables**: `graphs`, `versions`, `segments`, `vectors`,
  `vector_chunks` and `vector_quotas`, their sequences and indexes, and
  three trigger functions with their triggers, which keep
  `graphs.current_version` and `versions.label_vector` consistent (DESIGN.md
  §5.1). Graph data is stored in these ordinary tables, so it is WAL-logged,
  backed up, replicated and transactional like any other data.
- **Functions**, `\df warren.*` lists them: graph management, traversal,
  compute, introspection. Most are C functions in the shared library. The
  library of algorithms (`hits`, `louvain`, `label_propagation`, …) is
  PL/pgSQL, and each such function sets
  `search_path = pg_catalog, pg_temp`.
- **A composite type** `warren.step` and **a cast** from `jsonb` to
  `warren.step[]`, used by `warren.walk`. Casts are not schema objects; this
  one converts only to pg_warren's own type.

The script also runs `REVOKE ALL … FROM PUBLIC` on every `warren.*` table
and sequence, revokes `EXECUTE` on the trigger functions, grants `USAGE` on
the schema to `PUBLIC`, and registers every table and sequence with
`pg_extension_config_dump` so that `pg_dump` includes graph data
(`crates/pg_warren/src/catalog/bootstrap.rs`).

It does **not** create an event trigger, a role, a background worker or
anything outside schema `warren` other than the cast. The optional
`DROP OWNED` event trigger is something a superuser may create by hand
([semantics.md, "Access"](semantics.md#access)).

### Settings

Six GUCs, registered when the library loads (`_PG_init` in
`crates/pg_warren/src/lib.rs`). Sizes are in kB.

| Setting | Context | Default | Range | Bounds |
|---|---|---|---|---|
| `warren.shared_cache_memory` | `sighup` | `4GB` | `0` (off) to 2³¹−1 kB | the cluster's shared cache, all databases together |
| `warren.max_cache_memory` | `user` | `4GB` | `64kB` up | what one backend's private cache holds between statements |
| `warren.max_compute_memory` | `user` | `4GB` | `64kB` up | what one `warren.*` call holds |
| `warren.max_build_memory` | `user` | `4GB` | `1MB` up | what one `refresh` or `contract` holds |
| `warren.threads` | `user` | half the cores, at least 1 | 1–1024 | compute threads per backend |
| `warren.max_walk_paths` | `user` | `10000000` | 1 up | rows a path-per-row `walk` may be asked for |

`sighup` settings are changed in the server's configuration and take effect
on reload. `user` settings can be changed by any role, per session or
transaction (see [Resource use and limits](#resource-use-and-limits)).
What each bounds is in [limits.md, "Settings"](limits.md#settings).

### Shared memory, hooks and callbacks

- **`shared_preload_libraries` is optional.** pg_warren works without it on
  every supported major (15–18). On PG 15 and 16 the preload is what enables
  the shared cache and lets `warren.backends()` see other backends; without
  it each backend keeps its own copy of the graphs it reads. On PG 17 and 18
  nothing needs configuring.
- **When preloaded on PG 15 and 16** it requests a small, fixed amount of
  main shared memory (`shmem_request_hook`, `shmem_startup_hook`): the
  shared cache's registry (128 slots, a few tens of kB) and an activity
  table with one slot per backend (`MaxBackends`), and two named LWLock
  tranches. On PG 17 and 18 the same two structures are named DSM segments
  (`GetNamedDSMSegment`), created on first use.
- **Loaded graph versions** are kept in one dynamic shared memory segment
  each, bounded in total by `warren.shared_cache_memory`. With
  `dynamic_shared_memory_type = posix` on Linux these are files in
  `/dev/shm`, created by Postgres. Before creating one, pg_warren checks the
  free space there (`statvfs`) and keeps a quarter of it free, falling back
  to a private copy rather than fail (DESIGN.md §5.6, "Size checks";
  `crates/pg_warren/src/shared_memory.rs`). A crash restart destroys every
  segment.
- **Hooks and callbacks**: a `ProcessUtility_hook`, installed when the
  library loads, that frees pg_warren's session state after `DISCARD ALL`
  (`crates/pg_warren/src/discard.rs`); transaction and subtransaction
  callbacks and `before_shmem_exit` callbacks that give back shared-cache
  slots, activity slots and stop compute threads at exit; and memory-context
  reset callbacks that end per-statement pins. Each chains to any previous
  hook.
- **No background workers.** None is registered, static or dynamic. A
  compute background worker is a future direction (DESIGN.md §6.9), not
  built.

### Files, network and threads

- **Files.** pg_warren writes no files. Everything it persists is rows in
  its catalog tables. It reads file-system statistics (`statvfs`) of
  `/dev/shm` or `pg_dynshmem`, and Postgres itself creates the DSM segments.
  Messages go to the client and server log through Postgres's own error
  reporting (`NOTICE`, `WARNING`, `DEBUG1`).
- **Network.** None. The library opens no sockets; its Rust dependencies
  are `pgrx`, `libc`, `serde_json`, `thiserror`, `rayon` and `nalgebra`
  (`crates/*/Cargo.toml`), none used for I/O.
- **Threads.** Each backend that runs a compute kernel builds one thread
  pool of `warren.threads` threads (`rayon`), on first use. **Only the main
  thread calls Postgres** (DESIGN.md §6.9): the kernels live in
  `warren-core`, a crate with no Postgres dependency (CI fails if
  `cargo tree -p warren-core` mentions pgrx or pg-sys), and take plain
  slices of memory pg_warren owns. Pool threads start with every signal
  blocked, so Postgres's signal handlers run only on the main thread
  (`crates/pg_warren/src/interrupts/pool.rs`). The pool is stopped before
  Postgres unmaps shared memory at backend exit.

## Who can do what

### The trusted install

`pg_warren.control` sets `trusted = true` and `superuser = true`. Together
these mean: a superuser can install it, and so can any role with `CREATE`
on the database. In the second case Postgres runs the install script as the
bootstrap superuser, which then owns the schema and every catalog table,
while the installing role becomes the extension's owner
(`pg_extension.extowner`). DESIGN.md §7, "Trusted install", has the audit
of every install and upgrade script that this depends on: objects are
created only in `warren`, `CREATE SCHEMA IF NOT EXISTS` refuses to adopt a
schema created outside the extension (CVE-2022-2625 hardening), and
nothing evaluated at install time can be shadowed by a user's objects.

What the installer can do as the extension's owner: `ALTER EXTENSION
pg_warren UPDATE`, and `DROP EXTENSION`, which drops every graph in the
database. What it cannot do: read or write the catalog tables directly, read
other roles' graphs, or change who owns them. Granting it access to the
catalog was rejected because it could then rewrite a graph's source query,
which `refresh` runs as that graph's owner, a superuser included.
`test/sql/trusted_install.sql` checks these refusals, and CI runs the whole
SQL suite and the upgrade test with pg_warren installed by a non-superuser
(`test/run.py --trusted`, `test/upgrade.py --trusted`).

### Graphs and their access

Any role can call `warren.create_graph`; it becomes the graph's owner.
A graph exposes what its owner could see when it was built, like a
materialised view, so source-table row-level security does not apply to
what a traversal returns. Access is per graph
([semantics.md, "Access"](semantics.md#access)):

- **The owner** (and members of its role) may manage the graph: `refresh`,
  `promote`, `drop_*`, `compact`, `grant`, `revoke`, `set_owner`, quotas.
- **Readers**, roles added with `warren.grant` (and their members), may
  query it and write vectors on it, within an optional per-role quota.
- **Anyone** may call `warren.version()`, `warren.last_stats()`,
  `warren.evict()` (which frees only the caller's memory unless it owns the
  graph) and the introspection functions, which show only what the caller
  may see ([below](#what-a-non-superuser-can-see)).

Every query and management function checks membership on every call, from
Postgres's role caches, and management functions check ownership again
after taking the graph's row lock (DESIGN.md §7). Others get
`insufficient_privilege` (42501).

### Whose SQL runs as whom

| SQL | Runs as | `search_path` |
|---|---|---|
| a graph's `node_query` and `edge_query`, at `refresh` | the graph's owner | the one captured at `create_graph` |
| `vector_from_query`'s and `gather`'s queries, `vector_map` expressions | the caller | the caller's |
| pg_warren's own statements on its catalog | the catalog owner (the role that owns the `warren.*` tables) | `pg_catalog, pg_temp` |

All `warren.*` functions are `SECURITY INVOKER`; none is `SECURITY
DEFINER`. Instead, each catalog statement runs inside a switch to the
catalog owner (`SetUserIdAndSecContext` with
`SECURITY_LOCAL_USERID_CHANGE | SECURITY_RESTRICTED_OPERATION`), and source
queries inside a switch to the graph's owner, in the same restricted mode
(`crates/pg_warren/src/access/identity.rs`). The elevation therefore covers
only SQL that pg_warren wrote. An `ERROR` inside a switch is undone by the
transaction's abort.

User-written SQL that pg_warren runs (source, value and message queries,
executor-path `vector_map` expressions) also runs in Postgres's read-only
transaction state. A function it calls cannot write to tables (25006), and
a data-modifying CTE is refused. This protects what a build or a kernel is
reading; it is **not a sandbox**: writes to existing temporary tables,
`pg_notify`, `LOCK` and writes over another connection still work, with the
privileges of the role the SQL already runs as.

### `search_path` pinning

Schema-qualifying table names is not enough to make extension SQL safe: an
operator such as `=` or a catalog function such as `pg_has_role` also
resolves through `search_path`, so a caller could otherwise put its own
ahead of `pg_catalog` and have it run as the catalog owner or answer an
access check. So **every `warren.*` entry point runs its whole body under
`access::entry`**, which pins `search_path = pg_catalog, pg_temp` at a new
GUC nest level, as Postgres does for `SECURITY DEFINER … SET search_path`,
and restores it on return or abort
(`crates/pg_warren/src/access/guard.rs`). The caller's own `search_path` is
captured first and used only for the caller's own SQL.

A graph's source queries run under the `search_path` its creator had. If
that path includes a schema another role can create objects in (often
`public` when someone else owns the database), that role could add a
function or operator the queries call as the graph's owner at every
`refresh`. `create_graph` raises a `NOTICE` naming such schemas; the remedy
is to schema-qualify functions and operators in source queries, or create
graphs under a path only their owner can write (README, "Install";
`test/sql/search_path_notice.sql`).

### What a non-superuser can see

- **Catalog tables**: nothing directly. Their names appear in `pg_class`,
  and every function's definition in `pg_proc`, as for any extension.
- **Graphs**: there is no function that lists graphs. A role that names a
  graph it may not read is refused, and the refusal (42501) says the graph
  exists and names its owner role. Graph names share one namespace per
  database.
- **Introspection**: `warren.shared_cache()` and `warren.cache()` list only
  versions and vectors of graphs the caller may read, plus cluster totals
  (sizes and counts). `warren.backends()` and `warren.progress()` follow
  `pg_stat_activity`'s rule: details only for backends of roles whose
  privileges the caller has, or with `pg_read_all_stats`; a graph's name
  only where the caller may also read that graph. `warren.orphans()` shows
  a non-owner only rows of graphs it owns (DESIGN.md §5.7).

### Dumps and restores

Graph data is extension configuration data, so a superuser's `pg_dump`
includes it, and `test/dump.py` (CI, every major) checks that plain and
custom dumps restore through `psql` and `pg_restore` with versions, vectors
and answers intact. The roles that own and read graphs must exist first.

**A non-superuser's `pg_dump` fails while pg_warren is installed**, because
only the catalog owner can read the catalog. It can dump everything else
with `--exclude-table-data='warren.*'`; graphs are then created again with
`create_graph` and `refresh` after the restore, losing persisted vectors and
version history (DESIGN.md §7, "Dumps"). Postgres's predefined roles were
checked by hand on PG 18 with 0.7.0: a role granted `pg_read_all_data`
dumps the catalog, and one granted `pg_write_all_data` restores it. That is
**not yet tested in CI**. How a managed service without a superuser should
dump graphs is open: DESIGN.md §14 item 51, tracked in
[#103](https://github.com/ExaDev/pg_warren/issues/103).

### Connection poolers and `DISCARD ALL`

`DISCARD ALL` (PgBouncer's `server_reset_query` in session pooling) also
frees pg_warren's session state: every role's transient vectors on that
backend and `last_stats()`. Cached graph versions and persisted vectors are
not session state, and stay. In transaction pooling nothing is reset
between clients, so a transient vector left at the end of a transaction can
be read by another client **of the same role** on that backend (transient
vectors are private per role). Create, read and drop transient vectors
within one transaction behind a transaction pooler
([limits.md, "Known limitations"](limits.md#known-limitations)).
`test/pooler_soak.py` checks both pool modes in CI.

### Dropped roles

Owners and readers are stored as role OIDs, which `DROP ROLE` does not see.
pg_warren provides `warren.reassign_owned` and `warren.drop_owned` for the
standard role-removal procedure, an optional event trigger a superuser may
add, and `warren.orphans()` / `warren.drop_orphans()` to sweep up after a
`DROP ROLE` that skipped them. Only a superuser may adopt a graph whose
owner was dropped ([semantics.md, "Access"](semantics.md#access)). Until the
sweep runs, a new role that is given a dropped role's OID would inherit what
that OID still names.

## Failure model

### Errors and panics

- Errors are raised only through pgrx (`ereport!` with a standard
  SQLSTATE), never by calling Postgres's error functions directly. The
  SQLSTATEs pg_warren raises are documented per function.
- A Rust panic is caught by pgrx's guard at the function boundary and
  raised as an `ERROR`; the transaction aborts and the backend continues.
  `unwrap`, `expect` and `panic!` are lint errors outside tests, so a panic
  is a bug, and the SQL fuzzer fails on any internal error.
- pg_warren never catches an unwind around a Postgres call. The one place
  it holds a caught Postgres `ERROR` is the compute pool's wait: when
  cancellation arrives while worker threads run a kernel, it raises the
  kernel's cancellation flag, waits for the threads to stop, then resumes
  the unwind. Nothing calls Postgres in between (DESIGN.md §6.9).
- Every Postgres function pg_warren calls goes through pgrx's guarded
  bindings, so an `ERROR` inside one unwinds the Rust frames cleanly. Every
  `unsafe` block carries a `// SAFETY:` justification (a lint error
  otherwise); `warren-core` has no `unsafe` code.
- Cleanup that a transaction abort does itself (closing an SPI cursor,
  popping a GUC nest level, restoring the user id) is left to the abort
  while an `ERROR` unwinds, so it happens in Postgres's order.

### Cancellation and timeouts

Every long call checks for interrupts: traversals every 65,536 units of
work; compute kernels from the main thread every millisecond, while worker
threads check a cancellation flag every task of at most 4,096 rows. So
`statement_timeout`, `pg_cancel_backend` and `pg_terminate_backend` stop a
call promptly, with `query_canceled` for a cancel. A backend terminated
mid-kernel stops its threads before exit
(`test/sql/shared_cache_exit_mid_kernel.sql`), and a backend that is
waiting for another to load a version into the shared cache wakes every
second to check for interrupts.

### Memory

- Graph-sized buffers are reserved with `try_reserve`, so an allocation the
  system refuses is `out_of_memory` (53200), not an abort. Other, small
  allocations use Rust's ordinary allocator; as with any C extension, a
  failure there ends the backend.
- Within that, pg_warren bounds its own memory with the settings above.
  A call that would exceed `warren.max_compute_memory` or
  `warren.max_build_memory` is refused with `configuration_limit_exceeded`
  (53400) before it allocates. A backend's private memory stays within
  `warren.max_cache_memory` plus `warren.max_compute_memory` (plus
  `warren.max_build_memory` during a `refresh`), and the cluster's shared
  graph memory within `warren.shared_cache_memory`. Not bounded by a
  setting: a traversal's visited set and frontier, and several calls running
  at once in one statement ([limits.md, "Memory model"](limits.md#memory-model)).

### Crashes and recovery

Graph data lives in ordinary WAL-logged tables, so crash recovery is
Postgres's own: a `refresh` interrupted by a crash leaves no version, as its
transaction never committed. A crashed backend makes the postmaster
reinitialise shared memory, which destroys every shared-cache segment and
the registry, so there is nothing of pg_warren's to repair. A backend that
exits with `FATAL` gives back its shared-cache slots in its exit callback.
`test/crash_recovery.py` stops refreshes mid-way by `pg_terminate_backend`,
`SIGKILL` and an immediate shutdown, and checks the catalog, the answers,
the shared cache and a new refresh afterwards.

### Standbys and read-only transactions

On a physical standby every query function answers as on the primary once
it has replayed the same WAL, and computations that persist nothing work.
Every function that would change the catalog fails with
`read_only_sql_transaction` (25006), with a hint that the server is a
standby; so does any such call in a read-only transaction on a primary.
All catalog writes go through one writable SPI connection
(`read_only::writing`) that refuses first, and a lint forbids the other
writable SPI calls outside tests. A promoted standby writes normally.
`test/replicas.py` checks this on a hot standby and a cascading standby,
including recovery conflicts (DESIGN.md §5.1, "Standbys").

### Parallel query

Functions carry the parallel label their behaviour allows (DESIGN.md §9.3;
[semantics.md, "Parallel query labels"](semantics.md#parallel-query-labels)):
writers and anything that may persist are `PARALLEL UNSAFE`; vector reads
and a few others are `PARALLEL RESTRICTED`; graph reads that use no vector,
and functions that read no graph, are `PARALLEL SAFE`. Reads assign no
transaction id, so they work in parallel mode (a lint enforces this). Where
a version is not shared, each parallel worker loads its own copy, within
its own memory limits; any role can avoid that with
`max_parallel_workers_per_gather = 0`.

## Resource use and limits

Today's behaviour:

- `warren.shared_cache_memory` is set in the server's configuration only,
  so it is the one cluster-wide bound a provider controls. Default `4GB`:
  shared versions stay until evicted, dropped or the server restarts, so an
  idle cluster can hold up to the cap. In containers, `/dev/shm` must be
  sized for it (Docker's default is 64 MB); pg_warren falls back to private
  copies rather than fail when it is short.
- The other five settings are `user` settings: **any role can raise them for
  its own sessions**, up to 1,024 threads per backend and about 2 TB of
  memory per setting. `ALTER ROLE … SET` and `ALTER DATABASE … SET` set
  defaults, not ceilings. So on a shared instance one role can run many
  threads, or large computations, in each of its connections. Postgres's own
  limits (`max_connections`, `statement_timeout`, the operating system's
  memory limits) still apply.
- Per-database shares of the shared cache do not exist: one database's
  loads can evict another's unpinned versions.

Administrator-set ceilings on these settings and a cap on compute threads
across backends are planned for 0.9.0
([#103](https://github.com/ExaDev/pg_warren/issues/103), "Resource
governance across the instance"). Sizing guidance is in
[limits.md](limits.md#the-shared-cache).

## Test evidence

Workflows: [`ci.yml`](../.github/workflows/ci.yml) (every pull request,
every push to `main`, and nightly), [`corpus.yml`](../.github/workflows/corpus.yml)
(nightly), [`release-publish.yml`](../.github/workflows/release-publish.yml).
How to run each locally is in [CONTRIBUTING.md](../CONTRIBUTING.md).

| Evidence | What it checks | Where it runs |
|---|---|---|
| `warren-core` unit and property tests | kernels and data structures against reference implementations | every CI run |
| `#[pg_test]` suite (`cargo pgrx test`) | the SQL surface, SPI adapters, transactions, roles | PG 15, 16, 17, 18 on x86_64; PG 16 and 18 on arm64; PG 18 on macOS arm64 (reported, not yet gating) |
| Hostile `search_path` (`access::guard`, `every_warren_function_ignores_the_callers_search_path`) | every `warren.*` function, called with an attacker's operators ahead of `pg_catalog`, never runs them and its access checks hold; fails if a new function is not in the list | in the pgrx suite |
| Parallel labels (`parallel_labels`, `every_function_has_its_declared_parallel_label`, `every_read_runs_in_parallel_mode`) | each function's label is the declared one, and every non-`UNSAFE` read runs in parallel mode and in a worker | in the pgrx suite |
| SQL harness (`test/run.py`, `test/sql/`) | integration tests, races between sessions, the shared cache across backends, `DISCARD ALL`, the trusted install, documentation examples | PG 16 (x86_64) at the default, trusted and preloaded; the shared-cache tests in three modes on PG 15–18; arm64 and macOS |
| Oracle tests (`test/sql/oracle_*.sql`) | results against NetworkX and recursive SQL | in the SQL harness |
| Real graphs (`bench/corpus`) | 9 public datasets checked against NetworkX and igraph | nightly |
| Segment decoder fuzzing (`fuzz/`, cargo-fuzz) | the decoders of stored graph segments (CSR, id maps, tombstones, relation dictionary, membership) on arbitrary bytes, 30 s per target | every CI run |
| SQL-surface fuzzer (`test/sql_fuzz.py`) | every `warren.*` function with generated and hostile arguments, as four roles, under a hostile `search_path`, in parallel plans and rolled-back transactions; fails on crashes, internal errors, privilege leaks and leftover state | PG 16 |
| Miri (`cargo miri test -p warren-core`) | undefined behaviour in the domain crate and its dependencies' unsafe code | nightly on `main`, and on release pull requests |
| Upgrades (`test/upgrade.py`) | `ALTER EXTENSION … UPDATE` from every release gives the catalog of a fresh install, also by a non-superuser | PG 15–18 |
| Dumps (`test/dump.py`) | plain and custom dumps restore with graphs, versions and vectors; a trusted installer's dump without catalog data | PG 15–18 |
| Crash recovery (`test/crash_recovery.py`) | refreshes stopped by termination, `SIGKILL` and immediate shutdown | PG 16 |
| Replicas (`test/replicas.py`) | hot and cascading standbys read, refuse writes with 25006, and promote | PG 16 |
| Concurrency (`test/concurrent_refresh.py`) | many backends reading, computing and refreshing one graph | PG 16 |
| Pooler soak (`test/pooler_soak.py`) | PgBouncer in transaction and session mode, 2 minutes: memory growth, leftover state, session isolation | PG 16 (a 2.5-hour run is recorded in DESIGN.md §13, M11) |
| Lints | `rustfmt`; clippy and rustdoc with `-D warnings`; no `unwrap`/`expect`/`panic!`; documented `unsafe`; no SPI call that assigns a transaction id on a read path | every CI run |

## Supply chain and releases

- **Toolchain pins.** The Rust toolchain is pinned exactly in
  `rust-toolchain.toml`; `pgrx` and `cargo-pgrx` are pinned to `=0.19.3` and
  upgraded together in their own change; `Cargo.lock` is committed; CI's
  third-party actions are pinned by commit hash, and the Miri and fuzzing
  nightly by date.
- **Frozen schema.** A released install script never changes: its SHA-256 is
  recorded in `sql/released.sha256` and verified before publishing, and each
  later schema change ships an upgrade script that `test/upgrade.py` checks.
- **Release builds** (`release-publish.yml`; CONTRIBUTING.md, "Packages")
  run in jobs with read-only repository access and no secrets, in
  distribution containers pinned by digest. Each `.deb` is installed into a
  clean container and smoke-tested. After a person approves the protected
  `release` environment, a publish job that runs no build code checks each
  package's contents and metadata, pushes the annotated tag with a deploy
  key, writes `SHA256SUMS` and signs it (`SHA256SUMS.asc`), and assembles and
  signs the apt repository. The signing key's fingerprint is stated in the
  README and checked by the job; a signed apt snapshot is then published to
  GitHub Pages behind its own approval.
- **What is published**: `.deb` packages for PG 15–18 on Debian 12 and 13 and
  Ubuntu 22.04 and 24.04 (amd64, arm64), tarballs, Docker images on GHCR, a
  Homebrew formula and the source tarball (README, "Install").

**Not yet:**

- Reproducible builds: rebuilding a release does not give the same bytes, so
  a provider building from source cannot compare its build with ours.
- An SBOM attached to releases, and licence and advisory checks of
  dependencies in CI (`cargo-deny`). Dependencies are not vendored, and the
  package build fetches crates from crates.io.
- RPM packages, a PGXN release, and inclusion in the PGDG repositories.
- macOS as a supported platform.

These are tracked in [#103](https://github.com/ExaDev/pg_warren/issues/103)
("Supply chain").

## Reporting a vulnerability

pg_warren does not yet publish a security policy (`SECURITY.md`) or a
private channel for reporting vulnerabilities. This is a known gap.

## Known gaps, together

- Resource settings other than `warren.shared_cache_memory` can be raised by
  any role; instance-wide ceilings are planned for 0.9.0.
- A non-superuser's `pg_dump` fails while pg_warren is installed; the
  predefined-role route is checked only by hand (§14 item 51).
- A role can learn that a graph name exists, and its owner's role name, by
  calling a function on it.
- Small Rust allocations are not fallible; only graph-sized buffers are.
- No reproducible builds, SBOM, dependency audit or security policy yet.
