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
side: node ids go in, rows keyed by node id come out, and you join them
back. Think "pgvector for graphs": an index plus compute engine beside your
data, not a replacement for it.

Every build is an immutable **version** with stable dense ids, stored
transactionally in extension tables. A version can be built inside the
transaction that changes the data, compared with the current one, and
committed or rolled back with it.

## The principle: meet Postgres users where they are

pg_warren is an escape valve for graph workloads that Postgres alone handles
badly, without leaving Postgres or fighting it. Your data stays in your
tables. Node programs are SQL: PL/pgSQL loops over whole-graph calls, with
per-node logic as SQL expressions. Transactions, backups and replicas cover
the graph like everything else, because its versions are ordinary rows.

Pregel-style computation (`gather`, the algorithm library, PL/pgSQL loops
over `spmv` and `vector_map`) is the first-class way in. Sparse matrix
algebra beyond `spmv`/`spmm` (SpGEMM, sparse vectors, and CPU and later GPU
backends) is **planned** as the power-user path; it is not there yet.

## Install

pg_warren supports PostgreSQL 15, 16, 17 and 18 (16 is the development
baseline). `CREATE EXTENSION pg_warren` does not need a superuser (below).

**From source**, with Rust (the toolchain pinned in `rust-toolchain.toml`),
the server headers (`postgresql-server-dev-NN`), `clang`/`libclang-dev`, and
cargo-pgrx at exactly the version the extension uses:

