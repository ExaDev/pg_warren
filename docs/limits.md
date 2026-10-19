# Limits

Settings, memory, what has been measured, and what does not work yet.

## Settings

All are ordinary GUCs, settable per session or per transaction
(`SET warren.threads = 8`) by any user.

| Setting | Default | Range | What it bounds | When exceeded |
|---|---|---|---|---|
| `warren.threads` | half the cores, at least 1 | 1–1024 | threads a compute kernel runs on (`spmv`, `spmm`, `vector_map`, reductions, `gather`, `eigen`, `triangles`) | — |
| `warren.max_compute_memory` | `4GB` | `64kB` up | memory one call holds: every vector it reads (once, cached or not), every vector and work buffer it allocates | `configuration_limit_exceeded` (53400), before allocating |
| `warren.max_build_memory` | `4GB` | `1MB` up | the CSR offsets a `refresh` builds (8 bytes × (id_space + 1) per relation's CSR, merged CSR included, per direction) | 53400, before anything is allocated or written |
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
- **The cache is not size-bounded yet.** Versions stay cached until the
  connection ends, `warren.evict(name)` frees them (and this role's transient
  vectors), or the version is dropped. A size-bounded cache
  (`warren.max_cache_memory`) is planned; a shared-memory cache across
  backends is a later release.
- `warren.prewarm(name)` loads the current version ahead of first use, for
  pooled connections; `warren.evict(name)` returns how many entries it freed.
- `warren.max_compute_memory` bounds one call. Cached versions, cached
  vectors and transient vectors held *between* calls are not charged to it.
- Storage of a version, per direction: offsets 8 bytes × (id_space + 1) for
  each relation, plus the merged CSR's when there is more than one relation;
  targets 4 bytes per edge (8 above 2³² nodes); weights 8 bytes per edge.
  The reverse CSR doubles it (`build_reverse := true`, the default). A
  persisted `f64` vector is 8 bytes × id_space × width (80 MB per column at
  10M nodes).

Cold loads, the cost of a per-backend cache, have not been benchmarked yet;
DESIGN.md §5.5 estimates a few hundred MB/s from TOAST.

## Measured performance

These are the numbers measured so far, from DESIGN.md and `bench/results/`.
All are on a shared 4-vCPU Xeon, at 100K nodes and 1M edges unless stated.
The DESIGN.md §12 targets (10M nodes, 100M edges, 8 cores) have **not yet
been measured** end to end; that is milestone M7's soak test.

| What | Measured | Source |
|---|---|---|
| SpMV (`plus_times`), pull over forward or reverse CSR | 0.38–0.42 G edges/s on 1 thread, 1.2 G edges/s on 4 | DESIGN §6.3, `benches/compute.rs` |
| SpMV, push (no reverse CSR) | 0.22 G edges/s, 1 thread | DESIGN §6.3 |
| `gather`, every node sending | `sum` of f64 14 ms (1 thread) / 8 ms (4); `collect` of f64 19 / 8 ms; `collect` of 8-byte messages 46 / 18 ms; pushed 30–70 ms | DESIGN §6.4, `benches/gather.rs` |
| `diff` between two versions | ~25 ms, 1 thread | DESIGN §7, `benches/version_diff.rs` |
| `eigen`, k = 20, f32, adjacency, largest | 1,037 steps: 9.9 s (1 thread) / 3.7 s (4); ARPACK 1,020–1,313 matvecs | DESIGN §6.5, `bench/results/eigen/` |
| `eigen`, k = 20, f32, normalised Laplacian, smallest | 1,342 steps: 13.1 s / 5.1 s; ARPACK 1,406–1,477 matvecs | DESIGN §6.5, `bench/results/eigen/` |
| BFS, 1 / 2 / 3 / 5 of 5 relations (1M weighted edges, 1 thread) | 2.0 / 4.9 / 7.2 / 5.3 ms | DESIGN §5.3, `benches/relation_layout.rs` |
| Build, forward + reverse, 5 relations | 104 ms, 28.8 MB forward | DESIGN §5.3 |
| Build peak memory, 100M weighted edges (scaled from 10M) | ~2.9 GB, one relation with reverse; ~5.8 GB weighted / ~2.6 GB unweighted with several relations | DESIGN §5.4 |

The random 100K-node graph is about the slowest case for `eigen` (a dense
bulk spectrum). Scaled linearly, a 10M-node, 100M-edge `eigen` with k = 20
projects to 8–9 minutes on 4 threads; this is a projection, not a
measurement. A weighted graph of 100M edges with several relations exceeds
the 4 GB build budget the design aims for, because the merged CSR is stored
in addition to the per-relation ones.

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
c times; when that matters, run `eigen` on a graph defined over the
largest component only. `symmetrize := false` (non-symmetric spectra) is accepted
only when the graph is already symmetric.

**Stable functions in a `WHERE` clause are called per row.** Write
`WHERE x < (SELECT warren.vector_reduce(…))`, not `WHERE x <
warren.vector_reduce(…)`, so the call runs once.

**No shared cache.** Each backend loads its own copy of every version it
reads ([memory model](#memory-model)). The cache has no size bound yet.

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
