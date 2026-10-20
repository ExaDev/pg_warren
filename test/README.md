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
- `harness/` is the Python behind both; `tests/` are its pytest tests.

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
