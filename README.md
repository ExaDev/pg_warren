# pg_warren

Index-free-adjacency traversal and whole-graph computation for PostgreSQL.

`pg_warren` is a PostgreSQL extension that builds a compact, memory-resident
CSR adjacency structure from graphs whose nodes and edges live in ordinary
Postgres tables, and exposes it through SQL functions for deep traversal (BFS,
reachability, shortest paths, typed path walks) and whole-graph compute
(semiring SpMV, PageRank, label propagation, spectral analysis). Builds are
versioned snapshots with stable dense node IDs, so versions can be compared
for structural change.

It is not a graph database and has no query language: your tables stay the
source of truth, and `warren.*` functions compose with ordinary SQL on either
side. Think "pgvector for graphs".

See [DESIGN.md](DESIGN.md) for the full design.

## Status

- **0.1.0**: versioned graphs and traversal, milestones M0–M3 of
  DESIGN.md §13.
- **0.2.0**: vectors, semiring compute, `gather`, the algorithm library,
  `eigen` and `distance`, milestones M4–M6.

See [CHANGELOG.md](CHANGELOG.md). This is pre-1.0 software: the SQL surface
may still change between minor versions, with an upgrade script each time.

Targets PostgreSQL 15–18 (16 is the development baseline).

## Positioning vs prior art

pg_warren is a graph index and compute engine for PostgreSQL. It derives a
compressed-sparse-row adjacency structure from your ordinary tables and runs
deep traversals and whole-graph linear algebra over it from SQL. It is not the
first extension to put a CSR beside Postgres tables.
[pgGraph](https://github.com/evokoa/pggraph) pioneered derived, rebuildable CSR
traversal behind SQL functions, and
[OneSparse](https://github.com/OneSparse/OneSparse) brought the GraphBLAS
semiring model into Postgres as datatypes.

pg_warren's contribution is different:
- Every build is a versioned snapshot with a stable dense-ID space.
- Builds are stored transactionally in ordinary tables, so they are WAL-logged,
  replicated, and rolled back with your transaction.
- A version can be built *inside* the transaction that changes the data and
  measured against the current one, structurally or spectrally, before you
  commit.
- One CSR serves both traversal and semiring compute (with GraphBLAS operation
  names).
- Control flow stays in PL/pgSQL, with per-node logic written as SQL
  expressions.
- Contraction (planned, v1.1) will give community-level computation.

pg_warren takes no code from either project. Ideas adopted from them are
credited in [NOTICE](NOTICE) and in comments where they are used; the reading
is recorded in [docs/prior-art-notes.md](docs/prior-art-notes.md).

## Building and contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE).
