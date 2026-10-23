# CLAUDE.md — engineering conventions for agents working on pg_warren

Read `DESIGN.md` (the spec; §13 milestones, §14 open decisions, §15 rules)
before changing code. Commands, toolchain setup and environment gotchas are in
`CONTRIBUTING.md`. Do not duplicate them here.

## Non-negotiables

1. **TDD: red, green, refactor.** Write the failing test first and *run it to
   see it fail for the expected reason* before you write the implementation.
   Then write the minimum code to pass it, then refactor with the tests green.
   Bug fixes start with a test that reproduces the bug.
2. **Ports and adapters (hexagonal).**
   - `crates/warren-core` is the domain (the hexagon). It has **no Postgres
     dependency**: no pgrx, no pg_sys, no SPI types. It takes slices and a
     thread count, and returns values. CI fails if `cargo tree -p warren-core`
     mentions pgrx/pg-sys (DESIGN.md §15.4).
   - `crates/pg_warren` is the adapter layer. It stays thin: it translates
     between Postgres (SPI, datums, GUCs, memory contexts, interrupts) and
     domain calls. Graph logic belongs in `warren-core`.
   - Inside `pg_warren`, a feature talks to Postgres through **narrow traits
     (ports)** that the feature defines, for example a `SourceReader` for
     node/edge rows or a `SegmentStore` for chunked bytea, with an SPI-backed
     adapter and an in-memory fake. Logic behind a port is unit-tested against
     the fake with plain `#[test]`. Only the SPI adapter and the SQL surface
     need `#[pg_test]`.
   - Only the main thread may call into Postgres (DESIGN.md §6.9). Worker
     threads touch only extension-owned buffers.
3. **Package by feature, named in the domain language.** Modules are domain
   concepts, not technical layers. No `utils`, `helpers`, `common`, `models`,
   `types` or `misc` modules.
   - `warren-core/src/`: `graph/` (csr, id_map, tombstones, relation
     selection), `interrupts` (the cancellation poll shared by traversal and
     compute), `traversal/`, `compute/` (semiring, kernels, message),
     `community/` (labels, contraction, projection, modularity, local
     moving, refinement), `spectral/`,
     `version/` (diff, compaction across versions).
   - `pg_warren/src/`: `extension` (schema + `warren.version()`, SQL
     errors), `access/` (identity switches, privilege checks), `source/`
     (node/edge query columns and read-only cursors), `catalog/`,
     `segment/` (chunked persistence), `cache/` (loader; `shared`, the shared cache across backends, and `shared::introspection`, `warren.shared_cache()`), `build/`,
     `version_sql/` (compact, diff), `interrupts` (the main-thread poll),
     `read_only` (single-row SPI reads that assign no transaction id),
     `parallel_worker` (what a parallel worker records and raises),
     `external_ids/` (id arguments ↔ dense ids), `selection/` (direction and
     relations), `traversal_sql/`, `vector/` (persistence and cache of named
     vectors), `compute_sql/`, `spectral_sql/` (eigen, distance),
     `community_sql/` (contract, project, lift, modularity, local moving,
     and the Louvain/Leiden library).
     Feature dependencies form a DAG: `access`, `source` → `catalog` →
     `segment` → `vector` → `cache` → `build` → `version_sql`; `cache` →
     `external_ids`, `selection`; and `external_ids`, `selection`,
     `interrupts` → `traversal_sql`, `compute_sql` (siblings: neither depends
     on the other); `compute_sql` → `spectral_sql`; `build`, `compute_sql`
     → `community_sql`;
     `parallel_worker` → `cache`, `traversal_sql`; never the reverse.
   - Each module's `mod.rs` doc comment states its responsibility. Add a
     submodule when the code exists, not before.
4. **Ubiquitous language (from DESIGN.md).** Use these terms exactly, in code,
   tests and docs: *graph, version (staged / promoted / current), dense id,
   external id, id map, id space, tombstone, segment, chunk, relation, CSR
   (offsets / targets / weights, forward / reverse / merged), frontier, step,
   walk, vector, matrix, semiring, message, combiner, gather, label,
   dropped edges, deduplicated edges, community, quotient, membership,
   parent version, label vector, level, modularity, local moving,
   refinement.* *Merged* means only the all-relations
   CSR; parallel edges folded at build time are *deduplicated*. A
   *community label* is a node's value in a label vector; a version's
   *label* is its `versions.label` text: say which. Don't invent
   synonyms ("node index", "snapshot id", "edge type"). Don't use "version"
   for anything except a graph version or a software version, and say which.
5. **Small, focused files and functions. No dead code**: nothing unused,
   commented out or speculative. Empty feature modules may hold only their
   responsibility doc comment.
   Lints are set once in `[workspace.lints]` (root `Cargo.toml`). Every crate
   opts in, and a lint is never allowed crate-wide. If a macro trips a lint,
   allow it on the smallest possible item and add a comment saying why.
