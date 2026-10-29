# Changelog

## 0.9.0 — in development

### Upgrading

- **Run `ALTER EXTENSION pg_warren UPDATE;` in every database that has
  the extension**, after installing the new build. The update adds the
  two functions below; it changes no table.

### Added

- **Graphs from keys.** `warren.queries_from_keys(tables regclass[])`
  returns the `node_query`, `edge_query` and `has_relations` that the
  listed tables' primary and foreign keys give, for `create_graph`, and
  `warren.create_graph_from_keys(name, tables, ...)` creates the graph
  from them in one call. Each listed table's rows are nodes, each foreign
  key between listed tables a relation, and a junction table (its primary
  key two foreign keys, and no other column) an edge per row. One table of
  nodes keyed by an `integer`, `bigint`, `uuid` or `text` column keeps its
  key as the id; otherwise ids are text, `schema.table:key`, with a
  composite key as its row literal (`shop.shipments:(10,2)`). Only the
  listed tables are read, and foreign keys to other tables are ignored
  (docs/semantics.md, "Graphs from keys"; DESIGN.md §14 item 52).

## 0.8.0 — native PageRank and Leiden, leaner calls (2026-10-29)

### Upgrading

- **Run `ALTER EXTENSION pg_warren UPDATE;` in every database that has
  the extension**, after installing the new build. The update replaces
  the PL/pgSQL definitions of `warren.pagerank` and `warren.leiden` with
  native ones (under Performance), keeping their privileges; it changes
  no table. Until the update, 0.7.0's definitions still run with the new
  library, as they did.

### Behaviour changes

- `warren.pagerank` refuses a NULL `out` or `damping` before it runs, as
  `out must not be NULL` and `damping must not be NULL` (SQLSTATE 22004,
  as before; 0.7.0 ran first and then named `y`, or `value`). An empty
  or unwritable `out` is refused before the run, too. Its intermediates
  (`_warren_pagerank_*`) no longer exist, even during a run, and it no
  longer carries `SET search_path`: the access guard pins it for the call
  (DESIGN.md §7). Everything else is kept: its signature, ranks to the
  bit, iteration counts, SQLSTATEs, progress, parallel label and
  privileges, including a NULL `tol` never stopping it early and a NULL
  `max_iter` running no iteration.
- `leiden` checks that the caller may write `out` before it runs, not
  after. The error is the same (42501). The function no longer sets its
  own `search_path`; like every other `warren.*` function, it pins it for
  the call.
- On graphs with fractional weights, `leiden` sums a level's edge weights
  in a different order from 0.7.0's `contract`. A weight may differ in its
  last bit, and a tie between moves that it decides may then go the other
  way. On unweighted and integer-weighted graphs the partitions, kept
  levels and modularity are 0.7.0's, to the bit.

### Fixes

- **`gather` over `char(n)` messages no longer truncates them to one
  character** for `collect` and `first`: their query was cast to
  `character`, which is `character(1)`; it is now cast to `bpchar`, as
  `array_agg` keeps them.
