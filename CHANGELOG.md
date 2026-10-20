# Changelog

## 0.3.0 — in development (until M7)

- pg_warren is a trusted extension: a role with `CREATE` on the database
  can run `CREATE EXTENSION pg_warren` without being a superuser
  (DESIGN.md §7). Catalog statements now run as the owner of the
  `warren.*` tables (the installing superuser, or for a trusted install
  the bootstrap superuser), not as `pg_extension.extowner`. The installer
  gets no privilege on those tables. A non-superuser's `pg_dump` fails
  while pg_warren is installed: dump as a superuser, or pass
  `--exclude-table-data='warren.*'`, then create the graphs again with
  `create_graph` and `refresh`.
- Dumps restore. `graphs.current_version` and `versions.label_vector` are
  no longer foreign keys. Together with `versions.graph_id` and
  `vectors.version_id` they made cycles that pg_dump cannot order, so a
  dump of a database with a promoted graph failed to restore. Triggers now
  check that a graph's current version is one of its own versions, and
  that it is not deleted while current. Restoring needs the roles that own
  or read graphs and wrote vectors. Upgrade note: run
  `ALTER EXTENSION pg_warren UPDATE` (the update drops the two keys and
  adds the triggers).
- Query functions, `refresh` and `compact` refuse (55000) a graph whose
  stored current version is not one of its versions, as a restore or a
  direct write can leave it; `promote` one of its versions to repair it.
  Before, they read or built on that version, even another graph's.
- `promote` of a graph that another session drops while the promote waits
  fails with "graph does not exist" (42704), not an internal error.
- `create_graph` raises a `NOTICE` when the graph's `search_path` contains
  a schema that roles other than its owner can create objects in. Those
  roles could plant functions or operators that `refresh` would run as
  the graph's owner.
- `warren.max_cache_memory` (default `4GB`): a size-bound on what a
  backend's cache holds between calls. Cached graph versions and vectors
  are evicted least recently used first and reloaded when next used;
  transient vectors count against it but are never evicted, and a
  transient write that would take them past it fails with 53400.
- `create_graph(…, store_merged boolean DEFAULT true)`: a graph of several
  relations may leave its merged (all-relations) CSR unstored. All-relations
  reads then merge the relations' rows on the fly, with identical results,
  in 44% fewer CSR bytes for five weighted relations and a faster build,
  but 2–3× slower all-relations traversal and 5–8× slower all-relations
  `spmv` at 2–5 relations (more with more relations: 6× and 11× at 10). `warren.info` reports `store_merged`. Schema change:
  new column `warren.graphs.store_merged` (existing graphs are backfilled
  `true`); run `ALTER EXTENSION pg_warren UPDATE` before using 0.3.0.
- Upgrade note: the cache was unbounded before. A graph version larger
  than 4GB, or a version plus the library's transient scratch vectors
  (`pagerank` keeps about eleven full-length `f64` vectors, ~88 bytes per
  node) above it, is now reloaded on every call, with a `NOTICE` once per
  statement. Raise `warren.max_cache_memory` for such graphs.
- Behaviour change: `vector_map` expressions that take the executor path
  now run in Postgres's read-only transaction state too, as 0.2.0 made
  source queries, `vector_from_query` and `gather` do, so an expression
  that writes through a function it calls (a PL/pgSQL `w(x)` that inserts)
  now fails with 25006 instead of writing, as does `nextval` on a
  non-temporary sequence. The library functions, which call `vector_map`,
  inherit this. An integrity guard, not a sandbox (DESIGN.md §7).
- Fix: `vector_norm_diff` refuses (22023, `dense id … names different
  nodes in the two versions`) two versions whose shared dense ids name
  different nodes, such as sibling staged versions that added different
  nodes, as `diff` and `distance` already did. It used to align their
  vectors row by row and return a meaningless value.

## 0.2.0 — vectors, compute and spectral analysis

Milestones M4–M6 of DESIGN.md §13.

- Named vectors (`vector_from_query`, `vector_const`, `vector_degree`,
  `vector_dense_id`, `vector_values`, `matrix_rows`, `vector_topk`,
  `vector_stats`, `id_map`, `drop_vector`), persisted or transient
  (`persist := false`), with per-vector ownership (`created_by`).
- Compute: semiring `spmv` / `spmm` with masks and accumulation,
  `vector_map` (compiled subset plus executor fallback) and `explain_map`,
  `vector_reduce`, `vector_norm_diff`; `warren.threads` and
  `warren.max_compute_memory`.
- `compact` moves persisted vectors into the new id space.
- `gather`: messages of any SQL type from a query, relation or vector,
  fanned out over the graph and combined per receiver (`collect`, `count`,
  `sum`, `min`, `max`, `first`, `concat`, `bool_or`, `bool_and`).
- Algorithm library in PL/pgSQL over the primitives: `pagerank`
  (personalised by a seed vector), `components_vector`,
  `label_propagation`, `sssp_vector`, `degree_vector`,
  `aggregate_neighbours`; and `triangles`, a per-node triangle count.
- Spectral analysis and version distance: `eigen` (Lanczos top-k
  eigenpairs of the adjacency, Laplacian, normalised Laplacian or random
  walk, eigenvectors stored as `<prefix>_1..k` vectors, `f32` or `f64`) and
  `distance` (`edit`, `degree`, `spectral`, `subspace`, `embedding`, as
  `jsonb`), between versions of a graph or, with `graph2`, of two graphs
  aligned by external id.
- Behaviour change: SQL a user wrote now runs in Postgres's read-only
  transaction state while warren reads it, so a graph's source queries
  (at `refresh`), `vector_from_query`'s query and `gather`'s message query
  that write through a function they call now fail with 25006 (`cannot
  execute … in a read-only transaction`). This is an integrity guard, not
  a sandbox (DESIGN.md §7).
- Fix: functions that read a graph or vector no longer fail with
  "cannot assign XIDs during a parallel operation" when the query runs in
  parallel mode (`debug_parallel_query`, or a plan that costs out parallel,
  e.g. an InitPlan over a parallel scan). Parallel labels: every function
  that reads a graph or vector is `PARALLEL RESTRICTED` (`diff` and `info`
  change from `PARALLEL UNSAFE`), and writers are `PARALLEL UNSAFE`
  (DESIGN.md §9.3).
- Behaviour change: `evict` also frees the graph's cached and transient
  vectors in this backend (transient vectors are gone afterwards), and
  counts them in its result.
- Upgrade from 0.1.0: install the new build, then run
  `ALTER EXTENSION pg_warren UPDATE;` in each database before using it.

## 0.1.0 — a usable traversal index

Milestones M0–M3 of DESIGN.md §13.

- Named graphs over user tables (`create_graph`), rebuilt as immutable
  versions (`refresh`) with stable dense ids, staging, promotion, retention,
  `compact`, `diff`, `info`, `grant`/`revoke`, `drop_version`, `drop_graph`.
- Transactional, chunked persistence in extension-owned tables; a staged
  build inside a rolled-back transaction leaves nothing behind.
- int4, int8 and uuid node ids; optional relations and weights; forward and
  reverse CSR.
- Traversal: `bfs`, `neighbours`, `degree`, `has_edge`, `reachable`,
  `reachable_set`, `shortest_path`, `dijkstra`, `components`, typed path
  patterns with `walk` / `warren.step` / `warren.s`, and `last_stats`.
- Backend-local cache with `prewarm` / `evict`.
- Every SQL entry point runs under a pinned `search_path`; source queries run
  as the graph owner under the search path captured at `create_graph`.
- Tested on PostgreSQL 15–18; traversal functions checked against recursive
  CTE oracles.