```sh
git checkout v0.4.0   # the latest release; main is the version in development
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

pg_warren is a *trusted* extension, so this does not need a superuser: any
role with `CREATE` on the database can run it, as on hosted Postgres. That
role becomes the extension's owner. It may update the extension
(`ALTER EXTENSION pg_warren UPDATE`) or drop it, which drops every graph in
the database. It gets no direct access to the `warren.*` tables, which belong
to the bootstrap superuser. Every role, the installer included, reaches
graphs only through the `warren.*` functions and their access checks
(DESIGN.md §7).

On PG 15+ the database owner also owns schema `public`. A graph's source
queries run as the graph's owner, under the `search_path` it was created
with. If that path includes a schema another role can create in, such as
`public` when someone else owns the database, that role could add a function
or operator that the queries then call as the graph's owner at every
`refresh`. Schema-qualify the functions and operators in source queries, or
create graphs under a `search_path` of schemas only their owner can write;
`create_graph` raises a `NOTICE` when it finds such a schema.

Graphs, versions and vectors are ordinary tables, so a superuser's
`pg_dump` includes them, and a plain (`psql`) or custom (`pg_restore`)
restore brings them back. The roles that own and read graphs must exist
first. `pg_dump` warns about circular foreign-key constraints on
`warren.versions` (it references itself) and suggests `--disable-triggers`:
ignore that hint, which would skip pg_warren's catalog checks, and restore
normally. Because only that superuser can read the catalog, a non-superuser's
`pg_dump` of the database fails while pg_warren is installed. Dump as a
superuser, or leave the graphs out with `--exclude-table-data='warren.*'`
and, after the restore, create them again with `warren.create_graph` and
`warren.refresh`.

**Upgrading** from an earlier release: install the new build, then run
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
  for filtering and projection. Reads a version, not the live tables.

The criterion is **how many rows the traversal touches, not how many hops it
takes.** Three hops at average degree 10 is about a thousand intermediate rows
and belongs in SQL; three hops at degree 1,000 is a billion and belongs in
the engine. Rule of thumb: if `EXPLAIN` of the CTE shows the recursive term
producing more than about 10⁵–10⁶ rows, or the computation is iterative at
all (PageRank, components, propagation), move it into pg_warren. Both forms
produce rows keyed by node id, so switching changes one clause, not the
surrounding query.

## The SQL surface at a glance

Every function that works on a graph takes its name first and, where it
reads the graph, an optional `version` (default: the current version).
`version`, `last_stats`, `s`, `steps` and `shared_cache` take no graph.

| Area | Functions |
|---|---|
| Graphs and versions | `create_graph`, `refresh`, `promote`, `drop_version`, `compact`, `diff`, `info`, `drop_graph`, `grant`, `revoke`, `prewarm`, `evict` |
| Traversal | `bfs`, `neighbours`, `degree`, `has_edge`, `reachable`, `reachable_set`, `shortest_path`, `dijkstra`, `components`, `walk`, `last_stats` |
| Walk steps | `s` and `steps` (jsonb), which build the composite type `warren.step`; the cast `jsonb::warren.step[]` |
| Vectors | `vector_from_query`, `vector_const`, `vector_degree`, `vector_dense_id`, `vector_values`, `matrix_rows`, `vector_topk`, `vector_stats`, `id_map`, `drop_vector` |
| Compute | `spmv`, `spmm`, `vector_map`, `explain_map`, `vector_reduce`, `vector_norm_diff`, `gather` |
| Library | `pagerank`, `eigenvector_centrality`, `katz_centrality`, `hits`, `components_vector`, `label_propagation`, `label_propagation_majority`, `sssp_vector`, `degree_vector`, `aggregate_neighbours`, `triangles`, `clustering`, `core_number`, `strong_components_vector`, `topological_order`, `dag_longest_path`, `minimum_spanning_forest` |
| Spectral | `eigen`, `distance` |
| Extension | `version`, `shared_cache` |

Signatures are in `\df warren.*` and in DESIGN.md (§6, §8, §9.3).

## Documentation

- [docs/semantics.md](docs/semantics.md): graphs, versions, dense ids,
  staleness, transactions, retention, access, id types.
- [docs/traversal.md](docs/traversal.md): `bfs`, paths, components, `walk`
  and its steps, `last_stats`.
- [docs/compute.md](docs/compute.md): vectors, semiring products, masks,
  `vector_map`, `gather`, the algorithm library, `eigen` and `distance`.
- [docs/communities.md](docs/communities.md): contraction to community-level
  graphs, `project` and `lift`; Louvain, Leiden and modularity.
- [docs/change-detection.md](docs/change-detection.md): measuring structural
  change before you commit it, in one transaction or with a shadow graph.
- [docs/limits.md](docs/limits.md): settings, memory, measured performance,
  known limitations.
- [DESIGN.md](DESIGN.md): the full design and the reasons for it.

Every SQL example in the README and `docs/` runs in CI, in
`test/sql/docs_examples.sql`.

## Status

- **0.1.0**: versioned graphs and traversal, milestones M0–M3 of
  DESIGN.md §13.
- **0.2.0**: vectors, semiring compute, `gather`, the algorithm library,
  `eigen` and `distance`, milestones M4–M6.
- **0.3.0**: hardening, milestone M7. Memory is bounded by default (the
  cache, the whole build, and each call, with 53400 refusals before work
  that cannot fit), pg_warren installs without a superuser, dumps restore,
  and a graph of several relations may leave its merged CSR unstored
  (`store_merged := false`). Upgrading needs `ALTER EXTENSION pg_warren
  UPDATE`, and changes some behaviour (see the changelog).

  A graph of 10M nodes and 100M edges builds and runs within the default
  limits (each 4 GB) with one relation, or with five weighted relations and
  `store_merged := false` (DESIGN.md §12; measured on 4 vCPUs). A backend's
  peak is bounded by the cache and compute limits together (and the build
  limit while a `refresh` or `contract` runs), not by 4 GB:
  on the five-relation graph `pagerank` peaks at 4.5 GB and `diff` at
  7.2 GB. The caveats: the five-relation graph beside `pagerank`'s scratch
  is 30 MB over the cache cap (the excess is charged to compute memory) and
  its all-relations reads are slower; with its merged CSR it is 6.0 GB, so
  each statement reloads it, and `diff`, `distance` and `eigen` are refused
  unless the limits are raised. Several §12 targets are missed on 4 vCPUs:
  BFS through SQL is 11× a recursive CTE rather than 20×, `reachable_set`
  26M rather than 50M edges/s, `pagerank` on five relations 36–40 s rather
  than 30 s, its `diff` 58 s rather than 30 s, and `components` without the
  merged CSR is borderline at 10.1 s; `eigen` does not converge within
  10 minutes on a uniform random graph of that size
  ([docs/limits.md](docs/limits.md)).

- **0.4.0**: locality, the library and communities, milestone M8.
  Dense ids can be ordered for locality
  (`create_graph(…, node_order := 'locality')`, `compact(…, node_order)`;
  DESIGN.md §14 item 22). The library gains eigenvector and Katz
  centrality, HITS, clustering and majority-vote label propagation; k-cores,
  strong components, topological order and longest paths on DAGs, and
  minimum spanning forests; betweenness, closeness and harmonic centrality,
  exact or from sampled sources; and neighbourhood similarity for link
  prediction (DESIGN.md §6.7). Contraction builds community-level graphs
  (`contract`, with `project` and `lift` between the levels, DESIGN.md
  §6.6), and Louvain and Leiden community detection are written over it
  (`louvain`, `leiden`, `modularity`;
  [docs/communities.md](docs/communities.md)). Upgrading needs
  `ALTER EXTENSION pg_warren UPDATE` (see the changelog).

  The 100M-edge soak, re-run, moved no §12 row beyond run-to-run spread.
  At 10M nodes `louvain` and `leiden` find the communities of a block
  graph in 115 s and 247 s, but on a graph with little community structure
  `leiden` is refused (53400) at the default `warren.max_compute_memory`
  and `louvain` writes level graphs faster than they shrink (the soak
  stopped it for disk after 7.5 GB); a statement that runs `contract`, as both do, is bounded by
  the cache, compute and build limits together
  ([docs/limits.md](docs/limits.md)).

- **0.5.0** (in development, M9): the shared cache, one copy of a loaded
  version per cluster in dynamic shared memory (DESIGN.md §5.6).

See [CHANGELOG.md](CHANGELOG.md). This is pre-1.0 software: the SQL surface
may still change between minor versions, with an upgrade script each time.

Not there yet: the road to 1.0 (DESIGN.md §13) is a shared cache, one
copy of a version per cluster (M9, 0.5.0); text ids, ownership transfer,
cleanup when an owner role is dropped, quotas and observability (M10);
benchmarks and robustness on real graphs and deployments, and packages
(M11); and a reviewed, frozen SQL surface (M12, 1.0.0). Incremental
maintenance, general sparse matrix algebra (contraction's `PᵀAP` is the
one sparse product) and a GPU backend are left to 1.x
([docs/limits.md](docs/limits.md)).

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
- Every build is an immutable version with a stable id space.
- Builds are stored transactionally in ordinary tables, so they are WAL-logged,
  replicated, and rolled back with your transaction.
- A version can be built *inside* the transaction that changes the data and
  measured against the current one, structurally or spectrally, before you
  commit.
- One CSR serves both traversal and semiring compute (with GraphBLAS operation
  names).
- Control flow stays in PL/pgSQL, with per-node logic written as SQL
  expressions.
- Contraction gives community-level computation: a quotient graph per
  partition, a real graph version with its own vectors, and `project` /
  `lift` between the levels. Louvain and Leiden are short PL/pgSQL
  programs over it.

pg_warren takes no code from either project. Ideas adopted from them are
credited in [NOTICE](NOTICE) and in comments where they are used; the reading
is recorded in [docs/prior-art-notes.md](docs/prior-art-notes.md). Nor does
it bind SuiteSparse:GraphBLAS: its kernels are native, and GraphBLAS is read
for its algorithms and vocabulary, not imported (DESIGN.md §6.10).

## Building and contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE).