- **`gather`'s `collect`, `first` and `count` over `text`, `varchar` or
  `char` messages no longer depend on `client_encoding`**: a message it
  could not represent failed with 22P05 ("has no equivalent in
  encoding"), since each message passed through `textsend`.

### Performance

- `warren.pagerank` is a native kernel, not a PL/pgSQL loop over the
  compute primitives (DESIGN.md §6.7). The loop made eleven calls an
  iteration, about 1.5 ms of overhead an iteration however small the
  graph; the kernel makes two parallel passes and one product an
  iteration, and holds four vectors of the id space (five with a seed)
  where the loop held eleven: 0.32 GB of scratch at 10M nodes instead of
  0.88 GB.
  Twenty iterations, in a first single before-and-after run on a loaded
  machine (median warm times, `warren.threads = 4`): lesmis 34.7 → 1.6 ms, facebook 47.3 → 6.3 ms,
  email-enron 74.9 → 22.6 ms, cit-hepth 65.7 → 18.6 ms and DBLP
  318 → 113 ms. The later three-round run timed lesmis at 3.1 ms.
  Against igraph on the 12 head-to-head datasets (three
  interleaved rounds, `bench/results/corpus/head_to_head/README.md`,
  "PageRank in 0.8.0"): faster on 7 (roadNet-TX 0.22 s against 0.81 s,
  DBLP 105 ms against 251 ms) and slower on 5 (WN18RR, cit-hepth and
  YAGO3-10 by 1.2–1.8×, polblogs by 3.1×, and lesmis, 3.1 ms against
  29 µs, by a call's fixed cost).
- `leiden` is native (DESIGN.md §6.7, "Leiden's levels"). Each level's
  graph is contracted from the level below, not from the whole graph, and
  is held in memory, charged to `warren.max_compute_memory`. Only `out`
  and the levels `levels_prefix` asks for are written. No level graph,
  segment or WAL is written for a level, and the disk a run needed grew
  with its levels before.
  - It finds the same partitions as before. A pg_test runs the frozen
    0.7.0 function beside it.
  - On the head-to-head's real graphs it is 1.3–21× faster:
    fb15k-237 1.44 → 0.21 s, yago3-10 2.26 → 0.73 s, roadnet-tx
    7.9 → 3.8 s, lesmis 25 → 1.2 ms.
  - It takes 0.76–1.6× igraph's time on the medium tier, where it was
    1.9–180×, and is faster than igraph on ss-gleich-usroads-48, dblp,
    yago3-10 and roadnet-tx (`bench/results/corpus/head_to_head/`,
    "Leiden in 0.8.0").
- **`gather` over `text`, `varchar`, `char` and `bytea` messages** keeps
  them as their bytes, as `concat` did, instead of each going through its
  type's send and receive functions, and lays a collected array out
  straight from them: at 10M nodes and 11.5M messages, a `collect` of
  cheap text messages takes 14.5 s rather than 16.7 s (13.5 s as
  `bigint`). Appendix A.3's first superstep goes from 30.8 s to 29.5 s;
  most of it is its message query (DESIGN.md §13, "After 0.7.0").
- **`distance('edit')` across two graphs (`graph2`) no longer rebuilds
  the second graph** in the first's dense ids: it maps each edge as it
  reads it and looks it up in the first graph's row (DESIGN.md §8). It
  charges only the id mapping, 8 bytes a node of the second graph, where
  it charged 96 bytes an edge for the rebuild, so Appendix A.2 at 10M
  asks for 88 MB instead of 10.9 GB and runs under the 4GB default rather
  than failing with 53400. At 2.2M nodes and 22M stored edges the call's
  peak memory falls from 290 MB to 9 MB and its time from 1.33 s to
  0.39 s. Results are unchanged.
- **A short kernel step no longer waits to be woken.** The calling thread
  spins for up to 50 µs before it parks while the compute pool runs a job,
  so a job over a small graph returns in 2–4 µs instead of the 20–40 µs
  the operating system took to wake the caller (DESIGN.md §6.9).
  PageRank runs three jobs an iteration: on lesmis a call takes
  1.3–1.4 ms instead of 3.5–3.7 ms (on a 3-node graph 1.0–1.2 ms instead
  of 2.8–3.0 ms), and larger graphs are unchanged or slightly faster.
- **Measured: a traversal call's fixed cost is 25–70 µs, not 0.2–2 ms.**
  Warm, with the round trip, `bfs`, `shortest_path` and `walk` take
  0.07–0.11 ms against 0.04 ms for `SELECT 1`, on PG 16 and 18. Half of
  the server's share is the one catalog read that resolves the graph
  (DESIGN.md §13, "After 0.7.0"). Nothing in traversal changed.

## 0.7.0 — proven on real graphs and real deployments (2026-10-27)

Milestone M11 of DESIGN.md §13: proven on real graphs and in real
deployments. pg_warren now runs a suite of 19 real graphs pinned through
keepset (`bench/corpus`), checked against NetworkX and igraph and timed
head to head with igraph and Neo4j, with and without GDS (README, "How it
compares"), and Appendix A's use cases at 10M nodes. A pair of nodes
linked in several relations is now read one way everywhere: strengths
are summed and an unweighted pair counts once, costs take the pair's
best edge, and `per_relation := true` reads an edge in each relation; on
graphs with such pairs some results differ from 0.6.0's (under Behaviour
changes). A relation with few edges stores sparse offsets, so every graph
needs a `refresh` after the upgrade (under Upgrading). Concurrent
refreshes, crash recovery, streaming replicas, a connection pooler and a
SQL fuzzer found the bugs under Fixes, two of them server crashes.
Packages are published from this release on: a signed apt repository for
Debian and Ubuntu, Docker images, a Homebrew tap and tarballs, for amd64
and arm64 (under New).

### Upgrading

- **If `pg_warren` is in `shared_preload_libraries`, restart the server**
  after installing the new build: the postmaster keeps the library it
  loaded at start, and every backend inherits it.
- **Restart the server if the shared cache is in use.** The shared cache's
  registry is now `pg_warren.cache.4`, so backends of the old and the new
  build never share a version; the old registry's segments are freed by a
  restart.
- **Run `ALTER EXTENSION pg_warren UPDATE;` in every database that has
  the extension**, after installing the new build and before using the
  new version. The update drops and creates again the 27 functions that
  gain `per_relation` (under New), keeping their privileges; it changes
  no table. Until the update, 0.6.0's definitions still work with the new
  library, reading a pair as the new default does, except 0.6.0's
  `sssp_vector` (PL/pgSQL), which still sums a pair's weights until the
  update.
- The update drops those functions without `CASCADE`, so it fails if a
  view, or a function with a `BEGIN ATOMIC` body, calls one of them
  ("cannot drop function ... because other objects depend on it"). Drop
  the dependent objects, run `ALTER EXTENSION pg_warren UPDATE`, then
  create them again.
- **Refresh every graph after installing the new build.** A relation's
  CSR can now store sparse offsets, so CSR segments have a new format
  (2). Reading a version built by an earlier release fails with
  `object_not_in_prerequisite_state` (55000) and a message naming
  `warren.refresh`; a `refresh` builds on it as usual, and the version it
  builds reads. Old versions are not converted. Vectors are bound to a
  version, so recreate them on the new one.

### Behaviour changes

- **An unweighted pair of nodes linked in several relations now counts
  once in `modularity`, `contract`, `local_moving`, `leiden_refine`,
  `louvain` and `leiden`**, as it already did in `spmv`, `degree` and
  `pagerank`. They counted it once per relation, so on graphs with such
  pairs their results differ from 0.6.0's. A counting `contract` counts
  the pair in the first of its relations. Weighted pairs are unchanged:
  their weights are summed across relations.
- **Functions that read edge weights as costs now take a weighted pair
  linked in several relations at its best edge, not the sum of its
  relations' weights**: `sssp_vector`, `betweenness_centrality`,
  `closeness_centrality`, `harmonic_centrality` and
  `minimum_spanning_forest` its lightest relation's weight,
  `dag_longest_path` its heaviest. Strengths add, costs don't: a road of
  5 km and a railway of 3 km between two towns are a route of 3 km, not
  8. Through 0.6.0 these functions summed the weights, so on weighted
  graphs with such pairs their results differ from 0.6.0's. `dijkstra`
  already took the cheapest relation through 0.6.0, and still does.
  Functions that read weights as strengths (`spmv`, `spmm`, `pagerank`
  and the other spectral centralities, the community functions,
  `contract`, `eigen`, `distance`) still sum them; `spmv` with `min_plus`
  reads the sum too unless given `per_relation := true`. Unweighted calls
  are unchanged. See docs/semantics.md, "A pair in several relations".
- **`per_relation := true` gives the cost functions the same result as
  the default** (`dijkstra`, `sssp_vector`, the three shortest-path
  centralities, `dag_longest_path`, `minimum_spanning_forest`): as
  parallel edges, a path takes the cheapest (longest paths the heaviest).
  They accept it, and refuse NULL (22004), so that `per_relation` means
  one thing on every function.
- **Undirected weighted centralities refuse a pair whose weight is NaN in
  one direction** (22023), as they refuse a NaN anywhere else. They used
  the other direction's weight.
- **`eigen` (and `distance('spectral')`) returns every copy of the
  eigenvalue each connected component gives**: 0 of `laplacian` and
  `normalized_laplacian` with `which := 'smallest'`, 1 of `random_walk`
  with `which := 'largest'`, and each bipartite component's −1 of
  `random_walk` and 2 of `normalized_laplacian` with `which :=
  'largest'`. Each copy is one component's eigenvector,
  found exactly and without Lanczos steps, components in the order of
  their lowest node, and Lanczos computes the rest of the `k` orthogonal
  to them. Before, Lanczos found one copy and mistook the next eigenvalues
  for the rest: 30 disjoint 40-cycles' three smallest Laplacian
  eigenvalues came back as 0, 0.025 and 0.098, and on roadNet-TX (424
  components) the call ran past a 10-minute timeout; four paths' random
  walk gave 1, 1, 1, 1, −1, 0.9709, −0.9709 for its 7 largest, against
  four −1s. A call all of whose pairs are the components' no longer warns
  of several components; one with pairs from Lanczos still does. Other
  results change in the last digits; a connected graph's 0 is now exactly
  0.
- A graph's number of relations no longer refuses its build on its own.
  `warren.max_build_memory` used to be checked against 8 bytes per node
  for every relation's CSR (and the merged CSR), in each direction; it is
  now checked against the merged CSR's offsets and 8 bytes per relation,
  and each relation's offsets count in the build's estimate by its edges.

- **`bfs` expands each depth's nodes in ascending dense id order** (the
  graph's internal node order), so within a depth its rows can come in a
  different order than in 0.6.0, and a node reachable from several nodes
  of the previous depth reports as `via` the one of smallest dense id.
  Nodes and depths are unchanged.
- `walk` and `neighbours` make each row's external ids as the row is
  returned, rather than all rows before the first, as `components` and
  the others have since 0.6.0. Their rows and order are unchanged; `walk`
  still runs the whole walk in its first call. Such a call keeps the
  version it reads, private or mapped from the shared cache, until its
  last row is read or it ends (under Performance).

### New

- **`per_relation boolean DEFAULT false`** on `modularity`, `contract`,
  `local_moving`, `leiden_refine`, `louvain`, `leiden`, `spmv`, `spmm`,
  `pagerank`, `katz_centrality`, `eigenvector_centrality`, `hits`,
  `sssp_vector`, `dijkstra`, `betweenness_centrality`,
  `closeness_centrality`, `harmonic_centrality`, `dag_longest_path`,
  `minimum_spanning_forest`, `degree`, `vector_degree`, `degree_vector`,
  `aggregate_neighbours`, `gather`, `label_propagation_majority`, `eigen`
  and `distance`: `true` reads a pair linked in several relations as an
  edge in each (one per relation, as parallel edges), for when that
  multiplicity is the signal.

- **`docs/limits.md`, "Behind a connection pooler"**: what transaction
  pooling loses and leaks between clients (settings, transient vectors,
  `last_stats()`, SQL `PREPARE`) and what to use instead, checked by
  `test/pooler_soak.py`, a soak behind PgBouncer in transaction and
  session pooling that watches pooled backends, the shared cache and the
  catalog for growth (CI runs it for 2 minutes on PG 16).

- **Packages**, published with each release from this one on (README.md,
  "Install"): a signed apt repository of `postgresql-NN-pg-warren`
  packages for PostgreSQL 15–18 on Debian 12 and 13 and Ubuntu 22.04 and
  24.04, amd64 and arm64, built against PGDG's PostgreSQL; Docker images
  on GHCR (`ghcr.io/exadev/pg_warren:pgNN` and `X.Y.Z-pgNN`, PGDG's
  `postgres` image with the package's files and pg_warren preloaded);
  a Homebrew formula (`brew install exadev/tap/pg_warren`, for
  `postgresql@17` and `@18`, bottled on Apple silicon); and tarballs for
  other glibc Linux. Each GitHub Release has a signed `SHA256SUMS`. The
  packages do not edit `postgresql.conf`: add the preload yourself. macOS
  is not yet a supported platform (its tests run in CI without gating),
  and RPM packages and PGXN are not built yet.

### Fixes

- **A read of a graph inside a parallel `CREATE TABLE AS` (or `CREATE
  MATERIALIZED VIEW`, `REFRESH MATERIALIZED VIEW`) no longer fails with
  "cannot start commands during a parallel operation".** Such a statement
  has a transaction id before its plan runs, so pgrx ran every catalog
  read non-read-only, with a command counter increment, which Postgres
  refuses in parallel mode once the command has used its command id: in
  the leader as soon as it ran the function, and in a worker. In parallel
  mode the catalog reads now run read-only, as cursors under the active
  snapshot (DESIGN.md §6.9; found running Appendix A.3 at 10M nodes, whose
  sampled oracle builds a table from `warren.id_map`; tests
  `every_read_runs_in_a_parallel_create_table_as`,
  `a_parallel_create_table_as_runs_a_safe_read_in_its_leader_too`).
- **On a physical standby, every function that writes fails with
  `read_only_sql_transaction` (25006), "cannot change pg_warren's catalog
  in a read-only transaction", with a hint that the server is a
  standby.** It failed with an internal error (XX000), "cannot assign
  TransactionIds during recovery": pgrx's writable SPI calls assign a
  transaction id before the write, which a standby refuses. That covers
  `create_graph`, `refresh`, `promote`, `compact`, the `drop_*` functions,
  access and quota changes, `contract`, persisted vector writes and the
  library functions, which persist their result. Reads, and compute with
  `persist := false`, work on a standby and answer as on the primary
  (DESIGN.md §5.1, *Standbys*; found by `test/replicas.py`). A read-only
  transaction on a primary now gets the same refusal before it assigns a
  transaction id.

- **`refresh` with a NULL `name` or `promote`, and `prewarm` with a NULL
  `name`, are refused with `null_value_not_allowed` (22004)**, "… must not
  be NULL". They failed with an internal error, "argument 0 must not be
  null" (XX000). Found by the SQL fuzzer.

- **`eigen` with `vectors_prefix` and a `k` larger than the graph, and
  `distance(..., 'subspace')` with a `k` larger than the vectors stored,
  no longer crash the server.** Each made a name for every one of the `k`
  vectors before anything else, and with `k` near 2^31 that allocation
  failed and aborted the backend (signal 6), which restarted every
  backend. With `vectors_prefix`, `eigen` now admits the eigenproblem
  before it makes or checks a name for each eigenvector: a `k` past the
  dimension is refused with `invalid_parameter_value` (22023), "cannot
  compute k eigenpairs of a matrix of dimension n", and a working set
  that cannot fit with 53400, which now comes before a refusal to replace
  another role's eigenvectors (42501). `subspace` makes each name as it reads it,
  failing at the first missing vector with `undefined_object` (42704).
  Found by the SQL fuzzer.

- **An array of node ids whose last element is NULL no longer fails with
  an internal error** ("assertion failed: array.is_within_bounds", XX000)
  in debug builds: a `bfs` or `reachable_set` start, a `node_filter`, a
  step's `filter` (`warren.s`). The NULL is skipped, as a NULL elsewhere
  in the array always was. Found by the SQL fuzzer
  (`test/sql_fuzz.py`). The ids are read while the array is alive,
  including an array read from a table column, which a function gets
  detoasted into a copy (tested with short, compressed and out-of-line
  `text[]` and `uuid[]` arrays).

- **A persisted vector written to a version that retention or
  `drop_version` deleted meanwhile fails with `undefined_object` (42704),
  "version … no longer exists: vector … was not written to it".** It
  failed with a raw foreign-key violation on `warren.vectors` (23503).
  Every persisted write (`spmv`, `pagerank`, `vector_const` and the rest)
  now locks the graph's and the version's rows before it writes, waiting
  for a deletion in progress (harness tests `vector_retention_race` and
  `test/concurrent_refresh.py`). Under REPEATABLE READ or SERIALIZABLE
  such a write fails with `serialization_failure` (40001) instead: retry
  it.

- **A read of a shared version that another backend was evicting at that
  moment could fail with an internal error**, "cannot pin a segment that
  is already pinned" (XX000): the evicting backend unpinned the version's
  segment after releasing the shared cache's lock, so a reader could
  revive the version first. It now unpins under the lock a segment that
  another backend still maps, which frees nothing; one nobody maps, which
  unpinning frees, is still unpinned after the lock is released, so no
  lookup waits for it (found by `test/concurrent_refresh.py`, preloaded).

- **`compact` could fail with `data_corrupted` (XX001), "vector has 0
  bytes", when a vector bound to the version it compacts was dropped
  while it ran.** It now locks each vector before moving it and skips one
  dropped since it listed them (found by `test/concurrent_refresh.py`). A
  vector that `reassign_owned` gave to another role while `compact` ran
  is moved under that role (harness test `compact_reassign_race`). Under
  REPEATABLE READ or SERIALIZABLE either change fails the compaction with
  `serialization_failure` (40001) instead: retry it.

- **`DISCARD ALL` frees the backend's transient vectors, of every role,
  and forgets `last_stats()`.** It left both, so behind a session pooler
  (PgBouncer's `server_reset_query`), the next client on a server backend
  could read the transient vectors and the stats the last client left,
  and the vectors stayed until the backend exited (found by
  `test/pooler_soak.py`; regression test `test/sql/discard_all.sql`).

- **A persisted vector dropped by another backend no longer stays in the
  cache of the backend that wrote it.** That backend pruned dead vectors
  only when it next loaded one privately; with the shared cache serving
  its reads it never did, so behind a transaction pooler, where a vector
  is often written on one server backend and dropped on another, each
  backend kept the vectors it wrote until `warren.max_cache_memory` (4 GB
  by default) evicted them: 8 MB a minute per backend in the pooler
  soak. A write now prunes as a load does (found by `test/pooler_soak.py`).

- **An ERROR raised while pg_warren read a query through a cursor that
  had a parallel plan running no longer crashes the server** (SIGSEGV,
  which restarts every backend). Closing the cursor as the ERROR unwound
  detached the parallel query's shared memory before the
  (sub)transaction's abort ended the parallel query, which then read the
  unmapped segment. The cursors pg_warren reads (the node and edge
  queries, `gather`'s messages, the other queries a function is given,
  and segment chunks) are now left open on an ERROR, for the abort to
  close in Postgres's own order (DESIGN.md §6.9;
  found by the SQL fuzzer: a `statement_timeout` during
  `label_propagation_majority` under `debug_parallel_query`).

### Performance

- A relation whose edges start from at most about a sixth of the nodes
  (an eighth above 2³² nodes) stores its rows' ids and an offset per row
  (12 bytes a row, 16 above 2³²) instead of 8 bytes per node, in each
  direction on its own; the merged CSR is unchanged (DESIGN.md §5.3).
  FB15k-237 (237 relations, 14.5K nodes, 310K edges) now takes 8.0 MB
  where it took 60.5 MB, refreshes in about 0.2 s instead of 0.45 s and
  loads cold in 60 ms instead of 113 ms. Reads restricted to relations
  pay a binary search per row looked up: on FB15k-237, a BFS over its 10
  largest relations takes 1.1× as long and an SpMV over them 1.2–1.4×;
  reads over all relations are unchanged.
- Without the merged CSR (`store_merged := false`), BFS and `components`
  read the relations' rows in ascending order; on FB15k-237 they are
  1.5–2.2× faster than in 0.6.0, and the version ten times smaller, but
  an all-relations `spmv` takes 1.6–2× as long (docs/limits.md).
- `triangles` (and `clustering`, which counts them) orients each edge
  from its endpoint of lower degree and finds each triangle once, about
  M·√M at worst, where it intersected every neighbour row of every node:
  on SNAP's facebook graph 156 → 7.0 ms and on Hetionet
  4.09 → 0.116 s, and on a skewed (R-MAT) graph of 2M edges 14.6 →
  0.53 s on one thread. It holds 8 bytes per stored edge, half the
  symmetric adjacency it built before. Counts are unchanged.
- `contract` finds which of a pair's entries count in one sweep of the
  rows, so `louvain` and `leiden` on graphs with many relations are not
  slowed by counting a pair once: on FB15k-237 (237 relations) they take
  0.42 s and 1.49 s, against 0.70 s and 1.84 s in 0.6.0 (YAGO3-10 0.80 /
  2.49 s against 1.26 / 3.30 s; Hetionet 1.61 / 3.05 s against 1.77 /
  2.78 s; medians of 5). The partitions differ from 0.6.0's where pairs
  are linked in several relations (under Behaviour changes).
- `walk` and `neighbours` hold only the current row's external ids: over
  50,000 IRIs, a cursor on one held 3.7 MB (`neighbours`, `walk` ends) or
  17.9 MB (`walk` paths) after its first row, and now holds 0.09 MB,
  against 0.05 MB over 1,000.

## 0.6.0 — identity, lifecycle and visibility (2026-10-24)

Milestone M10 of DESIGN.md §13. Nodes can be keyed on strings: `text` is
a fourth id type, for IRIs and slugs, and every function takes and
returns `text` ids with no new signature. A graph's ownership can be
transferred (`warren.set_owner`), and the graphs, vectors and reader
entries of a dropped role no longer linger unseen: `reassign_owned` and
`drop_owned` run before `DROP OWNED`, a superuser may opt into a `DROP
OWNED` trigger, and `orphans()` and `drop_orphans()` find what is left. A
graph's owner can bound the vectors other roles persist on it. A
monitoring query can see what each backend's cache holds and counts
(`warren.cache()`, `warren.backends()`), and how far each long call has
got (`warren.progress()`). In the 100M-edge soak with IRIs as node ids, a
text graph's version is 28% larger than with `bigint` ids and builds in
1.39× the time; `pagerank` and `eigen` are within 12%, and the
community detections within 3%. The build itself, for every id type,
now maps edge endpoints on the compute pool, and the `bigint` graph
builds in 79 s where 0.5.0's soak took 173 s (under Performance).

### Upgrading

- **If `pg_warren` is in `shared_preload_libraries`, restart the server**
  after installing the new build. The postmaster keeps the library it loaded
  at start, and every backend inherits it, so until a restart the server runs
  the old code against the updated catalog. Without the preload, new sessions
  load the new library.
- **Run `ALTER EXTENSION pg_warren UPDATE;` in every database that has the
  extension**, after installing the new build and before using the new
  version. The new library is loaded at once, but the catalog changes only
  with the update, so calls in between can fail. The update:
  - creates twelve functions: `set_owner`; `reassign_owned`,
    `drop_owned`, `on_drop_owned`, `orphans` and `drop_orphans`;
    `set_vector_quota` and `vector_usage`; `cache` and `backends`;
    `progress` and `report_progress` (all under New);
  - adds the column `warren.graphs.reader_vector_quota` (existing graphs
    have no quota), the table `warren.vector_quotas` (dumped with the
    extension) and an index on `warren.vectors (graph_id, created_by)`;
  - replaces the library's PL/pgSQL functions, `aggregate_neighbours`,
    `components_vector`, `eigenvector_centrality`, `hits`,
    `katz_centrality`, `label_propagation`, `label_propagation_majority`,
    `pagerank`, `sssp_vector`, `louvain` and `leiden`, with the same
    signatures, so that they report their progress. Their results are
    unchanged.
- The update never creates the `DROP OWNED` event trigger: a superuser
  opts into it (under New).
- `text` ids change nothing in the catalog. Existing `integer`, `bigint`
  and `uuid` graphs, their segments and their shared versions are
  unchanged, and read, refresh and compact as before. A build older than
  0.6.0 cannot read a `text` graph, and downgrading is not supported.
- The shared cache's registry is unchanged, so on PostgreSQL 17 and 18
  versions stay shared across the upgrade without a restart.

### Behaviour changes

- `refresh` maps each batch of edge rows' endpoints to dense ids on the
  compute pool, on up to `warren.threads` threads, for every id type; the
  main thread still reads the rows. The edge list, and so the version, is
  the one the row-by-row intake made. Each batch is charged to
  `warren.max_build_memory`.
- `grant`, `revoke` and `drop_graph` take the graph's row lock, as
  `refresh` and the other writers do, so they wait behind a running
  `refresh` of the same graph (under Fixes).
- `components`, `minimum_spanning_forest`, `vector_topk` and
  `most_similar` make each row's external ids as the row is returned,
  rather than all rows before the first. Their rows and order are
  unchanged. Such a call keeps the version it reads, private or mapped
  from the shared cache, until its last row is read or it ends: a cursor
  on one, read across statements, keeps the version after the statement
  that opened it, even once it is evicted (under Performance).

### New

- **`text` node ids** (DESIGN.md §5.7, §14 items 41 and 42;
  docs/semantics.md, "Text ids"). A graph's node query
  may return `text` or `varchar` ids, such as IRIs or slugs; its id type
  is then `text`. Every function that takes or returns ids does so in
  `text`, with no new signature: a string literal needs its cast
  (`warren.neighbours('kg', 'https://example.org/a'::text)`), `ARRAY['…']`
  is already `text[]`, and the type witness is `NULL::text`.
  - Ids compare bytewise in the database encoding (`COLLATE "C"` order),
    which is SQL's own equality under every deterministic collation. An
    id column whose collation is nondeterministic is refused by
    `create_graph` and `refresh` (42804, hinting `COLLATE "C"` or
    `lower()`), as are `bpchar`, `name` and `citext` ids, each with the
    cast to write. There is no length limit beyond memory.
  - A text version stores each id's bytes once, in dense-id order, with
    their offsets and their sorted order: at 10M nodes 120 MB plus the
    ids' bytes, against 240 MB for `bigint`. It is shared across backends
    and read by parallel workers like any other version. One restored
    into a database of another encoding is refused (55000).
  - Calls that map more than 65,536 ids (`vector_from_query`, `gather`'s
    messages, `similarity`'s pairs) build a hash index of the id map once
    per call, charged to `warren.max_compute_memory` (80 MB at 10M
    nodes), and keep the binary search if it does not fit.
- **`warren.set_owner(name, owner regrole)`** gives a graph a new owner, as
  `ALTER MATERIALIZED VIEW … OWNER TO` does (DESIGN.md §5.7, §14 item 43).
  The caller must be a member of the current owner and able to `SET ROLE`
  to the new one (on PostgreSQL 15, a member of it). The source queries
  are opened as the new owner first, so one that cannot read them is
  refused (42501), as are queries whose columns no longer fit (42804).
  Versions and vectors stay as built until the next `refresh`; readers are
  kept, except the new owner; the old owner keeps no access unless granted
  (docs/semantics.md, "Access"). `ALTER EXTENSION pg_warren UPDATE`
  creates it.
- **Cleanup after a dropped role** (DESIGN.md §5.7, §14 items 44 and 45;
  docs/semantics.md, "Dropping a role"). `DROP ROLE` cannot see the roles
  a graph records, and `REASSIGN OWNED` moves no graph, so:
  - `warren.reassign_owned(old, new)` and `warren.drop_owned(role)`, SQL
    counterparts of `REASSIGN OWNED` and `DROP OWNED` for graphs, their
    vectors and reader entries, each needing the privileges of the role
    as the commands do. Run one before `DROP OWNED` in each database.
  - `warren.on_drop_owned()`, an event-trigger function a superuser may
    opt into with `CREATE EVENT TRIGGER … ON ddl_command_start WHEN TAG IN
    ('DROP OWNED')`. With it, `DROP OWNED` is refused (2BP01) while a role
    it names owns graphs, naming them, and otherwise also removes the
    role's reader entries and vectors. The extension never creates the
    trigger, so trusted installs work without it; while it exists, `DROP
    EXTENSION` needs `CASCADE`, and a non-superuser's restore of a dump
    fails at that statement.
  - `warren.orphans()` and `warren.drop_orphans()` list and remove graphs,
    reader entries and vectors that name roles dropped without any of the
    above.
  - A graph whose owner was dropped may now be dropped by the extension's
    owner (or a member with its privileges or the right to `SET ROLE` to
    it) as well as by a superuser (`drop_graph`, `drop_orphans`), and adopted by a superuser with `warren.set_owner`,
    which refused it before ("the owner no longer exists").
  - A `drop_vector`, a replacement or `compact`'s move that checked a
    vector's creator, and then waited behind a `reassign_owned` that changed
    it, fails with 40001 instead of deleting the new creator's vector.

  `ALTER EXTENSION pg_warren UPDATE` creates the five functions.
- **Cache observability** (DESIGN.md §5.7, §14 items 48 and 49).
  - `warren.cache()` lists this backend's private cache: each cached
    version, persisted vector and transient vector of the caller's role,
    with its bytes and state (`in use`, `pinned`, `idle`, `held`), then a
    `backend` row with the total against `warren.max_cache_memory`. Only
    graphs the caller may read are listed.
  - `warren.backends()` has a row per backend that has used pg_warren,
    with what it holds privately and maps from the shared cache, and its
    cache counters (lookups, hits, waits, loads and their bytes and
    seconds, evictions, and refusals by reason), counted once per
    statement and version; then a `cluster` row that includes backends
    that have exited. Another role's backend shows only who it is, unless
    the caller has that role's privileges or `pg_read_all_stats`, as in
    `pg_stat_activity`; a backend's role is its authenticated user, as
    `pg_stat_activity`'s `usesysid`, whatever `SET SESSION AUTHORIZATION`
    sets. Parallel workers have no row. It needs no configuration on PostgreSQL 17 and 18
    and `shared_preload_libraries` on 15 and 16; without it, the caller's
    own row only, with a `NOTICE`.
  - A backend waiting for another's load of a shared version shows
    `wait_event` `WarrenSharedLoad` in `pg_stat_activity` on PostgreSQL 17
    and 18 (`Extension` on 15 and 16).

  `ALTER EXTENSION pg_warren UPDATE` creates both functions; the shared
  cache's registry is unchanged, so on PostgreSQL 17 and 18 versions stay
  shared across the upgrade without a restart.
- **Progress** (DESIGN.md §5.7, §14 item 49).
  - `warren.progress()` has a row per backend running a long call: its
    command (the outermost call, such as `refresh`, `compact`, `eigen` or a
    library function) and the native call running inside it now, each
    with its graph, version, phase, units done of a total, start time and,
    for the native call, named counters. A `refresh` shows `reading
    nodes`, `reading edges` (rows of the planner's estimate), `building`
    (CSRs), `writing` and `promoting`; a load `loading` (bytes); `eigen`
    and `distance` their Lanczos steps, with the pairs converged;
    `louvain` and `leiden` their levels, with local moving's sweeps and
    refinement inside. Another role's row shows what runs and since when,
    not how far, unless the caller has that role's privileges or
    `pg_read_all_stats`; a graph's name shows only to callers who may
    read the graph. Other backends' rows need no configuration on
    PostgreSQL 17 and 18 and `shared_preload_libraries` on 15 and 16.
  - `warren.report_progress(command, graph, phase, done, total)` reports a
    PL/pgSQL program's own progress until the statement ends. The library's
    iterative functions call it once per iteration, level or hop.

  `ALTER EXTENSION pg_warren UPDATE` creates both functions and replaces
  the library's.
- **Quotas on readers' vectors** (DESIGN.md §5.7, §14 items 46 and 47;
  docs/semantics.md, "Quotas on readers' vectors"). A graph's owner may
  bound the persisted vectors each other role stores on the graph, counted
  as their elements' bytes across all versions:
  `warren.set_vector_quota(name, bytes, role DEFAULT NULL)` sets the
  graph's default (`info`'s new `reader_vector_quota`) or a role's own
  quota, and `warren.vector_usage(name)` shows each role's vectors, bytes
  and quota (a reader sees its own row). A persisted write that would take
  the writer past its quota, and above what it stored, is refused with
  53400; replacing a vector with one no larger always works. Members of
  the owner role are exempt, and transient vectors are not counted. One
  role's writes to one graph take a transaction-scoped lock on (graph,
  role), shown in `pg_locks` as `advisory` with `objsubid` 22353.
  `reassign_owned` counts the vectors it moves against the new role's
  quotas. `drop_owned` and the `DROP OWNED` trigger remove a role's quotas
  only on graphs whose owner the caller is a member of; `orphans()` and
  `drop_orphans()` handle those of dropped roles. A quota bounds a role's
  vectors on other owners' graphs, not the catalog's total storage. `ALTER EXTENSION pg_warren UPDATE` adds
  the column, the table `warren.vector_quotas` (dumped with the
  extension), an index on `warren.vectors (graph_id, created_by)` and the
  two functions; existing graphs have no quota.

### Fixes

- `refresh`, `compact`, `promote`, `drop_version`, `grant`, `revoke`,
  `drop_graph` and `contract` into an existing quotient check ownership
  again once they hold the graph's row lock (`grant`, `revoke` and
  `drop_graph` now take it). They checked only before it, so a call queued behind an ownership
  transfer ran with an ownership its caller no longer had (harness test
  `set_owner_race`).

### Performance

- **The 100M-edge soak with `text` ids** (DESIGN.md §12, "Text node ids";
  `bench/results/soak/a-text-m10.json` and `a-m10.json`; PG 16, 4 vCPUs,
  `warren.threads = 4`, private cache). Case a with IRIs of 33–36 bytes as
  node ids, against the same graph with `bigint` ids, every stage run:
  - the version is 1.44 GB against 1.12 GB, and `refresh` takes 110.2 s
    against 79.1 s, peaking 0.25 GB higher;
  - calls that return a row per reached node build each id's `text` datum:
    `bfs` (1.47M rows) 1.42 s against 0.66 s, `components` 10.6 s against
    5.2 s, `reachable_set` 1.42× as long;
  - `similarity` of 100,000 pairs builds its per-call index: 2.15 s
    against 0.47 s;
  - `pagerank` and `eigen` are within 12%; `eigenvector_centrality`,
    `katz_centrality` and `hits` take 1.2–1.4× as long, the rest of the
    library and `louvain` and `leiden` within 8%, with the same modularity
    to 0.001.

  §12's targets missed there, for both id types: `reachable_set` (13 and
  19M edges/s, against 50M) and `bfs` against the recursive CTE (5.2× and
  10.5× faster, against 20×); with `text` ids, `components` also misses
  its 10 s.
