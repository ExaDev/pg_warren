# pg_warren

Deep traversal and whole-graph computation for PostgreSQL, over graphs whose
nodes and edges live in your ordinary tables.

pg_warren builds a compact, memory-resident CSR adjacency structure from two
SQL queries (one for nodes, one for edges) and exposes it through `warren.*`
functions: BFS, reachability, shortest paths, typed path walks, semiring
SpMV, PageRank, label propagation, a Lanczos eigensolver, and distances
between graph versions.

It is not a graph database and has no query language. Your tables stay the
source of truth, and `warren.*` calls compose with ordinary SQL on either
side: node ids go in, relations keyed by node id come out, and you join them
back. Think "pgvector for graphs": an index plus compute engine beside your
data, not a replacement for it.

Every build is an immutable **version** with stable dense ids, stored
transactionally in extension tables. A version can be built inside the
transaction that changes the data, compared with the current one, and
committed or rolled back with it.

## The principle: meet Postgres users where they are

pg_warren is an escape valve for graph workloads that Postgres alone handles
badly, without leaving Postgres or fighting it. Your data stays in your
tables. Vertex programs are SQL: PL/pgSQL loops over whole-graph calls, with
per-node logic as SQL expressions. Transactions, backups and replicas cover
the graph like everything else, because its versions are ordinary rows.

Pregel-style computation (`gather`, the algorithm library, PL/pgSQL loops
over `spmv` and `vector_map`) is the first-class way in. Sparse matrix
algebra beyond `spmv`/`spmm` (SpGEMM, sparse vectors, and CPU and later GPU
backends) is **planned** as the power-user path; it is not there yet.

## Install

pg_warren supports PostgreSQL 15, 16, 17 and 18 (16 is the development
baseline). `CREATE EXTENSION pg_warren` needs a superuser.

**From source**, with Rust (the toolchain pinned in `rust-toolchain.toml`),
the server headers (`postgresql-server-dev-NN`), `clang`/`libclang-dev`, and
cargo-pgrx at exactly the version the extension uses:

```sh
cargo install cargo-pgrx --version 0.19.3 --locked
cargo pgrx init --pg16 "$(command -v pg_config)"
cd crates/pg_warren
cargo pgrx install --release --pg-config "$(command -v pg_config)"
```

**Docker**: `docker/Dockerfile` builds the extension for one major and
installs it into the official `postgres` image for that major. No image is
published yet; build it locally:

```sh
docker build -f docker/Dockerfile --build-arg PG_MAJOR=17 -t pg_warren:pg17 .
docker run --rm -e POSTGRES_PASSWORD=postgres -p 5432:5432 pg_warren:pg17
```

Then, in each database:

```sql
CREATE EXTENSION pg_warren;
SELECT warren.version();
```

**Upgrading** (from 0.1.0): install the new build, then run
`ALTER EXTENSION pg_warren UPDATE;` in every database that has the extension,
*before* calling any `warren.*` function. The new library is loaded at once,
but the catalog changes only with the update, so calls in between can fail.

## A ten-line example

```sql
CREATE TABLE people  (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE follows (src bigint NOT NULL, dst bigint NOT NULL);
INSERT INTO people  VALUES (1, 'ada'), (2, 'bob'), (3, 'cy'), (4, 'dee'), (5, 'eve');
INSERT INTO follows VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5);

SELECT warren.create_graph('social', 'SELECT id FROM people', 'SELECT src, dst FROM follows');
SELECT warren.refresh('social');                                   -- builds version 1

SELECT p.name, b.depth                                             -- who ada reaches in 3 hops
FROM warren.bfs('social', ARRAY[1::bigint], 3) b
JOIN people p ON p.id = b.node_id ORDER BY b.depth, p.name;
--  ada 0 | bob 1 | cy 2 | dee 3

SELECT warren.pagerank('social', 'pr');                            -- writes vector 'pr'
SELECT p.name, round(r.value::numeric, 3) AS rank
FROM warren.vector_topk('social', 'pr', 3, NULL::bigint) r
JOIN people p ON p.id = r.node_id ORDER BY rank DESC, p.name;
--  cy 0.243 | bob 0.209 | eve 0.209
```

