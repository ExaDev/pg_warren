# Changelog

## 0.4.0 — in development

## 0.3.0 — hardening: bounded memory, trusted install, 100M-edge scale (2026-10-20)

Milestone M7 of DESIGN.md §13. A graph of 10M nodes and 100M edges now
builds and runs within the default limits (each 4 GB; a backend's peak is
bounded by the cache and compute limits together) when it has one relation,
or five weighted relations with `store_merged := false` (the 100M-edge soak,
DESIGN.md §12; what still misses is under Performance).

### Upgrading

- **Run `ALTER EXTENSION pg_warren UPDATE;` in every database that has the
  extension**, after installing the new build and *before* calling any
  `warren.*` function. 0.3.0 changes the schema: the update adds the column
  `warren.graphs.store_merged` (existing graphs are backfilled `true`),
  drops the foreign keys `graphs.current_version` and
  `versions.label_vector` and adds the triggers that replace them (and
  their trigger functions `warren.current_version_is_the_graphs()` and
  `warren.current_version_is_kept()`, revoked from `PUBLIC`), adds
  `create_graph`'s `store_merged` argument, and changes the `tol` default
  of `eigen` and `distance` (`max_iter`'s new default comes with the
  library, whether or not the update has run). The new library is loaded
  at once, but the catalog changes only with the update, so calls in
  between can fail.
- `create_graph` is dropped and re-created by the update (it gains an
  argument), so its privileges return to the defaults: re-apply any
  `GRANT` or `REVOKE` you made on `warren.create_graph` after updating.
- Every backend's memory is now bounded by default (below). A graph or
  call that 0.2.0 ran outside every limit can now be reloaded per
  statement or refused with 53400; raise `warren.max_cache_memory`,
  `warren.max_compute_memory` or `warren.max_build_memory` for it.

### Behaviour changes

- **A size bound on the cache:** `warren.max_cache_memory` (default `4GB`)
  bounds what a backend's cache holds between statements; it was
  unbounded. Cached graph versions and persisted vectors are evicted least
  recently used first and reloaded when next used. Transient vectors count
  against it but are never evicted, and a transient write that would take
  them past it fails with 53400. A graph version larger than the cap, or a
  version beside the library's transient scratch (`pagerank` keeps about
  eleven full-length `f64` vectors, ~88 bytes per node) above it, is held
  only for the statement that reads it and reloaded by the next (a version
  alone over the cap says so with a `NOTICE` once per statement). Raise
  `warren.max_cache_memory` for such graphs.
- **The statement pin:** what a top-level statement reads stays cached
  until it ends. Every version and persisted vector it reads or writes is
  pinned, then released when the statement ends, normally or with an
  error, or when its transaction does (so a `CALL` that commits holds only
  what each of its transactions read), and the cache is trimmed to the
  cap. A statement is one client message: each extended-protocol
  `Execute`, or one query string of several statements. A library function
  (`pagerank`, …) is one statement, so it loads its version once even when
  the version and its transient scratch together exceed the cap, and a
  statement alternating between two graphs that do not fit the cap
  together loads each once. What the cache holds past the cap and cannot
  evict (transient vectors, what calls are using, and versions open
  cursors still hold) is charged to each call's
  `warren.max_compute_memory`, so a backend's peak is bounded by the two
  settings together (DESIGN.md §5.5, §14 item 20). What a statement read
  but no call is using is charged to each call past the cap too, and gives
  way, least recently used first, to a load or to a call that needs the
  room, so a statement reading more graphs in sequence than both settings
  hold still works. A vector the cache already holds is not charged to a
  call that reads it. `diff`, `distance` and `vector_norm_diff` read their
  two versions through the cache under the same rule.
- **`warren.max_build_memory` bounds the whole build**, not only the CSR
  offsets: the node ids, the id map, the relation dictionary, the edge
  list, the CSRs being built and the sort and write buffers. The offsets
  check is kept, and now also runs as edge rows add relations, and the
  relation dictionary is charged as it grows, so a relation column with a
  name per row is refused while it is read. The budget does not include
  what the backend's cache holds. Versions built are byte-identical to
  0.2.0's.
