# sql/

- `pg_warren--<version>.sql`: the extension script pgrx generates, committed
  so that schema changes are deliberate. `sql/schema-drift.sh` (run in CI)
  fails when the generated SQL differs from it, ignoring pgrx's unstable
  entity order and its comments; `sql/schema-drift.sh --update` regenerates
  it.
- Hand-written SQL that pgrx does not generate: extension upgrade scripts
  (`pg_warren--<from>--<to>.sql`, from the first release on) and SQL-language
  library functions such as the PL/pgSQL algorithm library (DESIGN.md §6.7).