6. **Main stays green; CI is the gate.** Every commit passes fmt, clippy
   `-D warnings`, rustdoc `-D warnings`, the core tests, the core-isolation
   check and pgrx tests on PG 15–18. Don't merge on red, and don't weaken a
   check to get green. Miri runs on pushes to main, not on pull requests; a
   Miri failure on main is fixed before anything else merges.
7. **Keep DESIGN.md true.** When you make a §14 decision or change a design
   detail, update DESIGN.md in the same change (§15.5).

## Test tiers

| Tier | Where | Runs with |
|---|---|---|
| Domain unit + property (`proptest`) | `warren-core`, `#[cfg(test)]` next to the code | `cargo test -p warren-core` |
| Adapter logic behind ports, against fakes | `pg_warren`, plain `#[test]` | `cargo pgrx test pgNN` (crate needs the pgrx harness) |
| In-database (`#[pg_test]`): SQL surface, SPI adapters, transactions | `pg_warren`, `#[pg_schema] mod tests` per feature module | `cargo pgrx test pgNN` |
| Integration / oracle / Appendix A use cases | `test/sql/` (+ generators in `bench/`) | `test/run.py` (and `pytest` for the generators) |
| Domain tests under Miri | `warren-core` (same tests) | `cargo +$MIRI_NIGHTLY miri test -p warren-core` (pinned nightly, see `ci.yml`; CI runs it on main only) |
| Micro-benchmarks (`criterion`) | `warren-core/benches/` | `cargo bench -p warren-core` |

## pgrx specifics (learned in M0)

- Schema `warren` is declared once, as `#[pg_schema] mod warren {}` in
  `pg_warren/src/extension.rs`. Put SQL functions in it from any feature
  module with `#[pg_extern(schema = "warren")]`. Do **not** add
  `schema = warren` to `pg_warren.control`: Postgres would pre-create the
  schema outside the extension, and the script's `CREATE SCHEMA IF NOT EXISTS`
  then fails with "schema warren is not a member of extension"
  (CVE-2022-2625 hardening).
- **Releases are frozen.** Released `sql/pg_warren--X.Y.Z.sql` files
  (listed in `sql/released.sha256`) never change. Any schema change after a
  release also updates `sql/pg_warren--<released>--<current>.sql` in the
  same commit; `test/upgrade.py` checks it (CONTRIBUTING.md, "Releases and
  upgrades").
- `#[pg_test]` names are prefixed `pg_` in test output. Running them as root
  (`--runas`) and their `Result` return convention are covered in
  CONTRIBUTING.md.
- **Errors and panics** (DESIGN.md §6.9, §11; lessons from pgGraph):
  - Raise SQL errors only through pgrx (`ErrorReport` or `ereport!` with a
    standard SQLSTATE). Never call `pg_sys` error functions directly.
  - Never `catch_unwind` around SPI or Postgres calls.
  - Use `pgrx::check_for_interrupts!()`. pgrx's `pg_sys` FFI is
    `#[pg_guard]`ed, so an ERROR unwinds the Rust frames. Never call unguarded
    FFI.
  - `unwrap`/`expect`/`panic!` and undocumented `unsafe` are lint errors
    (policy in CONTRIBUTING.md "Lints").
- Reserve graph-sized buffers with `try_reserve`; an allocation failure is a
  SQL error, never an abort.
- SPI writes need `Spi::connect_mut` + `update()`. `Spi::connect` is read-only.
- **Reads never assign a transaction id.** pgrx's `Spi::get_one*`,
  `get_two*` and `run*` go through `update()`, which calls
  `GetCurrentTransactionId()`; in parallel mode (a `PARALLEL RESTRICTED`
  function in the leader of a parallel plan) that raises "cannot assign
  XIDs during a parallel operation". Outside tests, read with
  `read_only::read_one`/`read_two` or `Spi::connect` + `select`, and write
  with `Spi::connect_mut` + `update`. Clippy's `disallowed-methods`
  (`clippy.toml`) enforces this; test modules allow it on the `mod` item.
- **Parallel labels** (DESIGN.md §9.3): writers and anything that may persist
  are `PARALLEL UNSAFE`; anything that reads a graph or vector is `PARALLEL
  RESTRICTED`; only functions that read no graph are `PARALLEL SAFE`.
  `parallel_labels`'s test declares every function's label; add each new
  function there (and its read to `READS` if it is not unsafe).
- **Every SQL entry point uses the guard.** The body of every
  `#[pg_extern]` is `access::entry(|caller| ...)`, which pins
  `search_path = pg_catalog, pg_temp` for the whole call and raises errors
  with their SQLSTATE (DESIGN.md §7). Write SPI SQL assuming that
  `search_path`: schema-qualify `warren.*` objects, never rely on `public`.
  Run user-supplied SQL only under the search_path it belongs to
  (`access::with_search_path`: the caller's, or a graph's stored one).
  `access::guard`'s test calls every `warren.*` function with a hostile
  `search_path`; add each new function to its `CALLS` list.
- Use pgrx `=0.19.3` (pinned). Upgrade pgrx and cargo-pgrx together, in their
  own change.

## Git

Small commits with clear messages. Don't push or open PRs unless your
coordinator or user asks.
