# Limits

Settings, memory, what has been measured, and what does not work yet.

## Settings

All are ordinary GUCs, settable per session or per transaction
(`SET warren.threads = 8`) by any user.

| Setting | Default | Range | What it bounds | When exceeded |
|---|---|---|---|---|
| `warren.threads` | half the cores, at least 1 | 1–1024 | threads a compute kernel runs on (`spmv`, `spmm`, `vector_map`, reductions, `gather`, `eigen`, `distance`, `triangles`) | — |
| `warren.max_compute_memory` | `4GB` | `64kB` up | memory one call holds: every vector it reads (once, cached or not), every vector and work buffer it allocates | `configuration_limit_exceeded` (53400), before allocating |
| `warren.max_cache_memory` | `4GB` | `64kB` up | what this backend's cache holds *between* calls: cached versions, cached persisted vectors, and every role's transient vectors | versions and persisted vectors are evicted, least recently used first; a transient write that would go over is refused with 53400 |
| `warren.max_build_memory` | `4GB` | `1MB` up | the CSR offsets a `refresh` builds (8 bytes × (id_space + 1) per relation's CSR, and the merged CSR's if stored, per direction) | 53400, before anything is allocated or written |
| `warren.max_walk_paths` | `10000000` | 1 up | the largest `max_results` for a `walk` that returns a row per walk (`mode := 'paths'`, or `'ends'` without `distinct_ends`) | `program_limit_exceeded` (54000) |

On the `kg` graph of [change-detection.md](change-detection.md):

```sql
SET warren.max_compute_memory = '64kB';
SELECT warren.vector_from_query('kg', 'wide', 'SELECT id, array_fill(1.0::float8, ARRAY[2000]) FROM entities');
-- ERROR (53400): over the compute budget
RESET warren.max_compute_memory;
```

Other built-in bounds:

- A `walk` that returns a row per walk stops at 100,000 rows when you give no
  `max_results` (or at `warren.max_walk_paths`, if lower), with a `NOTICE`
  and `last_stats() -> 'truncated'` true. Truncation is never silent.
- A walk step's hop range is at most 1,024.
- Allocator failure is `out_of_memory` (53200), never a crash: every
  graph-sized buffer is reserved fallibly.
- `warren.max_build_memory` does not bound the edge list or the targets;
  those are bounded only by fallible allocation.

Every long call checks for interrupts (traversals every 65,536 units of work,
compute kernels from the main thread every millisecond), so
`statement_timeout` and `pg_cancel_backend` stop it with `query_canceled`.

## Memory model

- **The cache is per backend.** The first call in a connection that reads a
  version loads it from `warren.segments` into that backend's memory (the
  CSRs and the id map; vectors likewise, on first use). Every connection
  that queries a graph holds its own copy: 20 pooled connections hold 20
  copies.
- Loading a graph's current version drops that backend's other cached
  versions of the graph, so after promotions a backend holds one copy per
  graph. Calls with an explicit `version :=` load that version too.
- **The cache is bounded by `warren.max_cache_memory`** (default `4GB`,
  per backend). When it is over the cap, cached versions and persisted
  vectors are evicted, least recently used first. A version larger than the
  cap is loaded for the call that needs it and then dropped (with a
  `NOTICE`), without flushing everything else. Before a load, room is made
  for the version first, so a load does not take the cache past the cap.
  Evicted entries are simply reloaded on next use (about a second per GB), so
  a cap below your working set makes every call reload. A call or streaming
  function that is using an evicted version keeps its copy until it ends.
- **Transient vectors are never evicted**, since they cannot be reloaded.
  They count against the cap, and a transient write that would take the
  transient vectors alone past it is refused with 53400 before anything
  changes. A write no larger than the vector it replaces is always allowed,
  so after lowering the cap you can still rewrite or shrink what you hold.
  Roles sharing a backend share its cap. Transient vectors of a version
  dropped elsewhere are freed once the drop has committed.
- The library's scratch is transient (`pagerank` holds about eleven
  full-length `f64` vectors, ~88 bytes per node), so the version and the
  scratch must fit the cap together; otherwise the version reloads on every
  superstep, or, if the scratch alone does not fit, the call fails with
  53400. Raise `warren.max_cache_memory` for large graphs.
- `warren.evict(name)` frees a graph's cached versions and vectors, and this
  role's transient vectors of it (which are then gone), at once. Lowering the
  cap takes effect at the next lookup or load. A shared-memory cache across
  backends is a later release.