- **New 53400 (`configuration_limit_exceeded`) refusals**, where 0.2.0
  ran past the limits or failed late:
  - a `refresh` over `warren.max_build_memory` (`build memory limit
    exceeded`), up front where it can be (before the edge query runs, or
    before the version is written), else when the edge list grows past
    it;
  - a version load that would take what the cache cannot evict past
    `warren.max_cache_memory` and `warren.max_compute_memory` together,
    refused before loading (the message names both settings and the
    bytes); `diff`, `distance` and `vector_norm_diff` refuse two versions
    that do not fit together before loading either;
  - a transient vector write that would take transient vectors alone past
    `warren.max_cache_memory`;
  - `eigen`, and `distance('spectral')` with a fresh solve, when the
    solve's working set cannot fit `warren.max_compute_memory`: once the
    operator's degrees give the dimension, everything the solve holds at
    its peak (the operator's work vectors and degrees, the basis, the
    projected matrices, now charged too, and the k eigenvectors in the
    target dtype) is checked at once, evicting idle pinned versions if
    that makes room, before the first step (`the eigenproblem needs N
    bytes, X of Y in use beside it`). Before, such a solve ran its steps
    and failed only when it allocated its eigenvectors.
    `distance('spectral')` checks both versions before solving either;
  - buffers grown on compute worker threads (`gather`'s result and a
    multi-relation product's row scratch) cannot evict what a statement
    read, so they can fail with 53400 where eviction would have made room.
- **`eigen` and `distance` defaults:** `tol := 1e-6` (was `1e-8`), about
  the rounding of the default `f32` vectors, whose explicit residual does
  not fall below about 10⁻⁷·‖M‖; the old default bought 5–16% more steps
  and no accuracy. `max_iter` defaults to 1,000 operator applications per
  basis vector (41,000 for k = 20), up from 100 (4,100): the normalised
  Laplacian of a random graph of 1M nodes needs 4,590 (DESIGN.md §6.5,
  §14 item 21). A solve that does not converge therefore runs about 10×
  longer before its `WARNING`; `statement_timeout` or a cancel stops it.
  Pass `tol` and `max_iter` explicitly to keep the old behaviour (with
  `dtype := 'f64'` for a tighter `tol` to count). The update changes the
  `tol` default; the `max_iter` default changes with the library.
- **`reachable_set` is direction-optimising.** When the version has the
  reverse CSR, a depth is pulled from it if its frontier holds more than
  1/24 of the id space and the unvisited ids have fewer in-edges than the
  frontier has out-edges (Beamer's switch), so each unvisited id scans its
  in-edges only until it meets the frontier. It returns the same nodes at
  the same depths, but within a pulled depth rows come in dense id order
  instead of the order reached (so which rows a select-list `LIMIT` keeps
  can differ from `bfs`'s; use `ORDER BY` when the order matters), and
  `last_stats().edges_traversed` counts the row entries scanned rather
  than every edge of the visited nodes: 17.9M rather than 100M on a
  G(n, m) graph of 10M nodes and 100M edges. That is never more than a
  push-only search would scan, except that it can be on a graph of
  several relations without the merged CSR, read with every relation.
  `bfs` is unchanged: it keeps push for its `via` and its order (DESIGN.md
  §9.3).
- **Trusted install, and a new catalog owner.** pg_warren is a trusted
  extension: a role with `CREATE` on the database can run `CREATE
  EXTENSION pg_warren` without being a superuser (DESIGN.md §7). Catalog
  statements now run as the owner of the `warren.*` tables (the installing
  superuser, or for a trusted install the bootstrap superuser), not as
  `pg_extension.extowner`. The installer gets no privilege on those
  tables. A non-superuser's `pg_dump` fails while pg_warren is installed:
  dump as a superuser, or pass `--exclude-table-data='warren.*'`, then
  create the graphs again with `create_graph` and `refresh`.
- **`create_graph(…, store_merged boolean DEFAULT true)`:** a graph of
  several relations may leave its merged (all-relations) CSR unstored. The
  default, and every graph created before 0.3.0, keep it; the choice is
  fixed when the graph is created, and `compact` keeps it. All-relations
  reads then merge the relations' rows on the fly, with identical results,
  in 44% fewer CSR bytes for five weighted relations and 35–45% less
  build time, but at 2–5 relations BFS is 2.1–3.2× slower, `components`
  2.7–4.5×, `gather` 1.5–1.9× and `spmv` 4.6–8.3×, and at 10 relations BFS
  6×, `components` 10× and `spmv` 11× (DESIGN.md §5.3, §14 item 19).
  `warren.info` reports `store_merged`.
- `vector_map` expressions that take the executor path run in Postgres's
  read-only transaction state, as 0.2.0 made source queries,
  `vector_from_query` and `gather` do, so an expression that writes
  through a function it calls (a PL/pgSQL `w(x)` that inserts) fails with
  25006 instead of writing, as does `nextval` on a non-temporary sequence.
  The library functions, which call `vector_map`, inherit this. An
  integrity guard, not a sandbox (DESIGN.md §7).
- Query functions, `refresh` and `compact` refuse (55000) a graph whose
  stored current version is not one of its versions, as a restore or a
  direct write can leave it; `promote` one of its versions to repair it.
  Before, they read or built on that version, even another graph's.

### New

- `warren.max_cache_memory` and the trusted install, above.
- `create_graph` raises a `NOTICE` when the graph's `search_path` contains
  a schema that roles other than its owner can create objects in. Those
  roles could plant functions or operators that `refresh` would run as the
  graph's owner.
- `refresh` reports its build memory peak at `DEBUG1`.

### Fixes

- Dumps restore. `graphs.current_version` and `versions.label_vector` are
  no longer foreign keys. Together with `versions.graph_id` and
  `vectors.version_id` they made cycles that pg_dump cannot order, so a
  dump of a database with a promoted graph failed to restore. Triggers now
  check that a graph's current version is one of its own versions, and
  that it is not deleted while current. Restoring needs the roles that own
  or read graphs and wrote vectors.
- `diff` loaded its two versions outside every limit: on two 6.0 GB
  versions a backend reached 12.8 GB and was killed. It now reads them
  through the cache, and refuses two that do not fit (above).
- `vector_norm_diff` refuses (22023, `dense id … names different nodes in
  the two versions`) two versions whose shared dense ids name different
  nodes, such as sibling staged versions that added different nodes, as
  `diff` and `distance` already did. It used to align their vectors row by
  row and return a meaningless value.
- `promote` of a graph that another session drops while the promote waits
  fails with "graph does not exist" (42704), not an internal error.
- Reading the edge query of a graph with relations held Postgres memory
  for every row's relation name until the `refresh` ended (about 1.7 GB at
  100M edges); names are now read in place.
- A cold load peaked at 2.7× the version (3.2 GB for a 1.2 GB version),
  because each segment's chunks were held in SPI memory and again in Rust;
  chunks are now read one at a time, and the peak is the version plus one
  CSR's raw segments (1.69 GB). Vector reads share the chunk reader.
- `vector_from_query` with array values held a copy of every row's array
  until the transaction ended (16 MiB for 20,000 rows of 64 values), and
  `gather` from a query held what each message's send function allocated
  until the read ended (about 230 B per `int[]` message). Both now read a
  batch of rows at a time and free it after; results are unchanged.

### Performance

Measured at 10M nodes and 100M edges on 4 vCPUs, at the default limits
(DESIGN.md §12, `bench/results/soak/*-m7*.json`).

- `refresh` streams each relation's CSRs to `warren.segments` and frees
  them, and encodes segments a chunk at a time. It peaks at 1.55 GB for
  one unweighted relation (was 2.0 GB), and 3.26 GB for five weighted
  relations with or without the merged CSR (was 9.0 and 5.5 GB), all
  within the default `4GB`, in 153, 260 and 228 s.
- `reachable_set` over the full component: the kernel takes 1.24 s
  instead of 5.96 s, and the call through SQL 3.86 s (26M graph edges/s)
  instead of 8.3–9.2 s; 6.15 s without the merged CSR. At 1M nodes and 10M
  edges, 0.21 s instead of 0.49 s.
- `eigen`'s steps are cheaper: an undirected graph's operator symmetrises
  with one product, and the basis passes sum their dot products in eight
  lanes and rotate by row chunks. On one relation a step takes 0.59–0.61 s
  (was 0.96 s).
- `pagerank` (20 iterations) takes 15.6 s on one relation, and loads its
  version once on five weighted relations: 35.6 s without the merged CSR
  (3.44 GB, 30 MB over the cap beside its scratch) and 40.1 s with it
  (6.0 GB, over the cap, 23 s of it the load).
- What still misses §12's targets: BFS to depth 6 is 11.0× the recursive
  CTE rather than 20× and `reachable_set` 26M rather than 50M edges/s,
  both mostly row emission through SQL; `eigen` (k = 20, normalised
  Laplacian) does not converge within 10 minutes on a uniform random graph
  of 10M nodes (~14,500 steps projected, about 2 h), while a community
  graph's ~400 steps would take 3–4 minutes; and five weighted relations
  with the merged CSR (6.0 GB) are over the cap, so each statement reloads
  them (~23 s) and `diff`, `distance` and `eigen` are refused at the
  default limits (docs/limits.md, "Sizing").

### Documentation

- A user-facing README, and `docs/`: semantics (versions, dense ids,
  staleness, transactions, retention, access, id types), traversal,
  compute (vectors, semirings, masks, `vector_map`, `gather`, the library,
  `eigen`, `distance`), the two change-detection patterns, and limits
  (settings, the memory model, a sizing table from the soak, measured
  performance, known limitations). Every SQL example runs in CI
  (`test/sql/docs_examples.sql`).
- DESIGN.md records the M7 decisions: native kernels, with GraphBLAS,
  LAGraph and OneSparse read rather than bound, and the kernel seam a GPU
  backend would implement (§6.10, §14 item 9); the cache and compute bound
  (§14 item 20); `eigen` at scale (§14 item 21); the default for the
  merged CSR (§14 item 19); and, for after 0.3.0, locality ordering of
  dense ids (§14 item 22) and the planned library (§6.7).
- The upgrade check now builds each release from its recorded commit and
  writes state with that release's own library, and runs with a trusted
  install too; a `pg_dump`/restore round trip runs in CI on PostgreSQL
  15–18.

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
