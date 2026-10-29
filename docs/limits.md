# Limits

Settings, memory, what has been measured, and what does not work yet.

## Settings

These are ordinary GUCs, settable per session or per transaction (`SET
warren.threads = 8`) by any user, but for `warren.shared_cache_memory`,
which bounds memory the whole cluster shares: it is set in the server's
configuration and takes effect on reload. So are the
[administrator's limits](#limits-for-a-shared-instance), which bound what
any session may ask.

| Setting | Default | Range | What it bounds | When exceeded |
|---|---|---|---|---|
| `warren.threads` | half the cores, at least 1 | 1–1024 | threads a compute kernel runs on (`spmv`, `spmm`, `vector_map`, reductions, `gather`, `eigen`, `distance`, `triangles`, the sort in `core_number`, the centralities' sources and the similarities), and the mapping of a `refresh`'s edge endpoints | a call runs on fewer, within `warren.threads_limit` and the threads free under `warren.cluster_threads_limit` ([limits for a shared instance](#limits-for-a-shared-instance)) |
| `warren.max_compute_memory` | `4GB` | `64kB` up | memory one call holds: every vector it reads (once, cached or not), every vector and work buffer it allocates, and what its statement holds in the cache past `warren.max_cache_memory` | `configuration_limit_exceeded` (53400), before allocating or loading |
| `warren.max_cache_memory` | `4GB` | `64kB` up | what this backend's cache holds *between* statements: cached versions, cached persisted vectors, and every role's transient vectors | versions and persisted vectors no running statement has read are evicted, least recently used first; a transient write that would go over is refused with 53400 |
| `warren.shared_cache_memory` | `4GB` | `0` up; `0` turns it off | the cluster's shared versions and persisted vectors, of every database together; set in `postgresql.conf` (or `ALTER SYSTEM`) and reloaded, not per session ([the shared cache](#the-shared-cache)) | unpinned entries no backend maps are evicted, least recently used first; one that still does not fit is loaded into the reading backend's own cache, with a `NOTICE` |
| `warren.max_build_memory` | `4GB` | `1MB` up | everything a `refresh` holds at once: the node ids, the id map, the edge list, the CSRs it is building and its sort and write buffers; and, separately, the offsets every version holds whatever its edges (the merged CSR's 8 bytes × (id_space + 1) if stored, and 8 bytes per relation's CSR, per direction) | `configuration_limit_exceeded` (53400): before the edge query runs if the id map or the offsets do not fit, before the version is written if the build's estimate does not, else when the edge list grows past it |
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
- A `refresh` streams each relation's CSRs to `warren.segments` as it builds
  them, so it holds the edge list (20 bytes per weighted edge, 12 unweighted,
  8 with one relation), the id map (24 bytes per `bigint` node), the
  relation dictionary, 8 bytes per node of row cursors while it builds the
  relations, and a group of relations' forward CSRs with one reverse CSR
  (each relation's offsets at most what its edges need): as many relations
  at once as fit half of what remains of the budget, so a small graph
  builds in one pass and a large one in up to about eight. With the merged CSR stored, its build peaks at about 28 bytes
  per weighted edge. A relation column with a distinct name per row is
  refused while it is read (the dictionary is charged as it grows).
- A `refresh` reports the most build memory it held at `DEBUG1`.

Every long call checks for interrupts (traversals every 65,536 units of work,
compute kernels from the main thread every millisecond), so
`statement_timeout` and `pg_cancel_backend` stop it with `query_canceled`.

### Limits for a shared instance

On a server that several roles share, an administrator bounds what each
session may ask, and the compute threads of all sessions together, with
settings that only the server's configuration sets: `postgresql.conf`,
`ALTER SYSTEM`, or a managed service's parameter group (on Amazon RDS,
the DB parameter group; the customer's administrator is not a superuser
there and cannot `ALTER SYSTEM`). They take effect on reload, and no
session can `SET` them (DESIGN.md §14 item 52).

| Setting | Default | Range | What it bounds |
|---|---|---|---|
| `warren.threads_limit` | `-1` (none) | `-1`, 1–1024 | `warren.threads` |
| `warren.compute_memory_limit` | `-1` (none) | `-1` up (kB) | `warren.max_compute_memory` |
| `warren.cache_memory_limit` | `-1` (none) | `-1` up (kB) | `warren.max_cache_memory` |
| `warren.build_memory_limit` | `-1` (none) | `-1` up (kB) | `warren.max_build_memory` |
| `warren.walk_paths_limit` | `-1` (none) | `-1` up | `warren.max_walk_paths` |
| `warren.cluster_threads_limit` | `-1` (none) | `-1`, 1–65536 | compute threads of every backend's calls at once |

`0` sets no limit, as `-1` does.

- **A session's setting above its limit is not refused:** a call uses the
  lower of the two, as Postgres runs `max_parallel_workers_per_gather`
  within `max_parallel_workers`. So a script that sets
  `warren.max_compute_memory = '8GB'` runs on any server, and a call that
  needs more than the limit fails as it would over the session's own
  setting (53400, or 54000 for a walk's `max_results`). Where the limit is
  the lower, a message that names a setting names the limit.
- **Fewer threads never change a result** (DESIGN.md §6.5, §6.7); they only
  take longer.
- **`warren.cluster_threads_limit`** counts the threads each running call
  holds, in pg_warren's shared memory. A call asks for the lower of
  `warren.threads` and `warren.threads_limit` and runs on as many as are
  free, up to that, waiting only while none is. A waiting backend shows
  `wait_event` `WarrenComputeThreads` in `pg_stat_activity` (`Extension`
  on PostgreSQL 15 and 16), and a cancel, `statement_timeout` or
  `pg_terminate_backend` ends the wait. A call holds its threads from its
  first kernel to its last, so between kernels (reading messages for
  `gather`, writing a result) they are held, not used. A backend that
  exits gives its threads back. It sets no cap by default. Set to the
  server's cores, it keeps compute from running more threads than there
  are cores, and two calls at the default `warren.threads` (half the
  cores) still run at full width together. Calls that wait for threads
  count the wait against `statement_timeout`, so allow for it there.
- **It needs pg_warren's shared memory:** nothing on PostgreSQL 17 and 18;
  on 15 and 16, `shared_preload_libraries = 'pg_warren'`. Without it the
  cap is not enforced (each call still keeps within `warren.threads_limit`),
  and each backend says so once in the server log.
- **Waiting takes no turn:** a call that frees threads does not hand them
  to the backend that waited longest, so under a constant stream of calls
  one may wait long. `statement_timeout` bounds it.
- **To see it work** without a shell (on a managed service),
  `warren.backends()` has a `compute_threads` column: on each backend's
  row the threads its running call holds (0 between calls; under the cap,
  what it was granted, which may be fewer than its `warren.threads`), and
  on the `cluster` row the threads all calls hold now, which the cap
  bounds. The count is kept whether or not a cap is set, so it also shows
  what a cap would have to allow. Another role's row shows it only to
  roles with that role's privileges or `pg_read_all_stats`, as the
  other sizes; the `cluster` row is everyone's. On PostgreSQL 15 and 16
  without the preload there is no `cluster` row (and no cap).

```sql
SHOW warren.cluster_threads_limit;
SELECT kind, pid, compute_threads FROM warren.backends();  -- held per backend, then the cluster's count
SELECT pid, wait_event FROM pg_stat_activity
WHERE wait_event IN ('WarrenComputeThreads', 'Extension');  -- calls waiting for threads
```

On a small instance, also size `warren.shared_cache_memory` (default `4GB`
for the whole cluster) to the instance: Postgres does not know the
machine's memory, so the default does not follow it. About an eighth of
the instance's memory is a starting point, beside `shared_buffers` (a
quarter by convention) and each backend's own `warren.max_cache_memory`
and `warren.max_compute_memory`; on Linux it must also fit `/dev/shm`
([the shared cache](#the-shared-cache)).

| Instance memory | `warren.shared_cache_memory` | `warren.max_cache_memory` and `warren.max_compute_memory` (limits) |
|---|---|---|
| 4 GB | `512MB` | `256MB` |
| 16 GB | `2GB` | `1GB` |
| 64 GB | `8GB` | `4GB` (the defaults) |
| 256 GB | `32GB` | `16GB` |

These are starting points, not measurements: what a graph needs is in
[Sizing](#sizing).

## Memory model

A loaded version (its CSRs and id map) lives in one of two places. By
default it is in the **shared cache**: one copy per cluster, in dynamic
shared memory, mapped by every backend that reads it (below). Where the
shared cache cannot hold it, a backend loads it into its own **private
cache**, as every version was before 0.5.0. Query functions behave the
same either way; only where the bytes live, and which setting counts them,
differs.

- **The private cache is per backend.** A version that is not shared is
  loaded by the first call in a connection that reads it, from
  `warren.segments` into that backend's memory (vectors likewise, on first
  use), and every connection that reads it holds its own copy: 20 pooled
  connections hold 20 copies. The rest of this list is about the private
  cache; shared versions are not counted in it.
- Loading a graph's current version drops that backend's other cached
  versions of the graph, so after promotions a backend holds one copy per
  graph. Calls with an explicit `version :=` load that version too.
- **The cache is bounded by `warren.max_cache_memory`** (default `4GB`,
  per backend). When it is over the cap, cached versions and persisted
  vectors are evicted, least recently used first. Before a load, room is
  made for the version first. Evicted entries are simply reloaded on next
  use (about a second per GB), so a cap below what one statement reads
  makes every statement reload. A call or streaming function that is
  using an evicted version keeps its copy until it ends.
- **A statement keeps what it reads.** Every version and persisted vector
  a top-level statement reads or writes stays cached until the statement
  ends, or its transaction does, whichever comes first, even past the cap,
  so a statement that makes many calls loads each version once. A library
  function such as `pagerank` is one statement, however many calls it
  makes inside. What counts as a statement:
  - one client message: with the extended protocol each `Execute` (a
    cursor or portal fetched over several is held only during each);
    several statements sent as one query string, as `psql -c 'a; b'`
    does, count as one;
  - a `CALL` or `DO` block is one statement, but a `COMMIT` inside it ends
    what it held, so a procedure that loops with `COMMIT` holds only what
    its current transaction read.

  When the statement ends, normally or with an error, what it held is
  released and the cap holds again, also between the statements of a
  transaction block. A version larger than the cap is held for the
  statement that reads it, with a `NOTICE` once per statement, and
  reloaded by the next one.
- **What a statement holds gives way under pressure.** A version or vector
  the statement read that no call is using (`pagerank`'s version between
  its calls, a graph a loop has moved past) stays while there is room, its
  bytes past the cap charged to each call's `warren.max_compute_memory`.
  It is evicted, least recently used first, only when a load needs the
  room or a call's allocation would otherwise exceed
  `warren.max_compute_memory`. So a statement that reads more graphs, one
  after another, than the two settings hold together still works; it
  reloads what it reads again. A statement that alternates between graphs
  that do not fit the cap together keeps them loaded while the two
  settings hold them, and reloads them only when a call needs the room.
  Allocations that kernels make on worker threads (`warren.threads`)
  cannot evict anything, since only the backend's main thread may touch
  the cache. Kernels make room first for the buffers they allocate whole
  there (`triangles`' adjacency, `gather`'s sorted messages and inbox),
  but two kinds grow as they go and cannot: `gather`'s result, and the
  row scratch of a product over several relations (`spmv`,
  `spmm`, and `eigen` and `distance` through them). Those can
  fail with 53400 where evicting the statement's idle versions would
  have made room. Raise `warren.max_compute_memory` or
  `warren.max_cache_memory` if that happens.
- **What the cache holds past the cap comes out of
  `warren.max_compute_memory`.** That is transient vectors, the versions
  and vectors calls are using, and versions evicted while a call still
  holds them (an open cursor or a suspended portal streaming `bfs` or
  `reachable_set`, which keep theirs until they close). Each call has that
  much less compute memory (reading a vector the cache already holds
  costs nothing more), and a version load that would take it past
  `warren.max_compute_memory` is refused with 53400 before anything is
  loaded; the message names both settings and the bytes needed. That
  happens only for what is in use: a version larger than the two
  together, the two versions of a `diff` or `distance` that do not fit
  them, or loads beside versions that open cursors hold.
- **Transient vectors are never evicted**, since they cannot be reloaded.
  They count against the cap, and a transient write that would take the
  transient vectors alone past it is refused with 53400 before anything
  changes. A write no larger than the vector it replaces is always allowed,
  so after lowering the cap you can still rewrite or shrink what you hold.
  Roles sharing a backend share its cap. Transient vectors of a version
  dropped elsewhere are freed once the drop has committed.
- The library's scratch is transient (`pagerank` holds about eleven
  full-length `f64` vectors, ~88 bytes per node). The version stays
  cached beside it for the whole run; if the two exceed the cap, the
  excess comes out of `warren.max_compute_memory`, and if the scratch
  alone does not fit the cap, the call fails with 53400. Raise
  `warren.max_cache_memory` for large graphs, so the version also stays
  cached between runs.
- `warren.evict(name)` frees a graph's cached versions and vectors, and this
  role's transient vectors of it (which are then gone), at once, and, if
  the caller is a member of the graph's owner role, evicts its shared
  versions and vectors that no statement is using. Lowering the cap takes
  effect at the next lookup or load.
- `warren.prewarm(name)` loads the current version ahead of first use, for
  pooled connections; `warren.evict(name)` returns how many versions and vectors it freed.
- `warren.max_compute_memory` bounds what one call reads and allocates,
  and what the cache holds past its cap; `warren.max_cache_memory`
  bounds what is held between statements. A backend's peak is therefore
  bounded by the sum of the two (a traversal's visited set and frontier
  aside, and calls running at once in one statement, each of which gets
  its own budget against the same cache), plus, while a version loads,
  one CSR's raw segments (at most ~1.3 GB, the merged weighted CSR of 100M
  edges; measured: a 1.2 GB version loads at a 1.69 GB peak, a 6.0 GB one at
  7.3 GB). `refresh` (below) is bounded by neither. A `refresh` or a
  `contract` adds its build scratch, charged to `warren.max_build_memory`,
  so a statement that runs one, as `louvain` runs `contract`
  once a level (and `leiden` did before 0.8.0), is bounded by the three settings together: on the 0.4.0
  soak's five-relation graph `louvain` peaked at 9.50 GB, past the 8.59 GB
  of cache and compute (at 1M nodes, its peak with all three limits at
  the least it needs was within 1.5% of their sum, that 1.5% Postgres's
  own memory). `diff`, `distance` and
  `vector_norm_diff` read their two versions through the cache under the
  same rule, and refuse two that do not fit the two settings together
  before loading either (before, `diff` held both outside every limit: a
  7.2 GB peak for two 3.44 GB versions, and on two 6.0 GB versions, in a
  soak run whose results are not committed, the backend was killed for
  memory at 12.8 GB).
- Storage of a version, per direction: offsets 8 bytes × (id_space + 1) for
  the merged CSR when there is more than one relation and the graph stores
  it (`store_merged`, the default; see below), and for each relation whose
  edges start from many of the nodes; one whose edges start from at most
  about a sixth of them (an eighth above 2³² nodes) stores 12 bytes (16)
  per node they start from instead (in the reverse CSR, per node they
  reach);
  targets 4 bytes per edge (8 above 2³² nodes); weights 8 bytes per edge.
  The reverse CSR doubles it (`build_reverse := true`, the default). A
  persisted `f64` vector is 8 bytes × id_space × width (80 MB per column at
  10M nodes).

Cold loads run at 0.26–0.55 GB/s from `warren.segments` (measured at 10M
nodes: 1.2 GB in 2.2–2.9 s, 3.44 GB in 6.2 s, 6.0 GB in 23 s), once per
cluster for a shared version, once per backend for a private one.

```sql
SET warren.max_cache_memory = '64kB';
SELECT warren.vector_from_query('kg', 'wide', 'SELECT id, array_fill(1.0::float8, ARRAY[2000]) FROM entities',
                                persist := false);
-- ERROR (53400): a transient vector this large does not fit the cap
SELECT warren.evict('kg');                                         -- frees kg's cached versions and vectors
SELECT count(*) FROM warren.bfs('kg', ARRAY[1::bigint], 2);        -- reloads; still works under the cap
RESET warren.max_cache_memory;
```

### The shared cache

Since 0.5.0 a version is loaded once per cluster and mapped by every
backend that reads it, so a pool of connections reading one graph holds
one copy, and the first connection's load serves the rest. Persisted
vectors (`pagerank`'s results, say) are shared the same way. Shared entries
are counted by neither `warren.max_cache_memory` nor
`warren.max_compute_memory`: evicting one would free nothing of the
backend's own.

- **`warren.shared_cache_memory`** (default `4GB`) bounds the shared
  entries of all databases together. It is a server setting: change it in
  `postgresql.conf` or with `ALTER SYSTEM`, then reload. `0` turns the
  shared cache off; the shared entries are then evicted as backends next
  read a graph. Memory is taken only as versions are read, and a shared
  entry stays until it is evicted (to make room, by `warren.evict` or when
  its version is dropped or superseded) or the server restarts, also while
  no backend uses it: an idle cluster can hold up to the cap.
- **When the cap is full**, entries no statement is using and no backend
  maps are evicted, least recently used first. A version that still does
  not fit (it is larger than the cap, or the versions in use leave no
  room) is loaded into the reading backend's private cache instead, with a
  `NOTICE` once per statement naming the setting, and the least recently
  used versions that idle backends still map are marked for eviction, so
  that the room appears as those backends next read a graph. Nothing waits
  for room and nothing fails.
- **PostgreSQL 17 and 18** need nothing configured: the cache's registry is
  created on first use, also under a trusted install.
- **PostgreSQL 15 and 16** need pg_warren preloaded, which takes a restart:

  ```
  shared_preload_libraries = 'pg_warren'
  ```

  Without it every backend loads its own copies, as in 0.4.0. At the
  default cap that is silent; with `warren.shared_cache_memory` set to
  anything else, each backend says so once in a `NOTICE`.
- **`/dev/shm`.** On Linux each shared version is a file in `/dev/shm`
  (`dynamic_shared_memory_type = posix`, the default), and its pages are
  memory. A version is shared only if it fits there with a quarter of
  `/dev/shm` left free for Postgres's own segments (parallel query uses
  them too); otherwise it is loaded privately. That is silent at the
  default cap, so that a container nobody configured does not say so in
  every statement; with the cap set by hand, a `NOTICE` per statement
  names the container's `--shm-size`. Either way `warren.shared_cache()`
  (below) lists what is shared, and a version missing from it was loaded
  privately. A host's `/dev/shm` is usually half its RAM.
  **Docker gives a container 64 MB**, so only versions up to 48 MB in all
  are shared there: run the container with `--shm-size` at least the cap
  plus a third (`docker run --shm-size=6g …` for the default 4GB; in
  Compose, `shm_size`; in Kubernetes, an `emptyDir` with `medium: Memory`
  mounted at `/dev/shm`). With `dynamic_shared_memory_type = mmap` the
  files are in the data directory's `pg_dynshmem` (backed by disk) and
  the same check applies there; `sysv` segments are not limited this way.
- **What can still fail.** On PostgreSQL 17 and 18 the cache's registry is
  created by the cluster's first read of a graph (after each start, and
  after an upgrade that changes its format). If `/dev/shm` then lacks a
  few megabytes (the reserve counts: a cache filled to its reserve does
  not stop it), the cache is unavailable and reads load privately,
  silently at the default cap; it is tried again at each read.
  Two errors are left to Postgres, which pg_warren cannot turn into a
  fallback: another process filling `/dev/shm` between pg_warren's check
  and a segment's creation (`could not resize shared memory segment`,
  53100), and, while the registry is being created, every one of the
  cluster's dynamic shared memory slots (64 plus 5 per allowed connection)
  already in use by other segments (`too many dynamic shared memory
  segments`, 53000). Either fails that one statement; the next one, once
  the room or a slot is free, works.
- **`min_dynamic_shared_memory`** (a restart setting) reserves part of
  Postgres's main shared memory for dynamic segments, backed by huge pages
  when `huge_pages` is on, and segments are taken from it first. Shared
  versions use it like any other segment, but pg_warren still admits a
  version only if it would fit `/dev/shm` as above (it cannot see how much
  of the reserved area is free, and a segment that does not fit there goes
  to `/dev/shm`), so it does not lift a small `/dev/shm`'s limit. Size it to
  the versions you want shared, plus what parallel query needs.
- **What stays private:** transient vectors (per role and backend by
  design), `bool` vectors, a version or vector written by the reading
  transaction (a staged build queried before commit), and everything on a
  big-endian machine.
- **Memory accounting.** Linux counts a shared version's pages in every
  backend that maps it (`RssShmem`, and so `VmRSS` and `top`'s `RES`), so
  adding up backends' RSS counts a shared version once per backend. Use
  their PSS (`/proc/<pid>/smaps_rollup`), the size of `/dev/shm`, or
  `warren.shared_cache()`:

```sql
SELECT kind, graph, version, state, pg_size_pretty(bytes) AS size, pins, mappers
FROM warren.shared_cache();                -- a row per shared entry you may read, then the cluster's totals
```

  Its rows list the shared versions and vectors of graphs the caller may
  read, in this database; the last row (`kind` `cluster`) gives the
  bytes, pins and mappers of every database's, and the cap. `pins` counts
  statements using an entry now, `mappers` backends that map it.

  Two more views (0.6.0) show it per backend. `warren.cache()` lists what
  this backend holds privately, against `warren.max_cache_memory`.
  `warren.backends()` gives each backend's private and mapped bytes and
  its cache counters, which count once per statement and version, then the
  cluster's totals (and, from 0.9.0, the compute threads held, under
  [Limits for a shared instance](#limits-for-a-shared-instance)):

```sql
SELECT pid, pg_size_pretty(private_bytes) AS private, pg_size_pretty(mapped_bytes) AS mapped,
       lookups, shared_hits, private_loads + shared_loads AS loads, refusals
FROM warren.backends();                    -- a row per backend, then the cluster's totals
```

  A `refusals` entry other than 0 says why versions were loaded privately
  (`no_room` for a full `/dev/shm`, `unavailable` for no shared cache).
  On PostgreSQL 15 and 16 other backends' rows need the preload.

Measured (DESIGN.md §12, `bench/pooler.py`; 1M nodes and 10M edges, 4
vCPUs, PgBouncer in transaction mode): 50 pooled backends reading one
graph hold one copy, 338 MB in all for a 112 MB version against 6.63 GB
with the shared cache off, and 562 MB for a 336 MB version that would need
16.8 GB of private copies. A pooled backend's own memory is 4–12 MB. The
pool's cold start takes 0.7 s for 50 sessions at once instead of 1.2 s,
and `neighbours`, `bfs` and `pagerank` run as fast as with private copies
or 5–15% faster. Connections that each read a graph of their own gain
nothing from sharing, and lose a little: a shared load of a 34 MB version
takes 20–70 ms longer than a private one.

**Parallel query.** The graph reads that use no vector are `PARALLEL
SAFE` (docs/semantics.md, "Parallel query labels"), so a scan that calls
one per row can run in parallel workers, each reading the version as
another backend would. Measured (DESIGN.md §12, `bench/parallel.py`; PG 16,
4 vCPUs, medians): `has_edge` over 1M rows of a 1M-node graph takes
9.8–10.3 s in the leader alone, 3.7 s with 2 workers, and 2.9 s with 4
with the shared cache (3.1 s when each worker loads its own copy): 2.6–2.8×
faster with 2 workers and 3.2–3.6× with 4 across the reads measured with
the shared cache. On a 10M-node graph (1.12 GB), 11.4–11.9 s alone, and
with 2 and 4 workers 4.4 and 3.6 s shared, 7.5 and 6.0 s when each worker
loads its own copy. A worker's first read costs it an attach (about 20 ms
for 112 MB) with the shared cache, or a whole load without it, so a
parallel plan over a small table that reads the version privately is
slower than the leader alone (1.8–3× over 10k rows without the preload,
when parallelism is forced). The planner chose parallel plans only for the
large table.

A worker reads a version privately wherever the leader would: not only on
PostgreSQL 15 and 16 without the preload or with
`warren.shared_cache_memory = 0`, but also for a version larger than the
cap, one the shared cache has no room for (beside what is pinned, or in
`/dev/shm`: Docker's default 64 MB holds little), and one the current
transaction created. Each worker's copy counts against its own
`warren.max_cache_memory` and `warren.max_compute_memory` together and is
held until the query ends, so a parallel plan can hold one copy per
worker plus the leader's (five with 4 workers). To bound that, or for
small tables: preload pg_warren on PostgreSQL 15 and 16 and size the
shared cache and `/dev/shm` (above); keep queries or roles off parallel
plans with `SET max_parallel_workers_per_gather = 0` (any role, per
session, `SET LOCAL` per transaction, or `ALTER ROLE … SET`); or, as a
superuser, mark the function `PARALLEL RESTRICTED` (docs/semantics.md,
"Parallel query labels").

### Sizing

Measured by the 100M-edge soak (`bench/soak.py`, DESIGN.md §12; the
full-scale re-run after M7, at 10f68ab): 10M nodes, 100M edges, u32
targets, reverse CSR included, a `bigint` id map; decimal GB, while `4GB`
in a setting is 4 GiB ≈ 4.29 GB. "Cached" is what the cache counts against
`warren.max_cache_memory`; peaks are a backend's private memory (shared
buffers excluded).

| Graph (10M nodes, 100M edges) | Stored | Cached | `refresh` peak | Cold load | Fits the 4 GiB cap with `pagerank`'s scratch? |
|---|---|---|---|---|---|
| one relation, unweighted | 1.12 GB | 1.20 GB | 1.55 GB (2.0 GB before 0.3.0) | 1.9–2.2 s | yes: 1.20 + 0.88 GB |
| five relations, weighted, merged CSR (default) | 5.92 GB | 6.00 GB | 3.26 GB (9.0 GB before) | 23 s | no: the graph alone is over the cap |
| five relations, weighted, `store_merged := false` | 3.36 GB | 3.44 GB | 3.26 GB (5.5 GB before) | 7–13 s | no: 3.44 + 0.88 = 4.32 GB, 30 MB over (charged to compute memory) |

The `refresh` peaks are measured through SQL, and match what the build
charges (1.56, 3.24 and 3.24 GB) plus the little a backend holds past the
charge and the edge query's own execution memory, which `work_mem` bounds
(DESIGN.md §5.4). All are within 4 GiB.
`warren.max_build_memory` is the build's alone: what the backend's cache
holds (`warren.max_cache_memory`) comes on top of it during a `refresh`.

Not measured, and estimated the same way (the estimates matched the
measured cached bytes to 0.1%): one weighted relation ~2.8 GB; five
unweighted relations ~2.8 GB, or ~1.84 GB with `store_merged := false`.
`pagerank`'s transient scratch is ~0.88 GB at 10M nodes (88 bytes per
node).

**A version that does not fit `warren.max_cache_memory`, alone or beside
a library run's transient scratch, is held for the statement that reads
it and reloaded by the next.** A library run is one statement, so it loads
its version once, and what it holds past the cap is charged to
`warren.max_compute_memory`. A statement that would hold more than the two
settings together fails with 53400 before loading. Before this, the soak
measured the cliff it replaces: the version was reloaded on every call,
and since `pagerank` makes 8 calls per iteration, a default-cap run on the
five-relation weighted graph reloaded it about 170 times, more than 15
minutes (where it was stopped). Measured again at full scale
(`bench/results/soak/`): `pagerank` now loads the version once at the
default limits, 40.1 s with the merged CSR (23 s of it the load) and
35.6 s without, against 17.4 s and 32.7 s at an 8GB cap. Raising
`warren.max_cache_memory` to hold the graph and the scratch (`SET
warren.max_cache_memory = '8GB'`) still saves the reload between
statements, and single calls on a graph over the cap still pay one load
each (`bfs` 23.4 s instead of 0.71 s). A statement that needs more than
the two settings together is refused: on the merged graph, `diff` and
`distance` between two versions (12.0 GB) and `eigen` (~3 GB beside the
6.0 GB version) fail with 53400 at the default limits. `eigen`, and
`distance('spectral')` without `vectors_prefix`, are refused before the
first step, not after it: with n the id space, m = min(active nodes,
max(2k + 1, 20)) basis vectors and e = 4 bytes (`f32`) or 8 (`f64`), a
solve holds at its peak n·33 bytes of operator (n·49 for the normalised
operators), ⌈n / 4096⌉·4096·m·e of basis, (4m² + m)·8 of projected
matrices, k·n·e of eigenvectors and n·11 for counting components (where the
components' eigenpairs are taken exactly, n·17 + 17 per component for them
instead, held through the solve, and n·9 of a product's scratch beside one
eigenvector fewer); at 10M nodes, k = 20, `f32` and the normalised
Laplacian that is 3.0 GB, 3.1 GB with `which := 'smallest'`. For a graph that is only just over, `store_merged :=
false` also helps. With the shared cache (above), a version
it holds counts against `warren.shared_cache_memory`, not
`warren.max_cache_memory`, so the five-relation graph without the merged
CSR (3.44 GB) is shared at the default `4GB` and `pagerank`'s scratch has
the backend's cap to itself. The merged graph (6.0 GB) is over the default
shared cap as well, and is loaded privately as above: raise
`warren.shared_cache_memory` (to `8GB`, say, with `/dev/shm` to match) to
share it. A `refresh` of any of these graphs fits the default
`warren.max_build_memory` (before 0.3.0, weighted multi-relation builds
of 100M edges needed 6–10 GB).

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
degree in one relation, and 8 bytes a relation for where it last found
each relation's row. Keep the default unless the merged CSR would not fit
your memory limits. The choice is per graph, kept by `compact`, and shown
by `warren.info(name) -> 'store_merged'`; it cannot be changed after
`create_graph`.

Since 0.7.0 a relation whose rows fill little of the id space stores
sparse offsets, so on a graph of many sparse relations (a knowledge
graph) leaving out the merged CSR saves less than it did, and costs
more: the merged CSR is now cheap beside the relations (FB15k-237: 2.5
of its 8.0 MB). Without it, BFS and `components` find each relation's
rows in order and stay within 1.3× of what they cost with dense offsets,
but an all-relations product (`spmv`, `spmm`, and so `pagerank`,
`eigen` and `distance`) seeks every node's row in every
relation: on FB15k-237 (237 relations, 14.5K nodes) `spmv` takes about
18 ms where 0.6.0's dense offsets took 11, 1.6–2×. The default, with the
merged CSR stored, is unaffected.

### Node order (locality)

At millions of nodes SpMV and traversal are bound by memory: each edge reads
its neighbour's value at a random place. `create_graph(…, node_order :=
'locality')` numbers the first build's nodes so that neighbours get nearby
dense ids (Reverse Cuthill–McKee), and `compact(name, node_order :=
'locality')` does the same for an existing graph (the order given becomes
the graph's). Results are the same in either order, read by external id.
Measured at 10M nodes and 100M edges on 4 vCPUs, against node ids that
carry no structure (DESIGN.md §14 item 22):

| | community-structured graph | power-law graph |
|---|---|---|
| SpMV, 1 / 4 threads | 1.51× / 1.63× faster | 1.13× / 1.04× |
| BFS | 1.25× | 1.23× |
| `components` | 1.66× | 1.25× |
| cost: ordering at the first build or `compact` | 10 s | 12 s |

The gain is worth having on graphs with communities whose ids are arbitrary
(uuids, ids from a busy shared sequence), and small on power-law graphs; if
your ids already follow the structure, keep `'id'`. Ordering needs scratch
of about 24 bytes per node and 8 bytes per edge (1.04 GB at 10M/100M),
charged to `warren.max_build_memory`, so a first build in locality order
peaks higher: 2.48 GB rather than 1.56 GB for one unweighted relation at
that size. Later refreshes keep every surviving node's dense id and append
new nodes, so locality decays as the graph changes; `compact` restores it.

## Measured performance

At the DESIGN.md §12 scale (10M nodes, 100M edges), from the M7 soak's
full-scale re-run (10f68ab) on an idle 4-vCPU Xeon (half the 8 cores §12
assumes), release build, `warren.threads = 4`, default memory limits; full
numbers, settings and the first soak's figures in DESIGN.md §12 and
`bench/results/soak/`. The 0.4.0 re-run (5b0bf81) measured the same peaks
and times within this machine's run-to-run spread (10–20% for one build;
an A/B against 10f68ab found no stage reproducibly slower), except that
the five-relation `components()` without the merged CSR took 11.8 s:

| What (target) | One relation, unweighted | Five weighted relations, merged CSR | Five weighted relations, `store_merged := false` |
|---|---|---|---|
| BFS depth 6 (≥ 20× a recursive CTE) | **11.0×** (0.67 s vs 7.32 s) | **23.4 s**, the load; 0.71 s at an 8GB cap | 0.71 s |
| `reachable_set`, full component (> 50M edges/s) | **26M/s** (3.86 s) | **3.8M/s**, the load; 25M/s at 8GB | **16M/s** |
| `components()` (< 10 s) | 6.2 s | **29.2 s**, the load; 5.8 s at 8GB | borderline: **10.1 s** (11.8 s in the 0.4.0 run) |
| `pagerank`, 20 iterations (< 30 s on 8 threads) | 15.6 s | **40.1 s**, one load; 17.4 s at 8GB | **35.6 s**, one load from cold; 32.7 s at 8GB |
| `eigen`, k = 20, normalised Laplacian (< 10 min) | **misses**: 0.59 s a step, ~14,500 steps on this random graph (~2 h) | **refused (53400)** at the default limits; 0.77 s a step at 8GB | not measured |
| `refresh()` (< 8 min, < 4 GB) | 153 s, 1.55 GB | 260 s, 3.26 GB | 228 s, 3.26 GB |
| `diff` (< 30 s) | 17.8 s | **refused (53400)**: two 6.0 GB versions exceed the two limits | **58.4 s** |
| Cold load (< 15 s) | 1.9–2.2 s | **23 s** | 7–13 s |

Bold misses a target. Why, in short: a graph over the cache cap loads once
per statement, so single calls on it pay its 23 s load, while a library
run such as `pagerank` loads it once (see [Sizing](#sizing)); calls that
need more than `warren.max_cache_memory` and `warren.max_compute_memory`
together (two 6.0 GB versions, or `eigen` beside one) are refused with
53400; `eigen`'s step count grows with N on a random graph's clustered
spectrum, beyond the default `max_iter` (DESIGN.md §6.5), while on a
graph with community structure it is projected at ~400 steps, 3–4 min at
this scale; and BFS and `reachable_set` are timed through SQL, row
emission included. BFS to depth 6 is about 85% row emission (the
traversal itself takes 0.11 s), so a faster kernel cannot close its gap.
`reachable_set` pulls levels from the reverse CSR (direction-optimising
BFS): its traversal takes 1.24 s instead of 5.96 s at this scale, and
through SQL 3.86 s instead of 8.3–9.2 s (DESIGN.md §9.3).

Smaller-scale kernel numbers (100K nodes, 1M edges unless stated):

| What | Measured | Source |
|---|---|---|
| SpMV (`plus_times`), pull over forward or reverse CSR | 0.38–0.42 G edges/s on 1 thread, 1.2 G edges/s on 4 | DESIGN §6.3, `benches/compute.rs` |
| SpMV, push (no reverse CSR) | 0.22 G edges/s, 1 thread | DESIGN §6.3 |
| SpMV out of cache (4M nodes, 40M edges), pull over reverse CSR | 0.12–0.13 G edges/s (f64) and 0.14–0.17 (f32) on 1 thread; 0.19–0.33 and 0.22–0.51 on 4 (a loaded machine) | DESIGN §6.10, `benches/spmv_out_of_cache.rs` |
| `vector_map`, `pagerank`'s four maps (4M nodes), 1 thread | 145–165 ms (340–390 ms before the no-NULL path: 2.1–2.7× faster); one whole iteration with its product 580–665 ms (790–840 ms before: 1.2–1.4×); 250–375 ms (480–550 ms: 1.3–2.2×) on 4 threads | DESIGN §6.3, `benches/spmv_out_of_cache.rs` |
| `gather`, every node sending | `sum` of f64 14 ms (1 thread) / 8 ms (4); `collect` of f64 19 / 8 ms; `collect` of 8-byte messages 46 / 18 ms; pushed 30–70 ms | DESIGN §6.4, `benches/gather.rs` |
| `diff` between two versions | ~25 ms, 1 thread | DESIGN §7, `benches/version_diff.rs` |
| `eigen`, k = 20, f32, adjacency, largest | 1,037 steps: 9.9 s (1 thread) / 3.7 s (4); ARPACK 1,020–1,313 matvecs | DESIGN §6.5, `bench/results/eigen/` |
| `eigen`, k = 20, f32, normalised Laplacian, smallest | 1,342 steps: 13.1 s / 5.1 s; ARPACK 1,406–1,477 matvecs. At 1M nodes and 10M edges: not converged in the then default 4,100 steps (226 s, 4 threads), where ARPACK took 4,343 matvecs (712 s); 4,590 steps to converge under the M7 default (`results/eigen/scaling/`); adjacency 3,174 steps (173 s) vs ARPACK 3,325 (499 s). Steps grow as √N on a uniform random graph and stay flat (300–386) on a 40-community block model (DESIGN §12) | DESIGN §6.5, `bench/results/eigen/` |
| BFS, 1 / 2 / 3 / 5 of 5 relations (1M weighted edges, 1 thread) | 2.0 / 4.9 / 7.2 / 5.3 ms | DESIGN §5.3, `benches/relation_layout.rs` |
| Build, forward + reverse, 5 relations | 104 ms, 28.8 MB forward | DESIGN §5.3 |
| Structural kernels, 1 thread: `core_number` / `strong_components` / `topological_order` / `dag_longest_path` / `minimum_spanning_forest` unweighted, weighted (`triangles`: 67 ms; 610 ms before M11's degree-ordered kernel) | 56 / 26 / 9 / 17 / 62, 109 ms | DESIGN §6.7, `benches/structure.rs` |

**The structural kernels at scale** (DESIGN.md §6.7), through SQL, release
build, PG 16, `warren.threads = 4`, on the shared 4-vCPU Xeon (another
worker's builds ran beside it): G(n, m) random graphs from a seeded
`random()`, `g` directed and unweighted with reverse CSRs, `dag` its edges
from the smaller id to the larger, self-loops dropped, weighted 1–100,
without reverse CSRs. Each call on a prewarmed version, `persist := false`;
two runs, seconds. The memory column is the backend's peak RSS past the
loaded version (VmHWM less RssAnon after `prewarm`), where the kernel's
peak exceeded the load's.

| Call | 1M nodes, 10M edges | 10M nodes, 100M edges | Peak past the version (10M) |
|---|---|---|---|
| `refresh` of `g` / `dag` | 8.2 / 9.6 | 175 / 192 | |
| `triangles` (4 threads; for comparison; before M11's degree-ordered kernel, not re-measured) | 2.6, 2.8 | 45.2, 42.7 | 2.0 GB |
| `core_number` (degeneracy 14) | 1.0, 1.0 | 22.8, 23.5 | 2.2 GB |
| `strong_components_vector` (89 / 867 components) | 0.62, 0.60 | 17.0, 16.8 | 0.7 GB |
| `components_vector` (PL/pgSQL, weak; for comparison) | 0.33, 0.36 | 7.1, 6.2 | |
| `topological_order` | 0.18, 0.18 | 4.9, 4.7 | |
| `dag_longest_path` | 0.31, 0.31 | 7.7, 7.6 | |
| `minimum_spanning_forest`, weighted (all rows, counted) | 3.1, 2.9 | 63.6, 55.7 | |
| `minimum_spanning_forest`, `weighted := false` | 1.7, 1.7 | 35.3, 36.2 | |

All run on one compute thread except `triangles` and the sort of
`core_number`'s adjacency. `core_number` spends most of its time building
the symmetric adjacency (16 bytes per stored edge, the bulk of its peak).
The forest is the slowest: each Borůvka round scans every stored edge with a
random access per edge, and random weights take more rounds than unit
weights.

**The centralities and similarities at scale** (DESIGN.md §6.7), through
SQL, release build, PG 16, `warren.threads = 4`,
`warren.max_compute_memory = 12GB`, on the shared 4-vCPU Xeon (15 GB):
G(n, m) random directed graphs from a seeded hash, weights 1–16, with
reverse CSRs. Each call on a prewarmed version, `persist := false`; two
runs at 1M, one at 10M, seconds.

| Call | 1M nodes, 10M edges | 10M nodes, 100M edges |
|---|---|---|
| `refresh` (for comparison) | 10.9 | 223 |
| `betweenness_centrality`, `k := 64` / 16 (1M / 10M), unweighted | 12.4, 16.3, 14.2, 13.8 | 130, 94 |
| the same, weighted (Dijkstra) | 23.1, 24.7 | 216 |
| the same, `directed := false`, unweighted (builds the symmetric adjacency) | 26.2, 21.0 | 134 |
| `closeness_centrality`, `k := 64` / 16, unweighted | 9.9, 5.6 | 58 |
| `harmonic_centrality`, `k := 64` / 16, weighted | 28.9, 18.5 | 165 |
| `most_similar`, one node, `k := 10`, jaccard / adamic_adar (rows read node by node) | 0.0015 / 0.0004, 0.0025 / 0.0005 | 0.0033 / 0.0010 |
| `similarity` of 100,000 random pairs (node by node) | 0.20, 0.19 | 0.44 |
| `similar_pairs`, `k := 10`, every 100th node (builds the symmetric adjacency) | 0.68 / 0.58 (jaccard / adamic_adar) | 10.6 / 9.6 |
| `similar_pairs`, `k := 10`, every node (10M / 100M rows, counted) | 12.0, 11.7 | 318 |

A source costs one search and, for betweenness, a second pass over what it
reached: about 0.8 s on one thread at 1M nodes and 20–30 s at 10M, where
the random reads of the per-node arrays (each search's 8-byte distances and
counts over 10M nodes) are memory-bound, so four threads give less than
four times one. Exact betweenness (`k := NULL`) at 1M nodes would take
about 55 hours on this box: the exact forms are for graphs of thousands of
nodes, and the oracle. Weighted searches cost about twice as much (a heap per source). At 1M
nodes four weighted searches took about 0.5 GB past the version, against
about 0.13 GB unweighted: the measurement predates the PR #29 review, when
Dijkstra's queue was a lazy-deletion heap that grew on the pool to an entry
per improving relaxation; it is now an indexed heap of 16 bytes per node
per search, allocated with the rest of the scratch (0.06 GB more for four
searches at 1M nodes).

The closeness and harmonic rows above, and the soak's below, predate
multi-source search: unweighted closeness and harmonic now search 64
sources at a time in one breadth-first search over all the compute
threads (DESIGN.md §6.7), with 40 bytes of scratch per node per call
rather than 16 per thread. Measured in the domain crate
(`cargo bench -p warren-core --bench centrality`: 1M nodes, 10M random
directed unweighted edges, `warren.threads`' equivalent 4, the same
shared 4-vCPU Xeon with 15 GB, other builds running, load average 3–12;
criterion means of 10 iterations, each run listed):

| Sources | closeness, per source | closeness, batched | harmonic, per source | harmonic, batched |
|---|---|---|---|---|
| 16 | 3.8, 6.6 s | 0.52–0.72 s (4 runs) | 4.3, 2.7 s | 1.05, 0.54 s |
| 64 | 10.1, 17.0 s | 0.73–1.05 s (4 runs) | 20.7, 11.0 s | 0.65–1.23 s (4 runs) |
| 256 | 50.0, 37.5 s | 1.8–3.7 s (6 runs) | 80.5 s | 3.0, 3.5 s |

At 64 sources that is 0.65–1.23 s against 10.1–20.7 s, 9–32 times
faster. Betweenness from 16 sources took 13.2 and 16.5 s on the same
graph (unchanged: one source per thread, so the benchmark no longer runs
it). Weighted closeness and harmonic still search one source per thread.
Not re-measured through SQL at 10M nodes.

`directed := false` and `similar_pairs` first build the symmetric
adjacency, 16 bytes per stored edge (32 weighted): at 10M nodes and 100M
edges weighted undirected betweenness needs about 4.6 GB of scratch,
past the default `warren.max_compute_memory`, which refuses them (53400).
`similar_pairs` over every node costs `Σ_w |N(w)|²` (about 4·10⁹ steps at
10M nodes and average degree 20) plus a row out per pair; its result is
charged at 24 bytes per row as rows are found. One node's ranking or a batch of pairs reads
only the rows it needs when the graph has reverse CSRs, so it costs
milliseconds at any size.

The library at 1M nodes and 10M undirected edges (G(n, m), 20M stored
edges, average degree 20; release build, `warren.threads = 4`, a shared
4-vCPU Xeon; each time is a fresh session, so it includes the 0.35 s load
of the version; best of three, `bench/results/library/`):

| What | Measured |
|---|---|
| `pagerank`, 20 iterations (for comparison) | 1.62 s |
| `eigenvector_centrality`, 20 iterations | 1.21 s; converged (defaults) in 0.70 s |
| `katz_centrality`, 20 iterations (`alpha := 0.01`) | 1.15 s |
| `hits`, 20 iterations (two SpMVs each) | 1.86 s |
| `triangles` (before M11's degree-ordered kernel; not re-measured) | 4.19 s |
| `clustering` (`triangles` plus degrees; likewise) | 4.45 s |
| `label_propagation`, 10 iterations (min-label, tier 1) | 0.97 s |
| `label_propagation_majority`, 1 iteration (0.4.0) | 21.3 s |
| `label_propagation_majority`, 5 iterations (0.4.0) | 100.5 s (~20 s an iteration) |

The SpMV centralities cost 40–90 ms an iteration. Majority-vote label
propagation is tier 2. In 0.4.0 each iteration `gather`ed 20M labels and
counted them in the executor (`unnest`, `GROUP BY`, `DISTINCT ON` over 21M
rows), about 300 times an SpMV iteration. Since 0.5.0 `gather` counts them
in-engine (`combine := 'mode'`, `include_self := true`; DESIGN.md §6.4,
§6.7), and only the 1M winners pass through the executor. Measured on the
same graph and build, old and new body side by side, on the same 4-vCPU
machine while three other builds ran (load average ~9, so every time is
slower than in the table above), each a fresh session:

| What | 0.4.0 (`collect`, counted in SQL) | 0.5.0 (`mode`) |
|---|---|---|
| `label_propagation_majority`, 1 iteration | 37.5 s | 3.0 s |
| `label_propagation_majority`, 5 iterations | 168.3 s | 13.2 s (~2.6 s an iteration) |

Both give the same labels. Of an iteration's ~3 s, `gather` alone (the
kernel and its 1M result rows) takes 2.8 s, so an iteration still costs
several times an SpMV centrality's. Run to
convergence (the default `max_iter` of 100) on this graph it took 300 s,
about 100 iterations' worth: a synchronous majority vote need not settle.

**Contraction at scale** (DESIGN.md §6.6, `bench/results/communities/`;
release build, 4 shared vCPUs, through SQL, each call in a fresh session so
including the version's load). A block graph of N nodes in blocks of 1,000
with five undirected edges per node (10N stored), contracted by
`components_vector` over the blocks' own edges:

| Call | 1M nodes | 10M nodes |
|---|---|---|
| `refresh` of the graph (for comparison) | 3.1 s | 36.5 s |
| `prewarm` (the load every call below includes) | 0.13 s | 1.45 s |
| `contract` into N / 1,000 communities | 0.35 s | 4.2 s |
| `contract` by dense id (N communities, the worst case) | 1.7 s | 13.7 s |
| `project` (`count`, `mean`), `lift` | 0.14–0.16 s | 1.9–2.0 s |
| `modularity` of a partition | 0.44 s | 4.9 s |
| `louvain` (3 levels at 1M, 5 at 10M) | 5.8 s | 93 s |
| `leiden`, one iteration (7 levels at 1M, 8 at 10M) | 8.3 s | 119 s |
| `leiden`, the default two iterations (13 levels at 1M, 18 at 10M) | 20.9 s, on a slower day (one iteration 10.4 s, `louvain` 9.1 s) | 246.5 s (`louvain` 115.4 s in the same run); modularity 0.900, peak 5.28 GB, 7.8 GB of segments written |

A level's time, as the difference of runs with `max_levels` 1, 2 and 3:
Louvain 2.4, 2.8 and 0.3 s at 1M, and 31.6, 49.5 and 11.8 s at 10M;
Leiden 2.3, 3.2 and 1.9 s at 1M (0.9 s for the rest), and 29.4, 49.2 and
22.3 s at 10M (18.2 s for the rest). The first level runs on the whole
graph; every later one contracts the whole graph by the partition found so
far, so each small late level costs a contraction. Leiden's second iteration
repeats the levels from the first's partition, so it costs about as much
again, and raises modularity on these blocks from 0.837 to 0.898.
Those `leiden` figures are 0.7.0's and earlier: since 0.8.0 each later
level contracts the level below, in memory, and writes nothing
(DESIGN.md §6.7, *Leiden's levels*).

**The library at the soak's scale** (0.4.0 soak, `bench/results/soak/`,
release build of 5b0bf81, PG 16, `warren.threads = 4`, every limit at its
default; the 4-vCPU Xeon): the random graph of 10M nodes and 100M
directed edges, one relation, unweighted (1.2 GB cached), each call once
on the cached version, `persist := false` where the call takes it.
Seconds, and the backend's private peak, version included:

| Call | 10M nodes, 100M edges |
|---|---|
| `eigenvector_centrality` / `katz_centrality` / `hits`, 20 iterations | 15.0 / 9.8 / 17.3 s, 1.8–2.0 GB |
| `clustering` | 47.5 s, 3.0 GB |
| `core_number` | 22.8 s, 3.2 GB |
| `strong_components_vector` | 15.8 s, 1.7 GB |
| `minimum_spanning_forest` (rows counted) | 54.4 s |
| `betweenness_centrality` / `closeness_centrality` / `harmonic_centrality`, 16 sources | 61.1 / 29.4 / 30.9 s, 2.0–2.6 GB (closeness and harmonic before multi-source search, above) |
| `most_similar`, one node / `similarity`, 100,000 pairs | 2 ms / 0.55 s |
| `similar_pairs`, `k := 10`, every 100th node | 10.2 s, 3.6 GB |
| `louvain` | stopped for disk after 383 s (7.5 GB of segments written) |
| `leiden` (two iterations) | refused (53400) after 59 s, in its first refinement |

With five weighted relations and no merged CSR (3.44 GB cached) each takes
1.4–3.5× as long (weighted searches, rows merged on the fly), within the
default limits but `louvain` (stopped for disk) and `leiden` (refused).
With the merged CSR (6.0 GB, over the cache cap) every call also pays the
22 s load, and `similar_pairs`, `louvain` and `leiden` are refused (53400).
Those two rows predate two fixes (DESIGN.md §14 item 40), not yet re-run
at this scale: level graphs store forward CSRs only, which halves a
directed graph's level segments (measured at 1M nodes, the same
partitions), and Leiden's refinement charges its real peak, so its first
refinement fits.

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

**`reachable_set`'s order within a depth.** Its rows come by depth. A depth
whose frontier holds more than a twenty-fourth of the id space (the
version's dense ids, tombstoned ones included), and whose unvisited ids
have fewer in-edges than the frontier has out-edges, is read backwards
from the reverse CSR. Its nodes then come in dense id order (the id map's
order) rather than the order `bfs` reaches them, so which rows a
select-list `LIMIT` keeps can differ from `bfs`'s. Sort with `ORDER BY`
when the order matters. `last_stats()` counts the row entries scanned,
which for such a depth are fewer than pushing it would scan (on a graph of
several relations without the merged CSR, read with every relation, only
usually).

**Integer `sum` overflows instead of widening.** `gather(…, 'sum')` of
integer messages returns `bigint` and raises `bigint out of range` on
overflow, where SQL's `sum(bigint)` returns `numeric`. `i64` arithmetic in
`spmv` and `vector_map` errors on overflow likewise (except the `min`/`max`
identities, which stand for ±∞). Use `f64` vectors or `double precision`
messages where totals may exceed 9.2 × 10¹⁸.

**Repeated eigenvalues.** Lanczos from one start vector finds one direction
per eigenspace, so a repeated eigenvalue may appear fewer times than its
multiplicity. The ones every connected component gives (0 of both
Laplacians, 1 of the random walk) and every bipartite one (−1 of the random
walk, 2 of the normalised Laplacian) are exempt where they are at the
wanted end (`smallest` for the Laplacians' 0, `largest` for the others):
`eigen` returns each copy exactly, as its component's eigenvector. Other
repeated eigenvalues, such as those of identical components or of a
symmetric structure, may still be missed; `eigen` warns when the active
nodes form several components and some pair came from Lanczos. Non-symmetric spectra
are not computed: `symmetrize := false` is accepted only when the matrix
is already symmetric.

**Clustered small eigenvalues are slow.** Lanczos needs about
√(spread / gap) steps per digit, where gap is the distance between the
wanted eigenvalues and their neighbours: on a long, thin graph such as a
road network the smallest Laplacian eigenvalues are 10⁻⁷–10⁻⁴ apart in a
spectrum two wide. `k = 20` smallest of usroads-48's normalised
Laplacian (126K nodes) takes about 20,500 steps (69 s), and of
roadNet-TX's largest component (1.35M nodes) more than the default
`max_iter` (DESIGN.md §12). Progress shows in `warren.progress()`, and a
solve that reaches `max_iter` returns its pairs with a `WARNING`. Ask for fewer pairs, loosen `tol`, or raise
`max_iter` and `statement_timeout` together.

**Stable functions in a `WHERE` clause are called per row.** Write
`WHERE x < (SELECT warren.vector_reduce(…))`, not `WHERE x <
warren.vector_reduce(…)`, so the call runs once.

**The shared cache's limits** ([the shared cache](#the-shared-cache)). On
PostgreSQL 15 and 16 it needs `shared_preload_libraries = 'pg_warren'` and
a restart. A version larger than `warren.shared_cache_memory`, one that
does not fit beside the versions in use, or one that does not fit
`/dev/shm` with a quarter of it left free (Docker's default is 64 MB) is
loaded by each backend that reads it, within its own
`warren.max_cache_memory`. The cap is one for the whole cluster: one
database's versions can evict another's, and there are no per-database
shares. An idle cluster keeps up to the cap of shared versions until they
are evicted (`warren.evict`) or the server restarts. Transient and `bool`
vectors are never shared. Wherever a version is read privately (the
shared cache unavailable or off, a version it refuses or has no room for,
one the current transaction created), each worker of a parallel plan
that calls a `PARALLEL SAFE` graph read loads its own copy, so the plan
can hold one per worker plus the leader's ("Parallel query", above;
docs/semantics.md, "Parallel query labels").

**Behind a connection pooler.** In session pooling a client keeps its
server backend, and PgBouncer's `server_reset_query = DISCARD ALL` runs
when it leaves; `DISCARD ALL` also drops pg_warren's transient vectors
(every role's on that backend) and `last_stats()`, so the next client
starts clean. In transaction pooling (PgBouncer's `pool_mode =
transaction`) each transaction may run on another backend, and nothing
is reset between clients, so what lives in a session does not follow
the client and is left for whoever gets that backend next
(`test/pooler_soak.py` checks both modes):
- a `SET` of `warren.*` (or any setting): use `SET LOCAL` in the
  transaction, or `ALTER ROLE … SET` / `ALTER DATABASE … SET`;
- a transient vector (`persist := false`): create it, read it and drop it
  (`warren.drop_vector`) in one transaction; otherwise a later
  transaction may not find it (42704), and it stays in the backend's
  memory, counted against `warren.max_cache_memory`, where another
  client of the same role can read it;
- `warren.last_stats()`: read it in the traversal's transaction;
- SQL `PREPARE`: use the protocol's prepared statements, which PgBouncer
  1.21 and later tracks across backends when `max_prepared_statements`
  is set.

Persisted vectors, graphs and versions are in the catalog and read the
same through any backend.

**Not yet supported:** incremental maintenance (every
`refresh` is a full rebuild); `refresh` or `compact` of a graph derived by
contraction (contract again); edge properties beyond one weight.
(`DROP ROLE` still cleans up nothing
by itself: see docs/semantics.md, "Dropping a role".)

**Cost shapes to know.**

- `triangles` orients each edge from its endpoint of lower degree and
  finds each triangle once: about M·√M at worst, near-linear on sparse
  graphs, hubs or not. It holds 8 bytes per stored edge, 24 per node and 4
  per node per compute thread.
- `core_number`, `strong_components_vector`, `topological_order` and
  `dag_longest_path` are linear (the DAG functions × log N for their heap);
  `minimum_spanning_forest` costs a pass over the edges per Borůvka round,
  at most log₂ N rounds. All five run on one thread.
- `gather(…, 'collect')` of growing sets costs memory O(N × neighbourhood);
  it suits local neighbourhoods, not community-wide state; contract to a
  quotient graph for that ([communities.md](communities.md)).
- `louvain` and `leiden` cost a pass over the stored edges per sweep of
  local moving (after the first sweep, only over the rows of nodes whose
  neighbours moved) and a contraction per level: of the whole graph for
  `louvain`, and for `leiden` (since 0.8.0) of the level below's
  undirected view, held in memory and charged to
  `warren.max_compute_memory` with the rest of the run (at its peak the
  graph's view, the level's, and the next level's while it is
  contracted). Their scratch is charged to `warren.max_compute_memory`: the undirected view
  (16 bytes per stored edge, both directions of a directed graph, and per
  node), up to 56 bytes per node while moving, and in Leiden's refinement
  24 bytes per node (beside its 8-byte-a-node label vector) plus 48 per
  community (and, for the communities being refined at once, one per
  thread, 24 bytes per member). Each of `louvain`'s levels (and
  `leiden`'s before 0.8.0; it now writes none) writes
  its graph's version (segments and WAL), each relation's forward CSR
  only (no reverse or merged CSR, whatever the input's layout); the
  level's graph is dropped when the next is built, but its rows stay until
  `VACUUM` after the run's transaction ends, so the disk a run needs grows
  with its levels: on the 1M-node block graph (75 MB of segments)
  `louvain` wrote 166 MB of segments and about 190 MB of WAL, `leiden`
  294 MB and 323 MB for one iteration (516 MB of segments for the default
  two); at 10M nodes (0.72 GB of segments) `louvain` wrote 2.5 GB of
  segments and `leiden`, with its default two iterations, 7.8 GB (and as
  much WAL).
- **At 10M nodes, on graphs with little community structure, Louvain
  needs several GB of disk, and Leiden did before 0.8.0** (the 0.4.0 soak's uniform random graphs
  of 10M nodes and 100M edges; see "The library at the soak's scale"
  above): a random graph's levels merge little (1M nodes → 448,022
  communities at level 1), so several levels keep nearly all the edges,
  and each is stored with 8-byte weights.
  - `louvain` had written 7.5 GB of segments (and 9.1 GB of WAL) when the
    soak stopped it for disk after 383 s, before level graphs stored
    forward CSRs only, which halves that for a directed input: at 1M
    nodes and 10M edges its levels now write 293 MB of segments (536 MB
    before), `leiden`'s 945 MB (1.76 GB), for the same partitions
    (`bench/results/communities/random-1m-level-graphs.txt`). Those rows
    stay on disk until `VACUUM` after the run's transaction ends.
  - `leiden` was refused with 53400 in its first level's refinement,
    which charged 88 bytes per node beside the 3.2 GB undirected view; it
    now charges about 38 there and fits the 4GB default, but its eight or
    so large levels are likely still past a disk like the soak's.
  - `leiden` is native since 0.8.0 and keeps its levels in memory, so it
    writes no level; not yet re-run at this scale. The planned fix for
    `louvain`'s disk is for `contract` to keep a quotient in memory for
    the rest of the statement instead of writing it as a version, which
    helps every multilevel algorithm users write (DESIGN.md §6.7, §13,
    §14 item 40).
- `contract` is linear: a counting sort of the nodes by community, then one
  pass over the stored edges, on one thread, plus a build of the quotient
  (its edges, at most the parent's). Its scratch, charged to
  `warren.max_build_memory`, is about 24 bytes per node at its peak and
  about 33 bytes per community, beside the parent version (cached) and the
  label vector (charged to `warren.max_compute_memory`). `project` and `lift` are one
  pass over the nodes and read the 8-byte-per-node membership.
- A persisted vector write costs WAL (8 bytes × id_space per `f64` column)
  on every call; keep iterative scratch transient (`persist := false`).
- `walk` in `paths` mode can be exponential in the number of hops; it is
  always bounded by `max_results`.
- `distance('edit')` across two graphs (`graph2`) rebuilds nothing: it
  maps the second graph's edges into the first's dense ids as it reads
  them, looking each up by binary search in the first graph's row, so it
  holds only the id mapping, 8 bytes a node of the second graph, charged
  to `warren.max_compute_memory` (88 MB at Appendix A.2's 11M nodes).
  Before 0.8.0 it rebuilt the second graph's CSRs and charged 96 bytes an
  edge for them, so A.2 at 10M asked for 10.9 GB and was refused with
  53400 at the 4GB default. Within one graph, `edit` aligns by dense id
  and maps nothing.
- `gather` with messages of a type other than the numbers, `boolean`,
  `text`, `varchar`, `char` and `bytea` (`jsonb`, `numeric`, arrays...)
  sends each through its type's binary send and receive functions
  (DESIGN.md §6.4). Text and `bytea` travel as their bytes: at 10M nodes
  and 11.5M messages a `collect` of cheap text messages takes 14.5 s,
  against 13.5 s as `bigint` (Appendix A.3 at 10M); the message query's
  own cost comes on top.