- `warren.prewarm(name)` loads the current version ahead of first use, for
  pooled connections; `warren.evict(name)` returns how many versions and vectors it freed.
- `warren.max_compute_memory` bounds what one call reads and allocates;
  `warren.max_cache_memory` bounds what is held between calls. A backend's
  peak is therefore about the sum of the two, plus, while a version loads,
  one CSR's raw segments (at most ~1.3 GB, the merged weighted CSR of 100M
  edges; measured: a 1.2 GB version loads at a 1.69 GB peak, a 6.0 GB one at
  7.3 GB). Two exceptions are bounded by neither limit: `refresh` (below)
  and `diff`, which holds both versions it compares (a 7.2 GB peak for two
  3.44 GB versions; on two 6.0 GB versions, in a soak run whose results are
  not committed, the backend was killed for memory at 12.8 GB), as
  `distance` between versions does.
- Storage of a version, per direction: offsets 8 bytes × (id_space + 1) for
  each relation, plus the merged CSR's when there is more than one relation
  and the graph stores it (`store_merged`, the default; see below);
  targets 4 bytes per edge (8 above 2³² nodes); weights 8 bytes per edge.
  The reverse CSR doubles it (`build_reverse := true`, the default). A
  persisted `f64` vector is 8 bytes × id_space × width (80 MB per column at
  10M nodes).

Cold loads, the cost of a per-backend cache, run at 0.26–0.55 GB/s from
`warren.segments` (measured at 10M nodes: 1.2 GB in 2.2–2.9 s, 3.44 GB in
6.2 s, 6.0 GB in 23 s).

```sql
SET warren.max_cache_memory = '64kB';
SELECT warren.vector_from_query('kg', 'wide', 'SELECT id, array_fill(1.0::float8, ARRAY[2000]) FROM entities',
                                persist := false);
-- ERROR (53400): a transient vector this large does not fit the cap
SELECT warren.evict('kg');                                         -- frees kg's cached versions and vectors
SELECT count(*) FROM warren.bfs('kg', ARRAY[1::bigint], 2);        -- reloads; still works under the cap
RESET warren.max_cache_memory;
```

### Sizing

Measured by the 100M-edge soak (`bench/soak.py`, DESIGN.md §12): 10M nodes,
100M edges, u32 targets, reverse CSR included, a `bigint` id map; decimal
GB, while `4GB` in a setting is 4 GiB ≈ 4.29 GB. "Cached" is what the cache
counts against `warren.max_cache_memory`; peaks are a backend's private
memory (shared buffers excluded).

| Graph (10M nodes, 100M edges) | Stored | Cached | `refresh` peak | Cold load | Fits the 4 GiB cap with `pagerank`'s scratch? |
|---|---|---|---|---|---|
| one relation, unweighted | 1.12 GB | 1.20 GB | 2.0 GB | 2.2–2.9 s | yes: 1.20 + 0.88 GB |
| five relations, weighted, merged CSR (default) | 5.92 GB | 6.00 GB | 9.0 GB | 23 s | no: the graph alone is over the cap |
| five relations, weighted, `store_merged := false` | 3.36 GB | 3.44 GB | 5.5 GB | 6.2 s | no: 3.44 + 0.88 = 4.32 GB, 30 MB over |

Not measured, and estimated the same way (the estimates matched the
measured cached bytes to 0.1%): one weighted relation ~2.8 GB; five
unweighted relations ~2.8 GB, or ~1.84 GB with `store_merged := false`.
`pagerank`'s transient scratch is ~0.88 GB at 10M nodes (88 bytes per
node).

**A version larger than `warren.max_cache_memory`, or one that does not fit
beside a library run's transient scratch, is reloaded on every call.** The
SQL library makes several calls per superstep (`pagerank` makes 8 per
iteration, each opening the version), so at the default cap `pagerank` on
the five-relation weighted graph reloads it about 170 times: more than 15
minutes (where it was stopped) with the merged CSR, at 23 s a load, and as
long without it. Single calls pay one load each (`bfs` 21.9 s instead of
0.73 s). The remedy is to raise `warren.max_cache_memory` to hold the graph
and the scratch (`SET warren.max_cache_memory = '8GB'`: `pagerank` then
takes 15.1 s with the merged CSR and 31.6 s without), or, for a graph that
is only just over, `store_merged := false`. Weighted multi-relation builds
of 100M edges also exceed 4 GB of build memory either way: give the
`refresh` backend 6–10 GB.

