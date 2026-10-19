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
   - `warren-core/src/`: `graph/` (csr, id_map, tombstones), `traversal/`,
     `compute/` (semiring, kernels, message), `spectral/`.
   - `pg_warren/src/`: `extension` (schema + `warren.version()`), `catalog/`,
     `build/`, `cache/`, `traversal_sql/`, `compute_sql/`.
   - Each module's `mod.rs` doc comment states its responsibility. Add a
     submodule when the code exists, not before.
4. **Ubiquitous language (from DESIGN.md).** Use these terms exactly, in code,
   tests and docs: *graph, version (staged / promoted / current), dense id,
   external id, id map, id space, tombstone, segment, chunk, relation, CSR
   (offsets / targets / weights, forward / reverse), frontier, step, walk,
   vector, matrix, semiring, message, combiner, gather, label.* Don't invent
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
   check to get green.
7. **Keep DESIGN.md true.** When you make a §14 decision or change a design
   detail, update DESIGN.md in the same change (§15.5).

## Test tiers

| Tier | Where | Runs with |
|---|---|---|
| Domain unit + property (`proptest`) | `warren-core`, `#[cfg(test)]` next to the code | `cargo test -p warren-core` |
| Adapter logic behind ports, against fakes | `pg_warren`, plain `#[test]` | `cargo pgrx test pgNN` (crate needs the pgrx harness) |
| In-database (`#[pg_test]`): SQL surface, SPI adapters, transactions | `pg_warren`, `#[pg_schema] mod tests` per feature module | `cargo pgrx test pgNN` |
| Integration / oracle / Appendix A use cases | `test/` | TBD (M2+) |
| Domain tests under Miri | `warren-core` (same tests) | `cargo +nightly miri test -p warren-core` |
| Micro-benchmarks (`criterion`) | `warren-core/benches/` | `cargo bench -p warren-core` |

## pgrx specifics (learned in M0)

- Schema `warren` is declared once, as `#[pg_schema] mod warren {}` in
  `pg_warren/src/extension.rs`. Put SQL functions in it from any feature
  module with `#[pg_extern(schema = "warren")]`. Do **not** add
  `schema = warren` to `pg_warren.control`: Postgres would pre-create the
  schema outside the extension, and the script's `CREATE SCHEMA IF NOT EXISTS`
  then fails with "schema warren is not a member of extension"
  (CVE-2022-2625 hardening).
- `#[pg_test]` names are prefixed `pg_` in test output. Clippy does not see
  them as tests, so they return `Result<(), pgrx::spi::Error>` and use `?`
  rather than `unwrap`.
- pgrx tests can't run as root. Use `--runas postgres --pgdata <dir owned by
  postgres>` (see CONTRIBUTING.md).
- **Errors and panics** (DESIGN.md §6.9, §11; lessons from pgGraph):
  - Raise SQL errors only through pgrx (`ErrorReport` or `ereport!` with a
    standard SQLSTATE). Never call `pg_sys` error functions directly.
  - Never `catch_unwind` around SPI or Postgres calls.
  - Wrap interrupt checks so that a Postgres ERROR unwinds the Rust frames.
  - No `unwrap`, `expect` or `panic!` outside tests.
  - Every `unsafe` block carries a `// SAFETY:` comment.
- Reserve graph-sized buffers with `try_reserve`; an allocation failure is a
  SQL error, never an abort.
- SPI writes need `Spi::connect_mut` + `update()`. `Spi::connect` is read-only.
- Use pgrx `=0.19.3` (pinned). Upgrade pgrx and cargo-pgrx together, in their
  own change.

## Git

Small commits with clear messages. Don't push or open PRs unless your
coordinator or user asks.