The graph's ids are `bigint`, so id arguments are cast (`1::bigint`), and
functions with no id argument take a type witness (`NULL::bigint`) for their
result type. A traversal reads the version, not the tables: rows inserted
after `refresh` appear after the next `refresh`
([docs/semantics.md](docs/semantics.md)).

## When to use it: frontier size, not hop count

A graph query has two forms, and you choose per query:

- **Shallow, small frontier:** a recursive CTE or self-joins over your
  `nodes`/`edges` tables. Fully transactional, planner-optimised, never
  stale. This is the default and covers most queries.
- **Deep, large frontier:** one `warren.*` call, joined back to your tables
  for filtering and projection. Reads a snapshot (a version).

The criterion is **how many rows the traversal touches, not how many hops it
takes.** Three hops at average degree 10 is about a thousand intermediate rows
and belongs in SQL; three hops at degree 1,000 is a billion and belongs in
the engine. Rule of thumb: if `EXPLAIN` of the CTE shows the recursive term
producing more than about 10⁵–10⁶ rows, or the computation is iterative at
all (PageRank, components, propagation), move it into pg_warren. Both forms
produce a relation keyed by node id, so switching changes one clause, not the
surrounding query.

## The SQL surface at a glance

Every function takes the graph name first and, where it reads a graph, an
optional `version` (default: the current version).

| Area | Functions |
|---|---|
| Graphs and versions | `create_graph`, `refresh`, `promote`, `drop_version`, `compact`, `diff`, `info`, `drop_graph`, `grant`, `revoke`, `prewarm`, `evict` |
| Traversal | `bfs`, `neighbours`, `degree`, `has_edge`, `reachable`, `reachable_set`, `shortest_path`, `dijkstra`, `components`, `walk` with `warren.s` / `warren.step`, `last_stats` |
| Vectors | `vector_from_query`, `vector_const`, `vector_degree`, `vector_dense_id`, `vector_values`, `matrix_rows`, `vector_topk`, `vector_stats`, `id_map`, `drop_vector` |
| Compute | `spmv`, `spmm`, `vector_map`, `explain_map`, `vector_reduce`, `vector_norm_diff`, `gather` |
| Library | `pagerank`, `components_vector`, `label_propagation`, `sssp_vector`, `degree_vector`, `aggregate_neighbours`, `triangles` |
| Spectral | `eigen`, `distance` |

Signatures are in `\df warren.*` and in DESIGN.md (§6, §8, §9.3).

## Documentation

- [docs/semantics.md](docs/semantics.md): graphs, versions, dense ids,
  snapshots and staleness, transactions, retention, access, id types.
- [docs/compute.md](docs/compute.md): vectors, semiring products, masks,
  `vector_map`, `gather`, the algorithm library, `eigen` and `distance`.
- [docs/change-detection.md](docs/change-detection.md): measuring structural
  change before you commit it, in one transaction or with a shadow graph.
- [docs/limits.md](docs/limits.md): settings, memory, measured performance,
  known limitations.
- [DESIGN.md](DESIGN.md): the full design and the reasons for it.

Every SQL example in the README and `docs/` runs in CI, in
`test/sql/docs_examples.sql`.

## Status

- **0.1.0** (released): versioned graphs and traversal, milestones M0–M3
  of DESIGN.md §13.
- **0.2.0** (milestones M4–M6, all landed; release being cut): vectors,
  semiring compute, `gather`, the algorithm library, `eigen` and
  `distance`.
- **0.3.0** (in progress, M7): hardening, including a size bound on the
  backend-local cache (planned, not shipped) and these docs.

See [CHANGELOG.md](CHANGELOG.md). This is pre-1.0 software: the SQL surface
may still change between minor versions, with an upgrade script each time.

Not there yet: a size bound on the backend-local cache (planned for 0.3.0),
a shared-memory cache across backends, incremental maintenance, contraction
(community-level graphs), and text ids ([docs/limits.md](docs/limits.md)).

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
- Contraction gives community-level computation (planned: DESIGN.md §6.6).

pg_warren takes no code from either project. Ideas adopted from them are
credited in [NOTICE](NOTICE) and in comments where they are used; the reading
is recorded in [docs/prior-art-notes.md](docs/prior-art-notes.md).

## Building and contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE).