### The merged CSR (`store_merged`)

A graph with several relations stores, besides each relation's CSR, the
merged (all-relations) CSR that every read without `rels` uses: traversal
without `rels`, `spmv`/`spmm`/`vector_degree` without `rels`, `components`,
`gather`, `triangles`, `eigen`, `distance` and `diff`'s `all_relations`.
`create_graph(…, store_merged := false)` leaves it out: those reads then
merge the relations' rows as they go, with the same results (up to NaN
payloads), in about 44% less memory for five weighted relations and a
faster `refresh`, but more slowly, and more so with more relations: at 2–5
relations BFS 2–3× and SpMV 5–8× slower, at 10 relations 6× and 11×
(DESIGN.md §5.3). Reads with `rels`
are unaffected. Without the merged CSR, a product over all relations
(`spmv`, `spmm`, and so `eigen` and `distance`) also needs a row scratch on
each worker thread, charged to `warren.max_compute_memory`: 16 bytes per
entry of the longest row, at most 16 B × the relation count × the largest
degree in one relation. Keep the default unless the merged CSR would not fit your
memory limits. The choice is per graph, kept by `compact`, and shown by
`warren.info(name) -> 'store_merged'`; it cannot be changed after
`create_graph`.

## Measured performance

At the DESIGN.md §12 scale (10M nodes, 100M edges), from the M7 soak on a
shared 4-vCPU Xeon (half the 8 cores §12 assumes; another agent's builds
ran alongside, so times vary by up to 2× between runs), release build,
`warren.threads = 4`, default memory limits; full numbers and settings in
DESIGN.md §12 and `bench/results/soak/`:

| What (target) | One relation, unweighted | Five weighted relations, merged CSR | Five weighted relations, `store_merged := false` |
|---|---|---|---|
| BFS depth 6 (≥ 20× a recursive CTE) | **10.5–11×** (0.69–0.76 s vs 7.6 s) | 21.9 s, reloading; 0.73 s at an 8GB cap | 0.80–0.85 s |
| `reachable_set`, full component (> 50M edges/s) | **11–12M/s** | 3M/s reloading; 11.5M/s at 8GB | **7.5–7.7M/s** |
| `components()` (< 10 s) | 4.5–5.4 s | **30.9 s** reloading; 5.8 s at 8GB | borderline: 9.3–10.4 s |
| `pagerank`, 20 iterations (< 30 s on 8 threads) | 15.2–16.7 s | **≥ 15 min** reloading; 15.1 s at 8GB | **≥ 15 min** reloading; 31.6 s at 8GB |
| `eigen`, k = 20, normalised Laplacian (< 10 min) | **65 min, not converged** in the default 4,100 steps | 0.83 s a step (with its load) | 2.6 s a step |
| `refresh()` (< 8 min, < 4 GB) | 146–167 s, 2.0 GB | 347 s, **9.0 GB** | 221–234 s, **5.5 GB** |
| `diff` (< 30 s) | 13.5–22.3 s | not in the committed run; killed for memory (12.8 GB) in an uncommitted one | **47.7–55.2 s** |
| Cold load (< 15 s) | 2.2–2.9 s | **23 s** | 6.2 s |