- **The build maps edge endpoints in parallel** (under Behaviour changes).
  In the same soak the `bigint` graph builds in 79.1 s from its table
  (0.5.0's soak: 173.1 s), 99.1 s from the generator query (184.5 s) and
  88.3 s again with 0.1% fewer edges (165.3 s), on the same machine.
- **0.5.0's fixes to level graphs and Leiden's refinement at 10M nodes**,
  run at that scale for the first time in the same soak: on the
  one-relation graph with little community structure, `leiden` completes
  in 804 s (16 levels, modularity 0.186, an 8.10 GB peak), where 0.5.0's
  soak was refused (53400), and `louvain` takes 528 s (627.5 s). The two
  wrote 13.1 and 4.6 GB of level segments and 15.1 and 6.1 GB of WAL,
  held until the run's transaction ends (docs/limits.md).
- **Rows made as they are returned** (DESIGN.md §12): `components`,
  `minimum_spanning_forest`, `vector_topk` and `most_similar` no longer
  hold every row's ids until the call ends. At 5M nodes and 50M edges,
  `components` peaks at 0.84 GB with `text` ids (1.28 GB before) and
  0.73 GB with `bigint` (0.80 GB), and `minimum_spanning_forest` at
  1.10 GB with `text` (1.77 GB), its `bigint` peak unchanged; times are
  unchanged. `walk` and `neighbours` still build their rows first, bounded
  by their arguments or by one node's degree (M11).
- Progress reporting costs a few atomic stores per statement and per
  poll: within noise for one `degree` per transaction and for `pagerank`,
  and about 1.5 µs a `report_progress` call.

## 0.5.0 — the shared cache (2026-10-23)

Milestone M9 of DESIGN.md §13, the first on the road to 1.0. A loaded
graph version, and a persisted vector, is kept once per cluster in dynamic
shared memory and mapped by every backend that reads it, instead of once
per backend: behind PgBouncer, 50 pooled connections reading a 112 MB
version hold 338 MB instead of 6.63 GB, and a pool's cold start is one
load (DESIGN.md §12, `bench/results/pooler/`). It is on by default, with
nothing to configure on PostgreSQL 17 and 18 and
`shared_preload_libraries` on 15 and 16. With it the graph reads that use
no vector are `PARALLEL SAFE`, so a scan that calls one per row runs in
parallel workers. The 100M-edge soak, re-run with the shared cache, reads
the shared version in every query stage, with each backend's private
memory lower by the version's size; where 0.4.0 reloaded a version larger
than `warren.max_cache_memory` on every statement, it now reads it in
place (under Performance).

### Upgrading

- **Run `ALTER EXTENSION pg_warren UPDATE;` in every database that has the
  extension**, after installing the new build and before using the new
  version: the update creates `warren.shared_cache()`, replaces
  `warren.gather` and `warren.label_propagation_majority`, and marks the
  graph reads `PARALLEL SAFE` (below). The new library is loaded at once,
  but the catalog changes only with the update, so calls in between can
  fail.
- The update drops and recreates `warren.gather` (it gains
  `include_self`; grants on it are kept). A view or a `BEGIN ATOMIC` SQL
  function that calls `warren.gather` depends on it and makes `ALTER
  EXTENSION pg_warren UPDATE` fail: drop it before the update and create
  it again after. Functions with a string body (PL/pgSQL, or SQL written
  as `AS $$ … $$`) do not depend on it and need nothing.
- Until the update, `warren.gather` keeps its 0.4.0 definition, which
  calls the new library without `include_self`; such a call behaves as
  `include_self := false`, as in 0.4.0.
- **The shared cache is on by default.** A loaded graph version is kept
  once per cluster in dynamic shared memory, up to
  `warren.shared_cache_memory` (default `4GB`, a server setting: set it in
  `postgresql.conf` or with `ALTER SYSTEM` and reload; `0` turns it off),
  and mapped by every backend that reads it. On PostgreSQL 17 and 18 it
  needs nothing configured. On 15 and 16 it needs
  `shared_preload_libraries = 'pg_warren'` and a restart; without it,
  backends load their own copies as in 0.4.0, silently at the default cap.
  Shared versions are files in `/dev/shm`: a version that does not fit
  there with a quarter of it left free is loaded privately (with a
  `NOTICE` only if you set the cap yourself; `warren.shared_cache()` shows
  what is shared), so in a Docker container (64 MB by default) raise
  `--shm-size` (docs/limits.md, "The shared cache"). An idle cluster keeps
  up to the cap of shared versions until they are evicted or it restarts.

### Behaviour changes

- **Graph reads can run in parallel workers** (DESIGN.md §14 item 33):
  `neighbours`, `degree`, `has_edge`, `id_map`, `info` and the traversals
  `reachable`, `shortest_path`, `dijkstra`, `bfs`, `reachable_set`,
  `components` and `walk` are `PARALLEL SAFE` (they were `PARALLEL
  RESTRICTED`), so a scan that calls one per row can run in parallel: at
  1M rows, `has_edge` per row takes 3.7 s with 2 workers and 2.9 s with 4
  with the shared cache, against 9.8–10.3 s in the leader alone. A worker
  maps the shared version; wherever a version is read privately, **each
  worker loads its own copy**: on PostgreSQL 15 and 16 without
  `shared_preload_libraries = 'pg_warren'` or with the shared cache off,
  and also for a version larger than `warren.shared_cache_memory`, one the
  shared cache has no room for (Docker's 64 MB `/dev/shm`, for one), and
  one the current transaction created. Each copy is within the worker's
  `warren.max_cache_memory` and `warren.max_compute_memory` and held until
  the query ends, so a plan can hold one per worker plus the leader's, and
  a short query forced parallel can be slower (1.8–3× over 10k rows
  without the preload). `last_stats()` reports only calls the leader ran:
  a traversal in a worker records nothing; and the cache's `NOTICE`s are
  raised by the leader only. To avoid parallel plans, any role can `SET
  max_parallel_workers_per_gather = 0` (per session, transaction or role);
  a superuser can keep 0.4.0's behaviour for a function with `ALTER
  FUNCTION … PARALLEL RESTRICTED` (docs/semantics.md, "Parallel query
  labels"). Vector reads, `explain_map`, `diff`, `distance`, `prewarm`,
  `evict` and `last_stats` stay `PARALLEL RESTRICTED`. `ALTER EXTENSION
  pg_warren UPDATE` changes the labels.
- A level graph (`contract(graph, labels, NULL)`, the graphs `louvain` and
  `leiden` contract each level into) stores each relation's forward CSR
  only, without a reverse or merged CSR whatever the input's layout. The
  partitions are the same; on a directed graph each level writes about
  half the segments and WAL (at 1M nodes and 10M random edges, `louvain`'s
  levels 293 MB instead of 536 MB, `leiden`'s 945 MB instead of 1.76 GB).

### New

- The shared cache (DESIGN.md §5.6): `warren.shared_cache_memory`. A
  version, or a persisted vector, is loaded once per cluster by the first
  backend that misses it (the others wait for that load) and is mapped by
  every backend that reads it; it is counted by neither
  `warren.max_cache_memory` nor `warren.max_compute_memory`. Entries that
  no statement uses are evicted least recently used first to make room; a
  version that does not fit is loaded privately, as before. A promotion,
  `drop_version`, retention and `warren.evict` by the graph's owner evict
  shared versions.
- `warren.shared_cache()` (`PARALLEL SAFE`): what the shared cache
  (`warren.shared_cache_memory`) holds. A row per shared version and
  persisted vector of a graph the caller may read (its state, bytes, pins,
  mappers and last use), then one `cluster` row of totals across every
  database, with the cap. A role sees only graphs it may read (superusers
  read all); dropped entries are listed to no one. `ALTER EXTENSION
  pg_warren UPDATE` creates it.
- Persisted vectors are shared across backends like versions: loaded once,
  mapped by every backend that reads them, and charged to no call's
  `warren.max_compute_memory`. `bool` vectors and vectors written by the
  reading transaction are still read privately. Dropping or replacing a
  vector evicts it, and `warren.evict` by the graph's owner evicts its
  shared vectors.
- `warren.gather` takes `combine := 'mode'`: the most frequent message, the
  smallest on a tie (integer, float and boolean messages), and
  `include_self := false`: with `true`, each receiver also receives its own
  messages, ahead of its senders'. `ALTER EXTENSION pg_warren UPDATE`
  replaces `gather`.

### Fixes

- `leiden_refine` (and so `leiden`) charged 88 bytes a node to
  `warren.max_compute_memory` up front, every community's state at once,
  and was refused (53400) in the first refinement of a 10M-node,
  100M-edge random graph at the default limit. It now charges what it
  holds: 24 bytes a node beside the result's label vector (8 bytes a
  node), then 48 a community and the state of the communities being
  refined, at most one a thread.
- After `DROP EXTENSION pg_warren` and `CREATE EXTENSION pg_warren` in a
  database, a backend that had read graphs before could serve the old
  installation's cached versions and vectors for the new installation's
  (version, graph and vector ids restart). A backend now drops what it
  cached when pg_warren is installed again.

### Performance

- **The 100M-edge soak with the shared cache** (DESIGN.md §12,
  `bench/results/soak/*-m9-shared.json`; PG 16 preloaded, 4 vCPUs). Every
  query stage reads the shared version, and a backend's private memory
  falls by the version (case a's stages peak at 0.01–3.11 GB private, from
  1.22–4.31 GB in 0.4.0). On the 6.0 GB five-relation graph with its
  merged CSR, at an 8GB `warren.shared_cache_memory` and the default
  `warren.max_cache_memory`, which no longer counts the version, `bfs`
  takes 0.89 s (25.1 s in 0.4.0, which reloaded it each statement),
  `components` 7.6 s (28.6 s) and `pagerank` 16.5 s (38.4 s), and `eigen`,
  refused there before (53400), runs. On the one-relation graph `louvain`
  completes (627.5 s, five levels) where 0.4.0's soak stopped it for disk
  at 383 s. Other stages are within the
  machine's run-to-run spread; a build is private, so `refresh` is
  unchanged. The soak ran before the two fixes to level graphs and
  Leiden's refinement (under Behaviour changes and Fixes), so its
  `leiden` refusal (53400) at 10M nodes is 0.4.0's; neither fix is re-run
  at that scale yet (docs/limits.md).
- Every graph read checked the caller's access with a catalog query planned
  on each call; it now reads the role caches. A point lookup such as
  `warren.degree` takes about a seventh of the time (200,000 calls in one
  statement: 2.1 s, from 14.5 s), and `pgbench` of one `degree` per
  transaction with 4 clients runs at about 140,000 tps, from 35,000.
- **`vector_map`'s no-NULL path** (`warren-core`'s `compute::map`;
  DESIGN.md §6.3). An expression built only from vectors, non-NULL
  literals, casts, arithmetic, comparisons, `NOT` and `CASE … ELSE` is
  never NULL. It is now evaluated without NULL flags, with each operator
  chosen once per chunk rather than per element. Results are
  bit-identical, and any error is still reported by the general path.
  Measured in `warren-core` at 4M nodes and 40M edges
  (`benches/spmv_out_of_cache.rs`), on a shared 4-vCPU machine, so the
  spread is wide, and not re-measured through SQL at 10M nodes:
  - `pagerank`'s four maps: 2.1–2.7× faster on one thread (340–390 ms
    before, 145–165 ms after);
  - a whole iteration: 1.2–1.4× faster on one thread (790–840 →
    580–665 ms), 1.3–2.2× on four (480–550 → 250–375 ms).
- **Multi-source BFS for unweighted closeness and harmonic centrality**
  (`warren-core`'s `compute::centrality`; DESIGN.md §6.7). Sources are
  searched 64 at a time, one bit per source, each step in parallel over
  the frontier, with 40 B of scratch per node.
  - Measured at 1M nodes / 10M edges on 4 threads
    (`benches/centrality.rs`): 64 sources of closeness or harmonic in
    0.65–1.23 s instead of 10.1–20.7 s (9–32× faster).
  - Closeness is bit-identical to before. Harmonic sums in a new,
    deterministic order, within 1e-12 relative of the old result.
  - Weighted graphs (Dijkstra) and betweenness are unchanged.
- `label_propagation_majority` counts its votes in-engine, with one
  `gather` (`mode`, `include_self`) per iteration instead of counting
  every received label in the executor: at 1M nodes and 20M stored edges
  an iteration takes ~3 s, against ~37 s before on the same machine. Its
  results are unchanged.

## 0.4.0 — locality, the library, and communities (2026-10-22)

Milestone M8 of DESIGN.md §13. Dense ids can be ordered for locality; the
algorithm library gains the planned groups 1 and 2 (centralities, HITS,
clustering, majority-vote label propagation, k-cores, strong components,
DAG order and longest paths, spanning forests, sampled shortest-path
centralities, neighbourhood similarity); and contraction gives
community-level graphs, with Louvain and Leiden written over it in
PL/pgSQL. The 100M-edge soak was re-run: no §12 row moved beyond the
machine's run-to-run spread, and Louvain and Leiden have known limits at
10M nodes (under Performance).

### Upgrading

- **Run `ALTER EXTENSION pg_warren UPDATE;` in every database that has the
  extension**, after installing the new build and *before* calling any
  `warren.*` function. 0.4.0 changes the schema: the update adds the column
  `warren.graphs.node_order` (existing graphs are backfilled `'id'`, the
  order they were built in) and the `node_order` arguments of
  `create_graph` and `compact`, and replaces `pagerank` (a fix below). For
  contraction it adds a `CHECK` on `warren.graphs.source`, makes
  `warren.versions.parent_version` `ON DELETE SET NULL`, indexes
  `versions.parent_version` and `versions.label_vector`, and adds the
  trigger that clears `versions.label_vector` when that vector is deleted
  (its trigger function `warren.label_vector_is_forgotten()` is revoked
  from `PUBLIC`). It creates every new function below. The new library is
  loaded at once, but the catalog changes only with the update, so calls in
  between can fail.
- `create_graph` and `compact` are dropped and re-created by the update
  (each gains an argument). Unlike 0.3.0's update, this one grants again
  every privilege that was set on them, so a `GRANT` or `REVOKE` you made
  on `warren.create_graph` or `warren.compact` survives it, in the catalog
  and in `pg_dump` output. The grants are recorded as made by the
  functions' owner: the original grantor, and so grant-option chains, are
  not kept.

### Behaviour changes

- **Graph names beginning with `_warren_` are reserved** for the level
  graphs of `louvain` and `leiden`: `create_graph` and `contract` refuse
  them with 42939 (`reserved_name`), as vector names with that prefix
  already were. Graphs so named before 0.4.0 keep working.
- `compact` is no longer `STRICT`: `compact(NULL)` raises 22004
  (`null_value_not_allowed`, `name must not be NULL`) instead of returning
  NULL.
- Errors from `warren-core`'s graph structures in `compact` and `diff` now
  carry their own SQLSTATE rather than 22023: running out of memory is
  53200 (`out_of_memory`), the build memory limit 53400, a size limit
  54000 (`program_limit_exceeded`), and a corrupt or inconsistent version
  (an internal error) XX001 (`data_corrupted`). `diff`'s refusals across a
  compaction, and other version mismatches, stay 22023.

### New

- **Locality ordering of dense ids** (DESIGN.md §14 item 22):
  `create_graph(…, node_order := 'locality')` (default `'id'`; anything
  else is refused with 22023, `invalid_parameter_value`) numbers the first
  build's nodes so that neighbours get nearby dense ids (Reverse
  Cuthill–McKee over the undirected view of the edges), so the per-node
  data a kernel reads at each edge shares cache lines. Ids are ordered only
  where they may change: the first build and `compact`. Later refreshes
  keep surviving ids and append new nodes, so locality decays as the graph
  changes; `compact` restores it. Until a graph is first promoted each
  staged first build gets its own order, so `diff`, `distance` and
  `vector_norm_diff` refuse two whose orders differ (22023). Results are
  the same in either order, read by external id. `warren.info` reports
  `node_order`. The ordering's scratch is charged to
  `warren.max_build_memory`; it is not cancellable (about 10 s at 10M
  nodes and 100M edges). Gains are under Performance.
- **`compact(name, node_order DEFAULT NULL)`:** compacts in the given order
  (NULL: the graph's own); an order given becomes the graph's, so later
  compactions follow it. This is how an existing graph gets locality order.
  The order's scratch is charged to `warren.max_build_memory` (53400 past
  it). Persisted vectors move by node, as before.
- **Library, group 1 (DESIGN.md §6.7)**, PL/pgSQL over the primitives like
  `pagerank`, each checked against NetworkX on random graphs:
  `eigenvector_centrality(graph, out, tol, max_iter, rels, weighted,
  version)`, `katz_centrality(graph, out, alpha, beta, seed, normalized,
  tol, max_iter, …)` (a per-node β from a seed vector) and `hits(graph,
  hubs, authorities, tol, max_iter, normalized, …)` (two vectors),
  following NetworkX's iteration, normalisation, defaults and stopping
  rule, so they agree to rounding; `clustering(graph, out, rels,
  version)`, the local clustering coefficient from `triangles` and
  degrees; and `label_propagation_majority(graph, out, seed, max_iter,
  rels, version)`, a deterministic, synchronous majority-vote label
  propagation (smallest label on a tie; NetworkX's is random or
  semi-synchronous, so it is checked against a Python reimplementation of
  its rule). Where NetworkX raises on non-convergence, these write the last
  iterate with a `WARNING`; a run whose values stop being finite (Katz with
  `alpha` too large for the graph) is an error, 22003, and writes nothing.
  NULL arguments are refused with 22004.
- **Structural kernels** (DESIGN.md §6.7, planned library group 2), each
  checked against NetworkX on a random graph with self-loops, parallel
  edges in one relation and across two, a deleted node and, for the DAG,
  negative weights (`test/sql/oracle_structure.sql`):
  - `core_number(graph, out, rels, persist, version)`: each node's k-core
    number in the undirected simple graph (Batagelj–Zaversnik peeling);
    returns the degeneracy.
  - `strong_components_vector(graph, out, rels, persist, version)`: each
    node's strongly connected component, labelled by its smallest dense id
    as `components_vector` labels weak ones (iterative Tarjan, so long
    paths need no recursion); returns the component count.
  - `topological_order(graph, out, rels, persist, version)`: each node's
    position in the topological order that takes the smallest ready dense
    id first (Kahn), NetworkX's `lexicographical_topological_sort`;
    returns the vector id.
  - `dag_longest_path(graph, out, rels, weighted, persist, version)`: the
    longest path ending at each node, as `nx.dag_longest_path`; returns the
    longest. Both DAG functions raise 22023 on a cycle.
  - `minimum_spanning_forest(graph, id_type, rels, weighted, version)`:
    rows `(source, target, weight)` of a minimum spanning tree of each
    component, directions ignored (Borůvka).

  They run on one compute thread, charge their scratch to
  `warren.max_compute_memory` and can be cancelled.
- **Shortest-path centralities** (DESIGN.md §6.7, planned library group
  2): `betweenness_centrality(graph, out, k, seed, normalized, endpoints,
  directed, sources, rels, weighted, persist, version)`,
  `closeness_centrality(…, wf_improved, …)` and `harmonic_centrality(…)`
  write an `f64` vector and return their number of sources. Exact with
  `k := NULL` (the default: every live node a source); otherwise from `k`
  sources sampled with `seed` (default 0; the same sample on any thread
  count, and after a refresh unless a new node's key displaces a sampled
  one or a sampled node is deleted; `compact` re-keys every node), or from
  the nodes a width-1 vector `sources` selects (non-zero values, as a
  mask). Brandes' algorithm, breadth-first or (weighted: every edge
  followed must weigh more than 0 and every distance stay finite, else
  22023; self-loops are ignored) Dijkstra, along edges or (`directed :=
  false`) in the undirected simple graph, one source per compute thread at
  a time, summed in source order so results are bit-identical on any
  thread count. Betweenness is rescaled exactly as NetworkX 3.7 (including
  its sampling adjustment); closeness (with `wf_improved`) and harmonic
  centrality use incoming distances as NetworkX does, and from a sample
  are estimated from each node's sampled sources (docs/compute.md has the
  estimator and when its error bound holds). Checked against NetworkX
  given the same sources (`test/sql/oracle_paths*.sql`). Exact forms are
  for small graphs: a source costs 20–30 s on one thread at 10M nodes.
- **Neighbourhood similarity for link prediction** (DESIGN.md §6.7):
  `similarity(graph, pairs, id_type, measure, rels, version)` scores the
  pairs a query returns, `most_similar(graph, node, k, measure, rels,
  version)` ranks one node's candidates and `similar_pairs(graph, id_type,
  k, measure, nodes, rels, version)` each node's (`k` default 10), by
  `jaccard` (the default), `adamic_adar` or `common_neighbours` over the
  undirected simple graph, as NetworkX's link-prediction functions
  (`test/sql/oracle_similarity.sql`). Candidates are non-neighbours
  sharing a neighbour, best first, ties by dense id; no all-pairs form.
  With reverse CSRs, `similarity` and `most_similar` read only the rows
  they need. A node paired with itself scores Adamic–Adar +∞ where it
  shares a degree-1 neighbour (NetworkX raises).
- **Contraction, projection and lifting** (DESIGN.md §6.6,
  [docs/communities.md](docs/communities.md)):
  - `contract(graph, label_vector, out_graph, weighted, drop_self_loops,
    promote, version)` builds a version of a **quotient graph**: one node
    per distinct label of an integral label vector (its `bigint` node ids
    are the labels), the edges between communities summed by weight or
    counted, the edges inside a community its self-loop (or dropped):
    `PᵀAP` of the stored matrix, so an undirected edge inside a community
    counts in both directions and every community's weighted degree is the
    sum of its members'; relations are kept. The first contraction
    creates the graph (`source` `'contraction'`, owned by the caller);
    later ones add versions, staged or promoted, with stable dense ids per
    label, and must come from the same parent graph (22023). With
    `out_graph := NULL` it creates a new graph named
    `_warren_level_<version id>`. The version records the parent version
    and label vector it came from, and stores the membership. `refresh`
    and `compact` refuse such a graph (42809). Non-integral or NaN labels
    are refused (22023). Its build scratch is charged to
    `warren.max_build_memory`.
  - `project(graph, x, out_graph, y, agg, persist, version)`: each
    community's `sum` (the default), `mean`, `max`, `min` or `count` of
    `x` over its members, row by row for matrices (`mean` of embeddings
    gives centroids); `lift(out_graph, y, graph, x, persist, version)`:
    each node gets its community's row. Both use exactly the version the
    quotient was contracted from.
  - A parent version a quotient version derives from is kept beyond
    `retain_versions` (with a `NOTICE`); dropping it unlinks the quotient
    (`project`/`lift` then fail with 55000). `warren.info` reports the
    graph's `source` (`query` or `contraction`) and each version's
    `parent_version`, `parent_graph` and `label_vector` (NULL to a caller
    who cannot read the parent graph).
  - Checked against NetworkX's `quotient_graph` and SQL `GROUP BY`/joins
    (`test/sql/oracle_contraction*.sql`).
- **Louvain and Leiden** (DESIGN.md §6.7 group 3,
  [docs/communities.md](docs/communities.md)): `louvain(graph, out,
  resolution, seed, max_levels, tolerance, weighted, levels_prefix,
  version)` and `leiden(graph, out, resolution, seed, max_levels,
  tolerance, randomness, iterations, weighted, levels_prefix, version)`
  write a label vector of communities that maximise modularity and return
  its modularity; `levels_prefix` keeps each level's partition. Leiden's
  communities are always connected. **Leiden runs `iterations` passes (2
  by default, as igraph's `community_leiden`)**, each after the first
  starting from the partition the last found, and stops early after a
  pass that changes nothing; a negative value runs until then (at most
  100, then a `WARNING`), and 0 is refused (22023). On the bench's 20
  planted blocks (modularity 0.855) one pass scores 0.794 and two reach
  0.855, as igraph's two do (the planted blocks with one node moved). Both
  are PL/pgSQL over `contract`, `project`, `lift` and two new kernels,
  `local_moving` (a level's moving, parallel and deterministic: the same
  result for any thread count and a given seed) and `leiden_refine`
  (Traag et al.'s refinement); each level's graph is a quotient of `graph`
  made by `contract(graph, labels, NULL)`, and only one exists at a time.
  `modularity(graph, labels, resolution, weighted, version)` scores any
  partition; it counts an undirected self-loop once, as `contract` does:
  on an undirected graph it equals NetworkX's `community.modularity` with
  each self-loop's weight halved (`test/sql/oracle_modularity.sql`), and
  on a directed graph it scores the undirected view (an edge and its
  reverse add up), not NetworkX's directed (Leicht–Newman) modularity.
  Checked against NetworkX's `louvain_communities` on stochastic block
  models (`test/sql/oracle_louvain*.sql`: modularity, NMI against the
  planted partition, connectedness).

### Fixes

- `pagerank` with a `seed` failed with "division by zero" on any version
  with a deleted node: it divided by a constant vector, which is 0 at
  tombstoned rows, and `vector_map` evaluates those rows too. It now
  multiplies by the reciprocal of the seed's sum.

### Performance

Measured on the 4-vCPU Xeon of DESIGN.md §12, release builds,
`warren.threads = 4`, every limit at its default unless stated.

- **Locality order** (`warren-core`'s kernels, `examples/locality.rs`;
  DESIGN.md §14 item 22): at 10M nodes and 100M edges, on a
  community-structured graph whose ids carry no structure, SpMV is 1.51×
  faster on one thread and 1.63× on four, BFS 1.25× and `components`
  1.66×; on a power-law graph 1.04–1.25×. The ordering costs 10–12 s at
  the first build or `compact`. On the soak's uniform random graph, which
  has no locality to recover, it gains nothing measurable through SQL, and
  its first build peaks at 2.51 GB of private memory against 1.55 GB in
  id order (163 s against 160 s; within the 4 GB and 8 min targets).
- **The 100M-edge soak, re-run for 0.4.0** (DESIGN.md §12,
  `bench/results/soak/*-040*.json`): no §12 row moved beyond the
  machine's run-to-run spread, every peak is unchanged to 0.01 GB, and no
  target is newly met. An A/B of the one-relation case against 0.3.0's
  build, three runs each, found no stage reproducibly slower (median
  ratios 0.87–1.05). The five-relation graph without the merged CSR now
  misses the `components()` target (11.8 s against 10.1 s; 10.2 s at an
  8GB cap in the same run), as borderline as before.
- **The new library at 10M nodes and 100M edges** (the soak's
  one-relation graph, 1.2 GB cached; seconds, and the backend's private
  peak with the version): eigenvector, Katz and HITS for 20 iterations
  15.0, 9.8 and 17.3 s (1.8–2.0 GB); `clustering` 47.5 s (3.0 GB);
  `core_number` 22.8 s (3.2 GB); `strong_components_vector` 15.8 s;
  `minimum_spanning_forest` 54.4 s; betweenness, closeness and harmonic
  from 16 sources 61.1, 29.4 and 30.9 s (2.0–2.6 GB); `most_similar` 2 ms;
  `similarity` of 100,000 pairs 0.55 s; `similar_pairs` over every 100th
  node 10.2 s (3.6 GB). With five weighted relations and no merged CSR
  each takes 1.4–3.5× as long; with the merged CSR (6.0 GB, over the
  cache cap) each call also pays the ~22 s load, and `similar_pairs` is
  refused (53400). At 1M nodes and 10M undirected edges the SpMV
  centralities take 40–90 ms an iteration, `clustering` 4.5 s, and
  `label_propagation_majority`, a tier-2 program, about 20 s an
  iteration (docs/limits.md).
- **Louvain and Leiden on a graph with communities** (blocks of 1,000
  nodes, five undirected edges per node, 90% inside the block;
  `bench/results/communities/`): at 1M nodes `louvain` takes 5.8 s and
  one `leiden` iteration 8.3 s (modularity 0.837 each); the default two
  iterations took 20.9 s on a slower day (one iteration 10.4 s there) and
  raise modularity to 0.898 (the planted blocks: 0.899). At 10M nodes, at
  their defaults (`blocks-10m-defaults.json`): `louvain` 115.4 s, 5
  levels, modularity 0.840, peak 4.31 GB private, 2.5 GB of segments and
  2.6 GB of WAL written; `leiden` 246.5 s, 18 levels, modularity 0.8996
  (the planted blocks: 0.8995), peak 5.28 GB, 7.8 GB of segments and
  8.2 GB of WAL written.
- **Known limits at 10M nodes** (docs/limits.md, "Known limitations"):
  - *Leiden on graphs with little community structure is refused.* On the
    soak's uniform random graphs (10M nodes, 100M edges) `leiden` fails
    with 53400 at the default limits in its first level's refinement,
    whose 88 bytes a node do not fit `warren.max_compute_memory` beside
    the 3.2 GB undirected view and the moving state. Raise
    `warren.max_compute_memory` for such graphs.
  - *Louvain's disk.* Every level writes its graph as a version, and a
    level's rows stay on disk until `VACUUM` after the run's transaction
    ends. On a random graph every level's quotient stays nearly as large
    as the graph: the soak stopped `louvain` for disk after 383 s, having
    written 7.5 GB of segments and 9.1 GB of WAL (0.61 GB of segments at
    1M nodes). Even on the 10M block graph (0.72 GB of segments) the
    defaults write 2.5 GB (`louvain`) and 7.8 GB (`leiden`). The planned
    fix is an in-memory `contract` (DESIGN.md §6.7, §13).
  - *A backend's peak during `refresh` or `contract` is bounded by the
    cache, compute and build limits together*, not by the first two: on
    the five-relation graph `louvain` peaked at 9.50 GB private against
    8.59 GB for cache and compute, the rest `contract`'s build scratch (at
    1M nodes, with all three limits at the least it needs, its peak was
    within 1.4% of their sum, that 1.4% Postgres's own memory).
  - On the five-relation graph with its merged CSR (over the cache cap)
    `louvain` and `leiden` are refused (53400): the undirected view does
    not fit beside the version's excess over the cap.
  - As in 0.3.0, BFS through SQL is about 10–11× a recursive CTE rather
    than 20×, `reachable_set` 23–26M rather than 50M edges/s, and `eigen`
    does not converge within 10 minutes on a uniform random graph of this
    size.

### Documentation

- [docs/communities.md](docs/communities.md): contraction, projection,
  lifting, Louvain, Leiden and modularity, with examples that run in CI
  as the other docs' do;
  docs/compute.md covers the group 1 and 2 functions (the sampled
  estimators and their error bounds); docs/semantics.md covers
  `node_order`; docs/limits.md adds node order, the library's timings at
  1M and 10M nodes, contraction at scale, the 0.4.0 soak and the cost of
  each new function.
- DESIGN.md records the 0.4.0 decisions (locality, §14 item 22; the
  library's groups 1–3 and contraction, §6.6, §6.7; open questions, §14
  items 23–24), the deferral of a native in-memory Louvain/Leiden kernel
  in favour of an in-memory `contract` (§6.7), the road to 1.0 (§13
  M9–M12, §14 item 25), and the design of 0.5.0's shared cache (§5.6, §14
  items 26–39).
- The upgrade check also covers grants on re-created functions (in the
  catalog and in dumps), initial privileges against a fresh install's, a
  planted operator that the update must not run, and contraction state
  written by each release; the dump check round-trips a quotient. Miri
  runs on pushes to main rather than on pull requests.

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
