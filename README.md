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

**Pre-alpha.** Milestone M0 (skeleton): the extension installs and exposes
`warren.version()`. Nothing else works yet.

```sql
CREATE EXTENSION pg_warren;
SELECT warren.version();
```

Targets PostgreSQL 15–18 (16 is the development baseline).

## Positioning vs prior art

TODO — what pg_warren adds over pgGraph and OneSparse, and credits for ideas
taken from them (DESIGN.md §4A).

## Building and contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE).