Bold misses a target. Why, in short: a graph over the cache cap (or over
it with a library run's scratch) reloads on every call (see
[Sizing](#sizing)); weighted multi-relation builds and `diff` are bounded by
no limit; `eigen`'s step count grows with N on a random graph's clustered
spectrum, beyond the default `max_iter` (DESIGN.md §6.5), at ~1 s a step;
and BFS and `reachable_set` are timed through SQL, row emission included.

Smaller-scale kernel numbers (100K nodes, 1M edges unless stated):

| What | Measured | Source |
|---|---|---|
| SpMV (`plus_times`), pull over forward or reverse CSR | 0.38–0.42 G edges/s on 1 thread, 1.2 G edges/s on 4 | DESIGN §6.3, `benches/compute.rs` |
| SpMV, push (no reverse CSR) | 0.22 G edges/s, 1 thread | DESIGN §6.3 |
| `gather`, every node sending | `sum` of f64 14 ms (1 thread) / 8 ms (4); `collect` of f64 19 / 8 ms; `collect` of 8-byte messages 46 / 18 ms; pushed 30–70 ms | DESIGN §6.4, `benches/gather.rs` |
| `diff` between two versions | ~25 ms, 1 thread | DESIGN §7, `benches/version_diff.rs` |
| `eigen`, k = 20, f32, adjacency, largest | 1,037 steps: 9.9 s (1 thread) / 3.7 s (4); ARPACK 1,020–1,313 matvecs | DESIGN §6.5, `bench/results/eigen/` |
| `eigen`, k = 20, f32, normalised Laplacian, smallest | 1,342 steps: 13.1 s / 5.1 s; ARPACK 1,406–1,477 matvecs. At 1M nodes and 10M edges: not converged in the default 4,100 steps (226 s, 4 threads) where ARPACK took 4,343 matvecs (712 s); adjacency 3,174 steps (173 s) vs ARPACK 3,325 (499 s) | DESIGN §6.5, `bench/results/eigen/` |
| BFS, 1 / 2 / 3 / 5 of 5 relations (1M weighted edges, 1 thread) | 2.0 / 4.9 / 7.2 / 5.3 ms | DESIGN §5.3, `benches/relation_layout.rs` |
| Build, forward + reverse, 5 relations | 104 ms, 28.8 MB forward | DESIGN §5.3 |

## Known limitations

**PostgreSQL versions.** 15, 16, 17 and 18. CI runs the in-database tests
and the upgrade check on each major, and the SQL integration suite on 16.

**Upgrades.** After installing a new build, run `ALTER EXTENSION pg_warren
UPDATE;` in every database that has the extension before calling any
function. The new shared library is loaded at once, but the catalog changes
only with the update.

**`LIMIT` does not stop a set-returning function in `FROM`.** Postgres runs
a function in the `FROM` clause to completion before `LIMIT` applies. Only
`bfs` and `reachable_set` stream, and only in the select list (on the
`roads` graph of [compute.md](compute.md)):

```sql
SELECT warren.reachable_set('roads', ARRAY[1]) LIMIT 2;          -- stops after 2 rows
SELECT * FROM warren.reachable_set('roads', ARRAY[1]) LIMIT 2;   -- visits the whole component
```

In `FROM`, bound the work with `max_depth` or `max_results` instead. `walk`
always collects its rows (within its row limit) before returning the first.

**Integer `sum` overflows instead of widening.** `gather(…, 'sum')` of
integer messages returns `bigint` and raises `bigint out of range` on
overflow, where SQL's `sum(bigint)` returns `numeric`. `i64` arithmetic in
`spmv` and `vector_map` errors on overflow likewise (except the `min`/`max`
identities, which stand for ±∞). Use `f64` vectors or `double precision`
messages where totals may exceed 9.2 × 10¹⁸.

**Repeated eigenvalues.** Lanczos from one start vector finds one direction
per eigenspace, so a repeated eigenvalue may appear fewer times than its
multiplicity. A graph with c connected components has Laplacian eigenvalue 0
c times; `eigen` warns when the active nodes form several components. When
that matters, run `eigen` on a graph defined over the largest component
only. Non-symmetric spectra are not computed: `symmetrize := false` is
accepted only when the matrix is already symmetric.

**Stable functions in a `WHERE` clause are called per row.** Write
`WHERE x < (SELECT warren.vector_reduce(…))`, not `WHERE x <
warren.vector_reduce(…)`, so the call runs once.

**No shared cache.** Each backend loads its own copy of every version it
reads ([memory model](#memory-model)), within `warren.max_cache_memory`.

**Not yet supported:** `text` node ids; incremental maintenance (every
`refresh` is a full rebuild); contraction, `project`, `lift` and Louvain
(DESIGN.md §6.6); edge properties beyond one weight; ownership transfer;
cleanup of graphs and vectors when their owner role is dropped (a superuser
can still `drop_graph`); per-role quotas on readers' vectors.

**Cost shapes to know.**

- `triangles` costs about Σ over edges of min(d_u, d_v) · log max(d_u, d_v):
  near-linear on sparse graphs, and a hub costs about its degree × log, not
  its degree squared.
- `gather(…, 'collect')` of growing sets costs memory O(N × neighbourhood);
  it suits local neighbourhoods, not community-wide state.
- A persisted vector write costs WAL (8 bytes × id_space per `f64` column)
  on every call; keep iterative scratch transient (`persist := false`).
- `walk` in `paths` mode can be exponential in the number of hops; it is
  always bounded by `max_results`.
