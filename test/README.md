# test/

Integration tests that cannot be expressed as `#[pg_test]` in
`crates/pg_warren`: multi-transaction scenarios, oracle suites against
recursive CTEs / NetworkX on generated fixtures, and the Appendix A use cases
(DESIGN.md §12). How to run them is in CONTRIBUTING.md.

- `run.py` installs pg_warren (`cargo pgrx install`, under
  `/tmp/pgrx-test.lock`), starts a scratch cluster and runs `sql/*.sql`.
- `sql/` holds the tests; `sql/include/` holds scripts they `\ir`.
- `lib/assert.sql` is the `assert` schema every test database gets.
- `oracle.py` writes a NetworkX reference table into any database.
- `upgrade.py` checks `ALTER EXTENSION pg_warren UPDATE` from every released
  version against a fresh install of the version in development (with
  `--trusted`, installed and updated by a non-superuser).
- `dump.py` checks that a `pg_dump` of graphs, versions and vectors
  restores (plain through psql, custom through pg_restore), and that a
  trusted install's installer can dump and restore without the catalog.
- `concurrent_refresh.py` runs many backends reading, computing on and
  refreshing one graph at once for a bounded time, preloaded and not, and
  checks every read against the oracle of the source generation its
  version was built from, every failure's SQLSTATE, what is left once
  they disconnect, and the server log (DESIGN.md §13, M11).
- `crash_recovery.py` stops refreshes mid-way (waiting to read edges,
  writing segments, before commit) by `pg_terminate_backend`, SIGKILL
  and an immediate shutdown, and checks that each recovery leaves the
  catalog and the graph's answers as before, the shared cache working
  and a new refresh succeeding (DESIGN.md §13, M11).
- `replicas.py` starts a primary, a physical streaming hot standby of it
  (`pg_basebackup -R`) and a cascading standby of the standby, builds
  graphs, versions and vectors on the primary, and checks on each
  standby, once it has replayed them, that every read answers as on the
  primary, that every write is refused with 25006, and that the shared
  cache and the activity table work; that a version retired on the
  primary under a standby query is read consistently or the query
  cancelled by the recovery conflict (40001), with and without
  `hot_standby_feedback`; and that the standby, promoted, writes and its
  cascade follows it (DESIGN.md §5.1, §13, M11).

- `sql_fuzz.py` calls every `warren.*` function (read from `pg_proc`)
  with generated arguments, valid ones from the fixture's graphs,
  versions, vectors and node ids (id arrays as literals or read from a
  table column, short or compressed and out of line) mixed with hostile
  ones (NULLs, extremes, NaN, wrong types, huge arrays, injections, SQL
  that writes or reads a secret), as the superuser, an owner, a reader and
  a stranger, in transactions and savepoints that roll back at random,
  under `SET LOCAL`s that bound memory and time or force parallel plans,
  and with a hostile `search_path`. It fails on a crash, a hang, an
  internal error or any SQLSTATE outside the documented classes, an
  allowed one about the extension's own objects (a privilege error on
  its schema or catalog, a missing `warren.*` relation), a call that
  succeeds beyond its caller's privileges, an action that changed a graph
  its role does not own or another role's vector (the catalog compared
  before and after), a hostile operator run elevated, a leak (a pin
  between statements, an activity row of a backend that has gone), a
  catalog left inconsistent or not empty once every graph is dropped, and
  trouble in the server log. Seeded: each failure is written, with its
  seed, as a psql script that reproduces it from a fresh cluster, the
  first `--max-shrinks` (3) of a run shrunk first
  (`harness/sql_fuzz/`; DESIGN.md §13, M11).

  None of them hangs: each of their sessions has a 120 s
  `statement_timeout` (the fuzzer's, 3 s, and a watchdog), a session still busy when it is closed is killed,
  and worker threads that do not stop in time are reported, not waited
  for.
- `pooler_soak.py` puts PgBouncer in front of a scratch cluster in
  transaction and session pooling, many clients over a few server
  connections, and runs a mix of reads, compute, vectors, prepared
  statements, timeouts, cancels, abandoned transactions and writes that
  retire versions for `--minutes` (CI: 2) or `--hours`. It samples each
  pooled backend's memory (`smaps_rollup`, memory contexts,
  `warren.backends()`), the shared cache, the DSM segments and the catalog,
  and fails on growth in the steady state (a robust slope over the window
  and over each half), on anything left behind once clients and poolers
  stop, on an undocumented SQLSTATE and on session state that session
  pooling loses or leaks; what transaction pooling loses and leaks is
  reported (`harness/pooler_soak/`; DESIGN.md §13, M11).
- `harness/` is the Python behind them; `tests/` are its pytest tests.

## Writing a SQL test

A test is `sql/<name>.sql`, run by psql (`ON_ERROR_STOP`) in a fresh
database `test_<name>` that has `pg_warren` and the `assert` schema. It passes
when psql exits 0, so any error fails it. Assert with:

```sql
SELECT assert.that(condition, 'message');
SELECT assert.equal(actual, expected, 'message');          -- IS NOT DISTINCT FROM
SELECT assert.raises($$statement$$, 'message', '22023');  -- SQLSTATE optional
```

Directives in the leading comment block prepare the database:

```sql
-- fixture: sbm --communities 4 --community-size 50 --seed 1
-- oracle: components
-- oracle: pagerank --damping 0.85 --directed --edges-sql 'SELECT src, dst FROM edges'
```

`fixture:` takes `bench/generate.py` arguments (without `--out`); its tables
and a `truth (key, value jsonb)` table are loaded before the test (at most
one fixture per test). Each `oracle:` line exports the graph (default
`SELECT id FROM nodes` / `SELECT src, dst FROM edges`, undirected) and writes
`oracle_<computation>` (see `harness/oracle.py`).

Transactions are the test's own: `BEGIN` … `ROLLBACK`/`COMMIT` in the file
are real top-level transactions. Use psql's `\gset` to carry values (version
ids) between statements, and `\ir include/<file>.sql` to repeat a block.
The test runs as the cluster superuser `postgres`, which can read the
`warren.*` catalog tables directly. With `run.py --trusted`, a non-superuser
(`warren_installer`) owns each test database and runs its
`CREATE EXTENSION`, as in a trusted install on hosted Postgres
(DESIGN.md §7). The test itself still runs as `postgres`. Tests must pass
both ways.
