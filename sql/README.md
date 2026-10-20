# sql/

- `pg_warren--<version>.sql`: the extension script pgrx generates, committed
  so that schema changes are deliberate. `sql/schema-drift.sh` (run in CI)
  fails when the generated SQL differs from the version in development's
  file, ignoring pgrx's unstable entity order and its comments;
  `sql/schema-drift.sh --update` regenerates it. Released versions' files
  are frozen: `released.sha256` lists them, and the drift check fails if one
  changes.
- `pg_warren--<from>--<to>.sql`: hand-written upgrade scripts, one from each
  release towards the version in development, updated in the same commit as
  every schema change; `test/upgrade.py` checks them.
- pg_warren is a trusted extension: a non-superuser's `CREATE` or `ALTER
  EXTENSION` runs these scripts as the bootstrap superuser. Create objects
  only in schema `warren`, schema-qualify names, and use only exact-match
  `pg_catalog` operators and functions in anything evaluated while a script
  runs (DESIGN.md §7).
- Other hand-written SQL that pgrx does not generate: SQL-language library
  functions such as the PL/pgSQL algorithm library (DESIGN.md §6.7).
