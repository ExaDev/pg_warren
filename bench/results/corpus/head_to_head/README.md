# Head-to-head: pg_warren, igraph, Neo4j, Neo4j GDS and pgGraph

`bench/corpus/suite.py head-to-head` output (DESIGN.md §12 "Real graphs",
§13 M11; `bench/corpus/README.md` describes the runner). There is one file per
dataset: every medium dataset, and four of the tiny ones (lesmis, which is
weighted; polblogs; facebook; wn18rr). The other tiny graphs are left to the
correctness suite. karate, celegans, london_transport and kg-codex-s are
small enough that their times would be round trips, and transport-airline
was left out by design. The large tier is deferred (DESIGN.md §13).
Each file holds the provenance, the machine, every engine's versions and
settings (`describe`), its load phases, and for each workload the cold time,
the warm times and their median, the check against the reference and the
partition's modularity. The files at the top level are the M11 run,
against pg_warren 0.7.0; `pggraph-1.2.1/` holds a later run of pgGraph
1.2.1 beside pg_warren 0.8.0 and igraph, described in its own section
below.

**Read the caveats before quoting a number.** This is one run on one shared
virtual machine. The engines keep their results differently, are cold in
different ways, and do not use the same number of threads. One table cell
is not a verdict on an engine.

## Provenance

| | |
|---|---|
| pg_warren | 0.7.0, release build of ae51d77 (`cargo pgrx package` into a private tree), PostgreSQL 16.15, a scratch cluster with the soak's settings (`settings` in each file: `shared_buffers = 1GB`, `fsync = off`, `synchronous_commit = off`), `warren.threads = 4`; `pg_warren.so` sha256 `18124b35bc64aea8d1deba8e1040ac5cb2b9e733fcabd84ce4bf93f01874274a` |
| igraph | python-igraph 1.0.0 (igraph's C core), CPython 3.13.16, in the runner's process |
| Neo4j | Community 2026.10.1 (sha256 `afcf4986…3e78`, Neo4j's published checksum), OpenJDK 21.0.12, heap 4 GB (initial = max), page cache 2 GB, the shipped G1 flags, auth off, Bolt on loopback, Python driver 6.4.1 |
| GDS | Graph Data Science 2026.10.1 (sha256 `1cdc0372…c133`, pinned at the first fetch: GDS publishes no checksum), no licence key (Community: at most 4 threads) |
| Harness | 09c0a6e (`bench/corpus/head_to_head/`), `suite.py head-to-head <the 12 datasets> --extension-revision ae51d77 --extension-profile release --timeout 300`, with the defaults `--repeat 5 --budget 120 --threads 4 --betweenness-work 5e8` |
| Machine | 4-vCPU Intel Xeon VM (no SMT), 15 GiB RAM, no swap, kernel 6.18.44, shared with other agents' work (`hardware.load_average` in each file: 0.8–1.6 at the start of each dataset) |

Downloads, extracted installs and Neo4j's databases live under
`/var/tmp/warren-corpus/head-to-head`, never in the repository; each
dataset's Neo4j database was deleted once it had been measured.

## Caveats

Read every number here with these in mind.

- **One machine, shared, one run.** A 4-vCPU Intel Xeon VM (no SMT), 15 GiB
  RAM, no swap, kernel 6.18.44, with other agents' work on it (load
  averages are in each file, `hardware.load_average`). The engines ran one
  at a time, each dataset in turn; nothing else of the head-to-head ran
  meanwhile, but the Postgres cluster stayed up (idle) while Neo4j ran, and
  the reverse. One run of the suite: no confidence intervals beyond the
  warm runs' spread (`warm_seconds` in each file).
- **Threads are not matched one for one, and cannot be.** pg_warren ran with
  `warren.threads = 4` and GDS with `concurrency: 4` (Community edition's
  maximum); igraph's algorithms are sequential and Neo4j Community runs a
  Cypher query on one thread. A row where igraph wins is a sequential C
  library beating 4 threads; a row where pg_warren wins over igraph may be
  its threads.
- **Times are wall clock at the client**, around the call that computes the
  result into the engine's own memory: an SQL statement over a local
  socket (pg_warren), a Python call (igraph), a Bolt query over loopback
  (Neo4j). Round trips (about 0.1–0.3 ms for pg_warren and 1–10 ms for
  Bolt here) are in the times and dominate the sub-millisecond rows.
  Reading the answer out is not timed.
- **Each engine keeps its result differently.** pg_warren's library
  functions (`pagerank`, `louvain`, `leiden`, `degree_vector`) persist
  their output vector to the catalog, since they have no `persist`
  argument; `triangles` and `betweenness_centrality` ran with
  `persist := false`. GDS ran in `mutate` mode (a node property in the
  in-memory projection). igraph returns a Python list. Cypher aggregates
  in the server. The Postgres cluster ran with the soak's settings,
  `fsync = off` and `synchronous_commit = off` among them, so a persisted
  vector costs no flush; Neo4j's defaults were kept (its transactions are
  durable).
- **Cold means something different in each engine.** pg_warren: after
  `warren.evict`, so the version is read back from its segments (the heap
  pages are in shared buffers or the OS cache; it is not a cold disk).
  Neo4j: after a server restart (empty page cache, plan cache and a cold
  JVM: JIT and class loading), and for GDS new projections too; the OS
  file cache stays warm. igraph keeps nothing between calls, so its cold
  run is just the first. Neo4j's cold column is mostly the JVM.
- **Loading is not one thing.** pg_warren's `ingest` is `load.sql` (CREATE
  TABLE, `\copy` from the CSVs, ANALYZE) and its `build` one
  `warren.refresh` (the median of three); igraph's `ingest` reads the CSVs
  with Python's csv module, its `build` makes the graph and folds parallel
  edges (and the undirected view); Neo4j's `import` is `neo4j-admin
  database import full` offline (a JVM start included), `start` the server
  start until Bolt answers and `index` the id index; GDS's `project` (and
  `project undirected` for a directed graph) the in-memory projections
  from the store. pg_warren's and igraph's loads read the same CSVs; the
  sum of the phases is the time to first query.
- **What each engine computes.**
  - *PageRank*: 20 propagation steps from the start vector, damping 0.85,
    in pg_warren (its library's loop, NetworkX's iteration: scores sum to
    1, a dangling node's rank spread evenly) and GDS (`maxIterations: 21`,
    since GDS counts its initial vector as the first iteration;
    unnormalised: x₀ = 0.15, a dangling node's rank lost). igraph's PRPACK
    solves for the fixed point instead; it cannot stop at 20 steps. Each
    is checked against a reference of its own convention (within 10⁻⁹),
    so the three answers are each right and not equal to each other.
  - *Communities*: Louvain and Leiden are heuristics with their own
    randomness, defaults and stopping rules (pg_warren: seed 0, tolerance
    10⁻⁷; igraph: seeded Python RNG, Leiden 2 iterations; GDS: its
    defaults, Leiden `randomSeed: 1`). Each partition is scored by the same
    modularity (the reference's, in the undirected view) and held to the
    correctness suite's bound: at least igraph Louvain's modularity less
    0.02. The modularity each reached is in the files (`check.modularity`).
  - *Projections*: GDS reads a directed graph's triangles, Louvain and
    Leiden on an `UNDIRECTED` projection with parallel relationships
    folded (`SINGLE`), so a reciprocal pair weighs 1 where pg_warren's
    undirected view weighs it 2; pg_warren and igraph optimise the latter.
  - *BFS depths*: pg_warren `bfs` with no depth bound; igraph
    `distances`; GDS `allShortestPaths.delta` with unit weights, mutated
    as relationships (its `stream` mode holds every target's path and ran
    out of a 4 GB heap on roadNet-TX); Cypher `shortestPath` from the
    start to each other node (the newer `ANY SHORTEST` passed 600 s on
    facebook's 4,039 nodes).
  - *Degree*: distinct out-neighbours; Cypher counts them per node, GDS's
    degree reads its `SINGLE`-folded projection.
  - *Shortest paths*: 25 seeded pairs in one call (one SQL statement, one
    Cypher query, a loop of igraph calls). igraph's `distances` with a
    target stops at the target but first builds the whole graph's
    adjacency list, on every call; the pgGraph run's `paths/` uses the
    faster `get_shortest_path` (see that section). pg_warren's
    `shortest_path` and Cypher's `shortestPath` search from both ends.
  - *Triangles*: per node in pg_warren, igraph (local transitivity times
    the pairs of neighbours, on the simple graph) and GDS; a total in
    Cypher.
- **Not comparable, or not run.** Cypher has no PageRank, components,
  Louvain, Leiden or betweenness (listed below, with why) and no weighted
  shortest path; APOC was not used. pg_warren has no weighted
  single-source shortest path (only `dijkstra` between two nodes), so the
  weighted dataset (lesmis) is compared on pairs only. Betweenness runs
  only where N·M ≤ 5·10⁸ (lesmis, polblogs, facebook). The large tier
  (LiveJournal, ogbn-mag) is deferred (DESIGN.md §13). Neo4j Enterprise
  and GDS Enterprise (more threads, the parallel Cypher runtime) were not
  tried.
- **Neo4j's configuration**: Community 2026.10.1 and GDS 2026.10.1 on
  OpenJDK 21.0.12, heap 4 GB (initial = max), page cache 2 GB, the shipped
  JVM flags (G1), auth off, Bolt on loopback, Python driver 6.4.1. GDS
  without a licence key is Community: 4 threads at most.

## Findings

The comparisons below use warm medians unless they say otherwise. "N× faster"
compares like with like under the caveats above.

**Correctness.** Every counted time comes from an answer that matched the
reference. That means exactly for degrees, reach, depths, path lengths,
components and triangles; within 10⁻⁹ for PageRank (under each engine's own
convention) and betweenness; and at or above the quality bound for
communities. Three things kept a time out of the tables:

- *GDS Leiden on email-enron fell below the bound.* It reached Q = 0.590,
  against a bound of 0.595 (igraph Louvain's 0.615 less 0.02). pg_warren
  reached 0.626 and igraph's Leiden 0.624. An earlier run of the same
  dataset gave GDS 0.594, also below, so this is GDS's Leiden on its folded
  projection, not one unlucky seed.
- *GDS Dijkstra on roadnet-tx stopped at its own memory guard.* The error
  was "Memory required to run Dijkstra (66409280b) exceeds current
  available memory (19565798b)", from a 4 GB heap after the cold restart
  and projection. An earlier GDS-only run of roadnet-tx, made while the
  harness was being fixed, finished it in 7.4 s cold and 6.6 s warm. That
  run is not in the table.
- *Cypher passed the 300 s timeout* on BFS depths for the two road networks,
  and on triangles for yago3-10 and hetionet.

pg_warren had no wrong answer. Its partitions reached the highest
modularity of the three engines on 7 of 12 datasets for Louvain and 6 of 12
for Leiden, and within 0.0015 of the best on the rest. The modularity each
engine reached is `check.modularity` in each file.

**Where pg_warren wins.**

- *Shortest paths between pairs.* pg_warren's bidirectional BFS
  (`shortest_path`) beat igraph on 11 of 12 datasets. The margin was 2–220×:
  dblp took 6 ms against 1.04 s, and roadnet-tx 1.46 s against 2.98 s.
  igraph's `distances` with a target builds the whole graph's adjacency
  list for each pair before it searches; against igraph's faster
  `get_shortest_path`, pg_warren 0.8.0 won on 7 of 12 (the pgGraph section
  below). pg_warren also beat GDS Dijkstra by 5–370×, and Cypher's
  `shortestPath` warm on 11 of 11. Cypher's warm times here are 18–330 ms;
  roadnet-tx took 5.9 s.
- *Louvain.* pg_warren beat GDS everywhere, by 1.7–280×: GDS took 3–16 s
  warm on the medium tier. It beat igraph on 7 of 12 datasets, the large
  sparse ones in particular: dblp 1.16 s against 5.49 s, roadnet-tx 3.59 s
  against 15.8 s, and usroads-48 0.28 s against 0.76 s. These wins may come
  from its 4 threads against igraph's one.
- *Time to first query.* pg_warren's ingest and build (0.01–3.5 s) are on a
  par with igraph reading the same CSVs into Python. Neo4j needs 13–18 s
  before the first query: 3.7–8.5 s to import, 7.2–8.3 s to start the
  server and 1.3–2.6 s to build the id index, plus 1.0–1.8 s for the GDS
  projection. The first Cypher query after a start then takes about 2 s
  (a cold JVM).
- *Traversals against Neo4j.* BFS depths and BFS to depth 6 beat GDS warm
  on every dataset except dblp, where the two tie within 5%. BFS depths
  was 3–9× faster on the medium tier, for example roadnet-tx at 0.38 s
  against 2.0 s. Cypher took seconds for the same answer, or passed 300 s.
  BFS to depth 6 also beat igraph on roadnet-tx (0.38 ms against 0.60 ms),
  and BFS depths beat it on yago3-10 (4.9 ms against 13 ms).

**Where pg_warren loses.**

- *Fixed cost per call.* The cheapest calls take 0.2–2 ms on the tiny
  graphs, where igraph answers in microseconds. `degree_vector` costs
  0.5 ms on lesmis against igraph's 2 µs, and 3–90 ms on the medium tier,
  for example 32 ms on dblp against 1.7 ms. pg_warren pays for an SQL
  statement, the cache lookup and, for the library functions, persisting the
  output vector to the catalog. GDS's `mutate` calls cost a flat 7–11 ms
  for the same reason (Bolt and the procedure framework).
- *PageRank.* pg_warren was slower than igraph everywhere: 1.02× on dblp,
  1000× on lesmis. Two things contribute. The library's loop is PL/pgSQL, with about
  1.5 ms of fixed overhead per iteration: lesmis's 77 nodes take 29 ms,
  against 28 µs. igraph's PRPACK also solves a linear system instead of
  iterating. GDS was faster than pg_warren on yago3-10 (0.15 s against
  0.17 s) and on roadnet-tx (0.62 s against 1.0 s).
  *Since addressed* (0.8.0): `pagerank` is a native kernel, to the bit of the
  loop (DESIGN.md §6.7). Re-timed against igraph alone ("PageRank in
  0.8.0", below): 2.9–9.3× faster than this run, faster than igraph on 7
  of 12 datasets (roadnet-tx 0.22 s against 0.81 s, dblp 0.11 s against
  0.25 s) and slower on 5 (lesmis 3.1 ms against 29 µs, polblogs, wn18rr,
  cit-hepth and yago3-10 by 1.2–3.1×).
- *Triangles.* igraph, on one thread, was 1.4–13× faster on every graph
  except the road networks and wn18rr: facebook 12 ms against 154 ms, email-enron 23 ms against
  185 ms, and hetionet 0.49 s against 4.17 s. GDS was also faster on five
  of them. `warren-core/src/compute/triangles.rs` intersects every
  neighbour's row, which is O(Σ d²) on skewed degrees, where igraph and GDS
  orient edges by degree first.
  *Since fixed* (M11): `triangles` now orients edges by degree and finds
  each triangle once (DESIGN.md §6.7, §12). Re-timed against igraph alone,
  not in the tables here: facebook 7.0 ms and hetionet 0.116 s, faster than
  igraph on every graph but lesmis and polblogs.
- *Leiden.* igraph was 1.9–180× faster everywhere (fb15k-237 0.14 s
  against 4.42 s, yago3-10 0.79 s against 4.19 s), and GDS was faster on 7 of 11
  datasets (roadnet-tx 2.5 s against 8.1 s). pg_warren's Leiden runs a
  level at a time through `warren.contract`, persisting each quotient graph
  as a version in the catalog. igraph and GDS keep every level in memory.
  *Since fixed* (0.8.0): `leiden` is native, contracting each level from
  the one below and keeping the levels in memory, for the same partitions
  (DESIGN.md §6.7, *Leiden's levels*). Re-timed against igraph alone
  ("Leiden in 0.8.0", below): 1.3–21× faster than main before the change,
  and within 0.76–1.63× of igraph on the medium tier (faster on
  ss-gleich-usroads-48, dblp, yago3-10 and roadnet-tx).
- *Louvain and Leiden on graphs with relations have slowed down since the
  timed run.* `suite.py time` at 1ffdcd9 took 0.78 s and 1.94 s on
  fb15k-237; this run at ae51d77 took 1.32 s and 4.42 s. yago3-10 went from
  1.29 s and 3.29 s to 1.39 s and 4.19 s, and hetionet from 1.86 s and
  3.32 s to 2.25 s and 5.49 s. The graphs without relations are unchanged
  or faster (dblp, email-enron, roadnet-tx). Two changes landed between
  those commits: sparse per-relation offsets, and a modularity that weighs
  a pair 1 across relations (0.7.0). The quotients keep per-relation CSRs.
  Re-timed with one harness, the slowdown was real: 0.7.0's counted-once
  `contract` looked each stored edge up in every earlier relation's row.
  It is fixed after this run (DESIGN.md §12, "A community regression in
  0.7.0"); these tables are ae51d77's.
- *Traversal and components against igraph.* igraph was 1.2–60× faster
  for BFS reach, depths and components on most datasets. The exceptions
  are the two BFS wins above, and components on cit-hepth and fb15k-237.
  pg_warren's traversals also return their rows to SQL; the timed query
  counts them.
- *Not timed: weighted single-source paths.* pg_warren's weighted
  single-source shortest path is `sssp_vector` (Bellman-Ford with
  `min_plus` products, persisting a vector), which this run does not time,
  so weighted lesmis is compared on pairs (`dijkstra`) only.

**Neo4j.** Plain Cypher on Community runs each query on one thread. Once
warm, it answers reach and pair queries in milliseconds to a few hundred.
It cannot express PageRank, components, Louvain, Leiden or betweenness. Its
`shortestPath` to every node and its triangle pattern take tens of seconds
to minutes on the medium tier. GDS answers everything, but every call costs
at least about 8 ms. Its Louvain was the slowest of the three engines on
every dataset but roadnet-tx (3–16 s on the medium tier). Its Leiden was
the fastest of the three on dblp, yago3-10, hetionet and roadnet-tx.

## Loading (median seconds of each phase; ms and µs where marked)

pg_warren's `build` is the median of three `refresh` runs. Neo4j's phases are shared by Cypher and GDS: both read the same store.

| Dataset | Engine | Phases (median seconds) |
| --- | --- | --- |
| lesmis | pg_warren | ingest 9.37 ms, build 2.29 ms |
| lesmis | igraph | ingest 253 µs, build 183 µs |
| lesmis | neo4j-cypher | import 3.65, start 7.90, index 1.52 |
| lesmis | neo4j-gds | import 3.65, start 7.90, index 1.52, project 1.04 |
| polblogs | pg_warren | ingest 0.024, build 9.13 ms |
| polblogs | igraph | ingest 5.82 ms, build 8.34 ms |
| polblogs | neo4j-cypher | import 4.18, start 8.30, index 1.56 |
| polblogs | neo4j-gds | import 4.18, start 8.30, index 1.56, project 1.02, project undirected 0.062 |
| facebook | pg_warren | ingest 0.045, build 0.026 |
| facebook | igraph | ingest 0.026, build 0.015 |
| facebook | neo4j-cypher | import 4.00, start 7.26, index 1.43 |
| facebook | neo4j-gds | import 4.00, start 7.26, index 1.43, project 1.10 |
| wn18rr | pg_warren | ingest 0.142, build 0.064 |
| wn18rr | igraph | ingest 0.084, build 0.051 |
| wn18rr | neo4j-cypher | import 4.48, start 7.27, index 1.39 |
| wn18rr | neo4j-gds | import 4.48, start 7.27, index 1.39, project 1.09, project undirected 0.155 |
| cit-hepth | pg_warren | ingest 0.145, build 0.086 |
| cit-hepth | igraph | ingest 0.140, build 0.174 |
| cit-hepth | neo4j-cypher | import 4.65, start 7.16, index 1.31 |
| cit-hepth | neo4j-gds | import 4.65, start 7.16, index 1.31, project 1.05, project undirected 0.114 |
| email-enron | pg_warren | ingest 0.149, build 0.099 |
| email-enron | igraph | ingest 0.145, build 0.075 |
| email-enron | neo4j-cypher | import 5.06, start 8.00, index 1.46 |
| email-enron | neo4j-gds | import 5.06, start 8.00, index 1.46, project 1.26 |
| fb15k-237 | pg_warren | ingest 0.288, build 0.183 |
| fb15k-237 | igraph | ingest 0.293, build 0.177 |
| fb15k-237 | neo4j-cypher | import 5.54, start 7.49, index 1.43 |
| fb15k-237 | neo4j-gds | import 5.54, start 7.49, index 1.43, project 1.12, project undirected 0.164 |
| ss-gleich-usroads-48 | pg_warren | ingest 0.159, build 0.078 |
| ss-gleich-usroads-48 | igraph | ingest 0.109, build 0.038 |
| ss-gleich-usroads-48 | neo4j-cypher | import 5.08, start 8.00, index 1.64 |
| ss-gleich-usroads-48 | neo4j-gds | import 5.08, start 8.00, index 1.64, project 1.20 |
| dblp | pg_warren | ingest 0.658, build 0.415 |
| dblp | igraph | ingest 0.751, build 0.398 |
| dblp | neo4j-cypher | import 6.64, start 7.89, index 1.64 |
| dblp | neo4j-gds | import 6.64, start 7.89, index 1.64, project 1.41 |
| yago3-10 | pg_warren | ingest 0.800, build 0.581 |
| yago3-10 | igraph | ingest 1.35, build 0.723 |
| yago3-10 | neo4j-cypher | import 6.68, start 7.16, index 1.43 |
| yago3-10 | neo4j-gds | import 6.68, start 7.16, index 1.43, project 1.33, project undirected 0.212 |
| hetionet | pg_warren | ingest 1.06, build 0.977 |
| hetionet | igraph | ingest 1.58, build 1.78 |
| hetionet | neo4j-cypher | import 8.46, start 7.26, index 1.42 |
| hetionet | neo4j-gds | import 8.46, start 7.26, index 1.42, project 1.35, project undirected 0.299 |
| roadnet-tx | pg_warren | ingest 2.23, build 1.24 |
| roadnet-tx | igraph | ingest 2.56, build 0.885 |
| roadnet-tx | neo4j-cypher | import 8.04, start 7.59, index 2.56 |
| roadnet-tx | neo4j-gds | import 8.04, start 7.59, index 2.56, project 1.79 |

## Workloads (cold / warm median, seconds; ms and µs where marked)

"—" means the engine cannot run the workload (reasons below). "timeout"
means the run passed 300 s. "error" and "below bound" are explained under
Findings. Betweenness ran only where N·M ≤ 5·10⁸.

| Dataset | Workload | pg_warren | igraph | neo4j-cypher | neo4j-gds |
| --- | --- | --- | --- | --- | --- |
| lesmis | degree | 2.23 ms / 501 µs | 9 µs / 2 µs | 1.90 / 0.013 | 0.095 / 0.010 |
| lesmis | bfs depth 6 | 892 µs / 217 µs | 11 µs / 4 µs | 1.82 / 7.89 ms | 0.440 / 0.016 |
| lesmis | bfs depths | 668 µs / 187 µs | 16 µs / 11 µs | 1.98 / 0.013 | 0.374 / 0.018 |
| lesmis | shortest paths | 1.66 ms / 855 µs | 465 µs / 330 µs | — | 1.05 / 0.211 |
| lesmis | pagerank 20 iterations | 0.035 / 0.029 | 70 µs / 28 µs | — | 0.222 / 0.063 |
| lesmis | components | 810 µs / 162 µs | 33 µs / 3 µs | — | 0.138 / 0.012 |
| lesmis | triangles | 871 µs / 251 µs | 55 µs / 23 µs | 2.16 / 0.013 | 0.131 / 0.017 |
| lesmis | louvain | 0.012 / 8.83 ms | 185 µs / 97 µs | — | 0.847 / 0.957 |
| lesmis | leiden | 0.032 / 0.029 | 198 µs / 161 µs | — | 0.353 / 0.153 |
| lesmis | betweenness | 1.19 ms / 578 µs | 587 µs / 538 µs | — | 0.136 / 0.015 |
| polblogs | degree | 2.30 ms / 935 µs | 218 µs / 6 µs | 1.94 / 0.015 | 0.099 / 9.67 ms |
| polblogs | bfs depth 6 | 1.38 ms / 538 µs | 108 µs / 45 µs | 1.98 / 0.011 | 0.487 / 0.014 |
| polblogs | bfs depths | 1.20 ms / 513 µs | 241 µs / 166 µs | 2.09 / 0.032 | 0.392 / 0.016 |
| polblogs | shortest paths | 1.63 ms / 802 µs | 3.41 ms / 3.10 ms | 2.10 / 0.018 | 0.703 / 0.075 |
| polblogs | pagerank 20 iterations | 0.041 / 0.035 | 2.04 ms / 1.63 ms | — | 0.244 / 0.059 |
| polblogs | components | 1.51 ms / 572 µs | 364 µs / 282 µs | — | 0.119 / 0.015 |
| polblogs | triangles | 0.021 / 0.021 | 1.89 ms / 1.71 ms | 3.67 / 1.24 | 0.148 / 0.028 |
| polblogs | louvain | 0.026 / 0.021 | 7.38 ms / 6.47 ms | — | 5.42 / 3.60 |
| polblogs | leiden | 0.064 / 0.059 | 5.85 ms / 5.90 ms | — | 0.480 / 0.194 |
| polblogs | betweenness | 0.101 / 0.093 | 0.082 / 0.080 | — | 0.353 / 0.071 |
| facebook | degree | 4.40 ms / 1.80 ms | 38 µs / 17 µs | 1.83 / 0.037 | 0.083 / 7.96 ms |
| facebook | bfs depth 6 | 3.07 ms / 1.74 ms | 372 µs / 328 µs | 1.85 / 0.019 | 0.427 / 0.016 |
| facebook | bfs depths | 3.13 ms / 1.70 ms | 930 µs / 867 µs | 2.17 / 0.224 | 0.374 / 0.027 |
| facebook | shortest paths | 3.96 ms / 2.37 ms | 0.019 / 0.019 | 1.95 / 0.025 | 0.682 / 0.094 |
| facebook | pagerank 20 iterations | 0.049 / 0.040 | 0.010 / 9.86 ms | — | 0.390 / 0.075 |
| facebook | components | 3.40 ms / 1.59 ms | 677 µs / 93 µs | — | 0.126 / 0.011 |
| facebook | triangles | 0.156 / 0.154 | 0.012 / 0.012 | 11.90 / 9.72 | 0.284 / 0.110 |
| facebook | louvain | 0.040 / 0.036 | 0.027 / 0.025 | — | 12.03 / 10.15 |
| facebook | leiden | 0.085 / 0.080 | 0.018 / 0.018 | — | 0.502 / 0.207 |
| facebook | betweenness | 1.13 / 1.15 | 1.67 / 1.67 | — | 1.34 / 1.07 |
| wn18rr | degree | 0.014 / 3.48 ms | 353 µs / 141 µs | 1.90 / 0.061 | 0.105 / 8.15 ms |
| wn18rr | bfs depth 6 | 0.011 / 2.56 ms | 693 µs / 248 µs | 1.75 / 0.016 | 0.460 / 0.015 |
| wn18rr | bfs depths | 0.013 / 5.05 ms | 3.92 ms / 3.82 ms | 5.01 / 3.05 | 0.417 / 0.034 |
| wn18rr | shortest paths | 8.93 ms / 1.32 ms | 0.080 / 0.079 | 2.16 / 0.023 | 0.806 / 0.171 |
| wn18rr | pagerank 20 iterations | 0.101 / 0.089 | 0.012 / 0.012 | — | 0.451 / 0.122 |
| wn18rr | components | 0.020 / 8.88 ms | 5.56 ms / 4.83 ms | — | 0.137 / 0.014 |
| wn18rr | triangles | 0.015 / 6.13 ms | 0.012 / 0.011 | 3.21 / 0.846 | 0.188 / 0.024 |
| wn18rr | louvain | 0.230 / 0.216 | 0.160 / 0.159 | — | 8.89 / 4.53 |
| wn18rr | leiden | 0.523 / 0.513 | 0.093 / 0.092 | — | 1.48 / 0.272 |
| cit-hepth | degree | 9.30 ms / 4.36 ms | 351 µs / 100 µs | 1.96 / 0.071 | 0.097 / 7.51 ms |
| cit-hepth | bfs depth 6 | 7.60 ms / 2.74 ms | 1.38 ms / 612 µs | 1.70 / 0.037 | 0.495 / 0.022 |
| cit-hepth | bfs depths | 8.37 ms / 4.75 ms | 4.00 ms / 3.93 ms | 16.83 / 15.14 | 0.460 / 0.042 |
| cit-hepth | shortest paths | 8.55 ms / 5.22 ms | 0.076 / 0.074 | 1.87 / 0.052 | 0.854 / 0.157 |
| cit-hepth | pagerank 20 iterations | 0.083 / 0.077 | 0.013 / 0.011 | — | 0.426 / 0.156 |
| cit-hepth | components | 0.012 / 6.47 ms | 9.92 ms / 6.91 ms | — | 0.158 / 0.021 |
| cit-hepth | triangles | 0.145 / 0.122 | 0.042 / 0.040 | 38.77 / 37.86 | 0.352 / 0.136 |
| cit-hepth | louvain | 0.237 / 0.202 | 0.454 / 0.453 | — | 8.00 / 7.97 |
| cit-hepth | leiden | 0.445 / 0.441 | 0.203 / 0.198 | — | 1.29 / 0.350 |
| email-enron | degree | 0.011 / 5.06 ms | 233 µs / 153 µs | 1.97 / 0.106 | 0.109 / 0.011 |
| email-enron | bfs depth 6 | 0.015 / 7.92 ms | 2.97 ms / 2.21 ms | 1.99 / 0.076 | 0.572 / 0.040 |
| email-enron | bfs depths | 0.012 / 8.22 ms | 5.24 ms / 5.08 ms | 6.72 / 4.53 | 0.549 / 0.057 |
| email-enron | shortest paths | 7.18 ms / 2.59 ms | 0.115 / 0.113 | 1.98 / 0.024 | 0.962 / 0.260 |
| email-enron | pagerank 20 iterations | 0.100 / 0.096 | 0.037 / 0.036 | — | 0.449 / 0.159 |
| email-enron | components | 0.013 / 8.02 ms | 4.17 ms / 3.21 ms | — | 0.176 / 0.020 |
| email-enron | triangles | 0.205 / 0.185 | 0.024 / 0.023 | 39.80 / 37.53 | 0.298 / 0.080 |
| email-enron | louvain | 0.154 / 0.146 | 0.207 / 0.205 | — | 6.83 / 7.07 |
| email-enron | leiden | 0.378 / 0.357 | 0.125 / 0.122 | — | below bound |
| fb15k-237 | degree | 0.066 / 3.34 ms | 286 µs / 52 µs | 1.88 / 0.079 | 0.084 / 7.58 ms |
| fb15k-237 | bfs depth 6 | 0.068 / 4.83 ms | 2.33 ms / 1.45 ms | 1.82 / 0.048 | 0.451 / 0.025 |
| fb15k-237 | bfs depths | 0.067 / 5.23 ms | 3.05 ms / 2.80 ms | 3.56 / 1.68 | 0.413 / 0.023 |
| fb15k-237 | shortest paths | 0.066 / 3.58 ms | 0.054 / 0.054 | 1.89 / 0.027 | 0.740 / 0.141 |
| fb15k-237 | pagerank 20 iterations | 0.135 / 0.069 | 0.015 / 0.014 | — | 0.405 / 0.129 |
| fb15k-237 | components | 0.066 / 4.67 ms | 7.38 ms / 6.18 ms | — | 0.117 / 0.017 |
| fb15k-237 | triangles | 0.196 / 0.136 | 0.020 / 0.018 | 159 / 166 | 0.276 / 0.075 |
| fb15k-237 | louvain | 1.39 / 1.32 | 0.257 / 0.257 | — | 8.42 / 5.74 |
| fb15k-237 | leiden | 4.51 / 4.42 | 0.150 / 0.144 | — | 0.968 / 0.459 |
| ss-gleich-usroads-48 | degree | 0.021 / 9.75 ms | 747 µs / 628 µs | 2.25 / 0.101 | 0.092 / 8.43 ms |
| ss-gleich-usroads-48 | bfs depth 6 | 8.70 ms / 323 µs | 202 µs / 21 µs | 1.87 / 7.78 ms | 0.445 / 0.015 |
| ss-gleich-usroads-48 | bfs depths | 0.038 / 0.033 | 0.017 / 0.016 | timeout | 0.859 / 0.236 |
| ss-gleich-usroads-48 | shortest paths | 0.139 / 0.128 | 0.278 / 0.275 | 2.26 / 0.329 | 1.30 / 0.615 |
| ss-gleich-usroads-48 | pagerank 20 iterations | 0.166 / 0.161 | 0.087 / 0.077 | — | 0.528 / 0.193 |
| ss-gleich-usroads-48 | components | 0.036 / 0.023 | 0.015 / 2.90 ms | — | 0.214 / 0.023 |
| ss-gleich-usroads-48 | triangles | 0.022 / 0.011 | 0.031 / 0.030 | 2.66 / 0.377 | 0.191 / 0.036 |
| ss-gleich-usroads-48 | louvain | 0.303 / 0.281 | 0.745 / 0.758 | — | 4.26 / 3.15 |
| ss-gleich-usroads-48 | leiden | 0.706 / 0.661 | 0.292 / 0.289 | — | 1.73 / 0.741 |
| dblp | degree | 0.065 / 0.032 | 2.40 ms / 1.72 ms | 2.38 / 0.436 | 0.096 / 8.55 ms |
| dblp | bfs depth 6 | 0.118 / 0.071 | 0.043 / 0.033 | 2.21 / 0.331 | 0.569 / 0.069 |
| dblp | bfs depths | 0.127 / 0.078 | 0.065 / 0.058 | 37.18 / 34.70 | 0.647 / 0.075 |
| dblp | shortest paths | 0.035 / 6.03 ms | 1.24 / 1.04 | 1.86 / 0.026 | 2.88 / 2.22 |
| dblp | pagerank 20 iterations | 0.386 / 0.306 | 0.284 / 0.299 | — | 0.996 / 0.443 |
| dblp | components | 0.118 / 0.071 | 0.049 / 7.48 ms | — | 0.290 / 0.034 |
| dblp | triangles | 0.276 / 0.220 | 0.165 / 0.159 | 17.52 / 14.93 | 0.565 / 0.272 |
| dblp | louvain | 1.18 / 1.16 | 5.19 / 5.49 | — | 18.78 / 12.73 |
| dblp | leiden | 2.79 / 2.78 | 1.34 / 1.33 | — | 2.35 / 0.932 |
| yago3-10 | degree | 0.052 / 0.014 | 912 µs / 485 µs | 2.29 / 0.308 | 0.081 / 7.04 ms |
| yago3-10 | bfs depth 6 | 0.054 / 5.20 ms | 1.91 ms / 801 µs | 1.94 / 0.030 | 0.424 / 0.017 |
| yago3-10 | bfs depths | 0.040 / 4.92 ms | 0.013 / 0.013 | 70.07 / 66.77 | 0.367 / 0.027 |
| yago3-10 | shortest paths | 0.033 / 1.19 ms | 0.260 / 0.260 | 1.87 / 0.019 | 0.741 / 0.135 |
| yago3-10 | pagerank 20 iterations | 0.200 / 0.169 | 0.040 / 0.035 | — | 0.485 / 0.146 |
| yago3-10 | components | 0.067 / 0.030 | 0.026 / 0.020 | — | 0.141 / 0.030 |
| yago3-10 | triangles | 0.228 / 0.172 | 0.069 / 0.059 | timeout | 0.358 / 0.133 |
| yago3-10 | louvain | 1.43 / 1.39 | 1.68 / 1.71 | — | 7.30 / 6.97 |
| yago3-10 | leiden | 4.19 / 4.19 | 0.781 / 0.787 | — | 1.93 / 0.712 |
| hetionet | degree | 0.061 / 0.013 | 629 µs / 180 µs | 2.60 / 0.592 | 0.079 / 7.37 ms |
| hetionet | bfs depth 6 | 0.064 / 0.014 | 4.26 ms / 2.89 ms | 2.10 / 0.241 | 0.644 / 0.061 |
| hetionet | bfs depths | 0.063 / 0.014 | 8.96 ms / 7.75 ms | 91.65 / 87.30 | 0.445 / 0.042 |
| hetionet | shortest paths | 0.052 / 2.71 ms | 0.165 / 0.162 | 1.85 / 0.023 | 0.866 / 0.241 |
| hetionet | pagerank 20 iterations | 0.167 / 0.103 | 0.060 / 0.053 | — | 0.589 / 0.375 |
| hetionet | components | 0.066 / 0.017 | 0.043 / 1.01 ms | — | 0.184 / 0.027 |
| hetionet | triangles | 4.25 / 4.17 | 0.487 / 0.494 | timeout | 3.10 / 2.88 |
| hetionet | louvain | 2.34 / 2.25 | 2.36 / 2.39 | — | 14.46 / 15.62 |
| hetionet | leiden | 5.66 / 5.49 | 1.40 / 1.43 | — | 1.41 / 0.761 |
| roadnet-tx | degree | 0.187 / 0.090 | 0.014 / 7.40 ms | 3.47 / 1.25 | 0.086 / 8.10 ms |
| roadnet-tx | bfs depth 6 | 0.096 / 380 µs | 1.14 ms / 596 µs | 1.82 / 9.56 ms | 0.433 / 0.016 |
| roadnet-tx | bfs depths | 0.476 / 0.382 | 0.179 / 0.169 | timeout | 2.86 / 2.00 |
| roadnet-tx | shortest paths | 1.59 / 1.46 | 3.06 / 2.98 | 7.94 / 5.87 | error |
| roadnet-tx | pagerank 20 iterations | 1.09 / 0.998 | 0.868 / 0.876 | — | 1.23 / 0.617 |
| roadnet-tx | components | 0.400 / 0.295 | 0.157 / 0.131 | — | 0.325 / 0.056 |
| roadnet-tx | triangles | 0.255 / 0.145 | 0.350 / 0.334 | 8.55 / 6.26 | 0.396 / 0.304 |
| roadnet-tx | louvain | 3.94 / 3.59 | 16.72 / 15.80 | — | 6.86 / 6.07 |
| roadnet-tx | leiden | 8.13 / 8.10 | 4.34 / 4.29 | — | 4.22 / 2.50 |

## Not run by Cypher

- shortest paths (weighted lesmis only): Cypher's shortest paths count hops (shortestPath, SHORTEST); a weighted one needs GDS or APOC
- pagerank 20 iterations: iterative over the whole graph's state: no Cypher query expresses it (GDS or APOC do)
- components: a fixed point of label propagation, or a traversal per component: not one Cypher query
- louvain: a heuristic optimisation over the whole graph: not expressible in Cypher
- leiden: a heuristic optimisation over the whole graph: not expressible in Cypher
- betweenness: needs every pair's shortest paths counted (Brandes): not a practical Cypher query

## Leiden in 0.8.0 (re-timed against igraph alone)

`leiden-0.8.0/` holds one file per dataset for each side. Both sides
are release builds (`cargo pgrx package`, PostgreSQL 16.15, the settings
above, `warren.threads = 4`). `4592b0e` is main before the change: 0.7.0
with the community regression fixed, so faster than the ae51d77 column
above. `99aeb74` is native `leiden` with its levels in memory; rebased onto main's later commits (examples and the website, none to the extension) it is f618847.

The command line, per dataset, alternated the two builds so they shared
the machine's load:
`suite.py head-to-head <dataset> --engine pg_warren [--engine igraph] --workload leiden --threads 4 --extension-profile release`.
The machine was busier than in the M11 run, with a load average of 2.2–7.0
at the start of each dataset from other agents' builds and tests. Read
the ratios rather than the absolute times. Both builds found the same
partition on every dataset: the same number of communities, and the same
modularity to the last bit.

| Dataset | before (4592b0e) | after (99aeb74) | speed-up | igraph | after / igraph |
| --- | --- | --- | --- | --- | --- |
| lesmis | 0.032 / 0.025 | 2.28 ms / 1.17 ms | 21× | 282 µs / 177 µs | 6.6 |
| polblogs | 0.080 / 0.081 | 0.014 / 0.012 | 6.7× | 5.84 ms / 5.74 ms | 2.1 |
| facebook | 0.096 / 0.082 | 0.037 / 0.030 | 2.7× | 0.019 / 0.020 | 1.5 |
| wn18rr | 0.421 / 0.401 | 0.111 / 0.096 | 4.2× | 0.096 / 0.095 | 1.0 |
| cit-hepth | 0.483 / 0.451 | 0.315 / 0.299 | 1.5× | 0.206 / 0.204 | 1.5 |
| email-enron | 0.420 / 0.340 | 0.184 / 0.169 | 2.0× | 0.121 / 0.120 | 1.4 |
| fb15k-237 | 1.45 / 1.44 | 0.289 / 0.206 | 7.0× | 0.144 / 0.143 | 1.4 |
| ss-gleich-usroads-48 | 0.682 / 0.645 | 0.263 / 0.238 | 2.7× | 0.294 / 0.283 | 0.84 |
| dblp | 2.81 / 2.70 | 1.33 / 1.26 | 2.1× | 1.33 / 1.30 | 0.97 |
| yago3-10 | 2.35 / 2.26 | 0.819 / 0.731 | 3.1× | 0.930 / 0.933 | 0.78 |
| hetionet | 3.07 / 2.95 | 2.27 / 2.22 | 1.3× | 1.35 / 1.36 | 1.6 |
| roadnet-tx | 7.73 / 7.91 | 3.27 / 3.80 | 2.1× | 5.32 / 4.99 | 0.76 |

Each cell is cold / warm median in seconds, with ms and µs where marked.
The speed-up and ratio columns use the warm medians.

Every level now costs the level below it, not the whole graph. What is
left is level 1: the undirected view, the colouring and the sweeps over
the whole graph, twice by default. On hetionet and cit-hepth that is most
of the time.
GDS was not re-run. Against its warm times in the table above, the new
`leiden` is faster on seven of the eleven datasets GDS ran. It is slower
on dblp, yago3-10, hetionet and roadnet-tx.

## PageRank in 0.8.0 (re-timed against igraph alone)

`pagerank-0.8.0/6c918eb/` holds the run: `round-N/<engine>/<dataset>.json`,
three rounds, one file per dataset for each engine. pg_warren is a release
build of 6c918eb (main with the native `pagerank` and `leiden`;
`cargo pgrx package`, PostgreSQL 16.15, the settings above,
`warren.threads = 4`; `pg_warren.so` sha256
`a59481d01fe533b8bb9f81663f8b40dbcfb2279efbbb62accb62bb6a8f103bd3`). igraph
is python-igraph 1.0.0 (PRPACK, one thread), as above.

Each round ran, per dataset, the two engines one after the other, pg_warren
first in rounds 1 and 3 and igraph first in round 2:
`suite.py head-to-head <dataset> --engine <engine> --workload pagerank --threads 4 --extension-profile release`,
with the defaults `--repeat 5 --budget 120`. Nothing else ran on the
machine: the load average at the start of each run was 0.1–1.4, most of it
the runs themselves. Every answer of every round matched the reference
(within 10⁻⁹, each engine under its own convention, as above).

| Dataset | M11 (ae51d77) | 0.8.0 cold | 0.8.0 warm | igraph warm | 0.8.0 / igraph | speed-up on M11 |
| --- | --- | --- | --- | --- | --- | --- |
| lesmis | 0.029 | 3.97 ms | 3.13 ms (2.75–3.60) | 29 µs (28–33 µs) | 108 | 9.3× |
| polblogs | 0.035 | 5.84 ms | 4.31 ms (3.76–4.35) | 1.41 ms (1.36–1.59) | 3.1 | 8.1× |
| facebook | 0.040 | 0.011 | 6.92 ms (6.77–8.07) | 0.010 (0.010–0.013) | 0.67 | 5.8× |
| wn18rr | 0.089 | 0.031 | 0.021 (0.020–0.023) | 0.012 (0.012–0.015) | 1.8 | 4.2× |
| cit-hepth | 0.077 | 0.023 | 0.018 (0.018–0.018) | 0.011 (0.011–0.011) | 1.6 | 4.3× |
| email-enron | 0.096 | 0.027 | 0.021 (0.020–0.021) | 0.032 (0.027–0.037) | 0.66 | 4.6× |
| fb15k-237 | 0.069 | 0.078 | 0.011 (0.010–0.012) | 0.012 (0.012–0.014) | 0.90 | 6.4× |
| ss-gleich-usroads-48 | 0.161 | 0.053 | 0.037 (0.036–0.040) | 0.074 (0.073–0.081) | 0.50 | 4.4× |
| dblp | 0.306 | 0.140 | 0.105 (0.101–0.112) | 0.251 (0.239–0.261) | 0.42 | 2.9× |
| yago3-10 | 0.169 | 0.081 | 0.043 (0.039–0.044) | 0.036 (0.035–0.038) | 1.2 | 3.9× |
| hetionet | 0.103 | 0.081 | 0.031 (0.031–0.032) | 0.052 (0.049–0.057) | 0.60 | 3.3× |
| roadnet-tx | 0.998 | 0.363 | 0.224 (0.224–0.225) | 0.810 (0.806–0.819) | 0.28 | 4.4× |

Seconds, with ms and µs where marked. A warm cell is the median of the
three rounds' warm medians, with the lowest and highest of them in
brackets (each round's median is of 5 warm runs); the cold cell is the
median of the three cold runs. M11 is pg_warren's warm median in the main
table above (0.7.0's PL/pgSQL loop). The ratio and speed-up columns use
the warm medians.

pg_warren is faster than igraph on 7 of the 12 datasets, by up to 3.6×
(roadnet-tx), and the brackets do not overlap on any of them. It is
slower on 5:

- *lesmis* (77 nodes) and *polblogs* (1,490 nodes): the call's fixed cost.
  An SQL statement, the cache lookup and persisting the output vector to
  the catalog take about 3 ms however small the graph, where igraph
  answers lesmis in 29 µs. An earlier before-and-after run of the kernel
  (DESIGN.md §13) measured 1.6 ms on lesmis; this run's three rounds took
  2.75–3.60 ms. The difference was not investigated.
- *wn18rr*, *cit-hepth* and *yago3-10*, by 1.2–1.8×. These are not
  fixed cost (a few ms of 18–43 ms) and were not profiled. pg_warren runs
  20 iterations; PRPACK solves for the fixed point, so the two do
  different amounts of work, and how much differs by graph.

The ratios move with the graph more than with the run. An engine's
highest round median is within 1.2× of its lowest on 20 of the 24
engine-dataset pairs; the others are pg_warren on lesmis (1.31×) and
igraph on wn18rr (1.24×), facebook (1.35×) and email-enron (1.36×).

GDS was not re-run. Against its warm times in the main table above, which
are from the M11 run, the new `pagerank` is faster on all 12 datasets: on
roadnet-tx 0.22 s against 0.62 s, on yago3-10 0.043 s against 0.146 s.

## pgGraph 1.2.1 against pg_warren 0.8.0 and igraph (2026-10-29)

[pgGraph](https://github.com/Evokoa/pgGraph) (Apache-2.0, Rust and pgrx,
PostgreSQL 14–18) is the closest project to pg_warren: it also builds a CSR
from your tables and answers graph queries in SQL. This run times it on the
same 12 datasets, in the same scratch cluster, beside pg_warren 0.8.0 and
igraph. `pggraph-1.2.1/run/` holds one file per dataset with every workload
for pg_warren and igraph and what pgGraph runs. `pggraph-1.2.1/paths/`
re-times shortest paths for all three, once igraph's pairs went through a
fairer call (below). Neo4j and GDS were not re-run: their figures above are
from the M11 run, against 0.7.0.

### Provenance

| | |
|---|---|
| pg_warren | 0.8.0, release build of 8dd426c, the release commit that `v0.8.0` tags (the tag was not yet on origin when this ran); `cargo pgrx package` with cargo-pgrx 0.19.3 and Rust 1.97.0 into a private PostgreSQL 16.15 tree; `pg_warren.so` sha256 `b720e538f278899d8846e017d0f2667f71a0163d3552bf8ce48c3ff3af5bfde0`; `warren.threads = 4` |
| pgGraph | 1.2.1, the latest release (tag `v1.2.1`, commit `1bbe30149100b49fc4a1494bd3afbeba82070092`), built in release mode (its profile: opt-level 3, fat LTO, one codegen unit) with Rust 1.96.0 and cargo-pgrx 0.19.1, as it pins them; `cargo pgrx package --no-default-features --features pg16`, installed into the same private tree; `graph.so` sha256 `7cb8900debd93f77f8df88fc52bddf21581723432400200d2f110a882e5535ba`. `head_to_head/pins.json` and `bench/corpus/README.md` have the recipe |
| pgGraph's settings | Its recommended starting configuration: `shared_preload_libraries = 'graph'`, `graph.memory_limit_mb = 4096`, `persist_on_build` and `auto_load` on, `csr_readonly`; `graph.sync_mode = 'manual'`, its advice for a static graph; `graph.maintenance_memory_mb = 4096`. Each session raises the circuit breakers so a whole-graph answer is not cut short: `graph.max_nodes` and `graph.max_frontier` 10,000,000 (their maximum), `graph.query_work_limit` 2,000,000,000 (its maximum), `graph.query_memory_mb` 4096 (the memory limit). Every setting is in each file (`engines.pggraph.describe`); string settings at their defaults read empty there |
| The cluster | PostgreSQL 16.15 with the soak's settings, as in M11 (`shared_buffers = 1GB`, `fsync = off`, `synchronous_commit = off`), plus pgGraph's preload, which pg_warren's backends also load |
| igraph | python-igraph 1.0.0 (its C core 1.0.0), CPython 3.13.16, one thread |
| Harness | `run/`: fe2aed7, `suite.py head-to-head <the 12 datasets> --engine pg_warren --engine pggraph --engine igraph --extension-revision 8dd426c --extension-profile release --timeout 300`, with the defaults `--repeat 5 --budget 120 --threads 4`. The tree reads dirty from dblp on: the igraph change below was edited in during the run, after the process had loaded its modules, so that run's igraph pairs still use `distances`. `paths/`: 794b65a, the same command with `--workload paths` |
| Machine | The M11 VM (4 vCPUs, 15 GiB, no swap, kernel 6.18.44), shared with other agents. One-minute load average at the start of each dataset: 0.5–3.5 for `run/`, 0.2–1.0 for `paths/` (`hardware.load_average`) |

### How pgGraph runs each workload

pgGraph's SQL API (its [api-reference](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/api-reference.mdx))
has traversal, unweighted and weighted shortest paths, connected components,
search, aggregation over traversals and a GQL subset. It has no degree,
PageRank, triangle count, community detection or betweenness, so those
workloads are recorded as unsupported. `graph.gql` cannot count each node's
distinct neighbours either: `MATCH ... OPTIONAL MATCH` is a syntax error in
its subset, and a plain match leaves out nodes with none.

- *Loading.* `nodes` and `edges` are loaded by the same `load.sql` as
  pg_warren's (`ingest`). pgGraph reads a relationship table's primary key
  as each edge's identity. The suite's `edges` has none (a parallel edge
  repeats its `(src, dst)`), so `key` adds a surrogate `bigint` identity
  primary key. `register` is `graph.add_table` and `graph.add_edge`: an
  undirected graph's edges `bidirectional` (stored both ways), a directed
  one's one way, the relation as the edge's dynamic label, the weight as
  its weight. `build` is `graph.build()`, which also writes the persisted
  artifact (the median of three). The sum of the phases is the time to
  first query.
- *Cold and warm.* Cold follows `graph.unload_graph`, so the call loads
  the graph from its artifact (`graph.auto_load`), as pg_warren's cold
  follows `warren.evict`. A query runs on its backend's one thread;
  pgGraph has no thread setting.
- *BFS reach and depths.* `count(*)` over `graph.traverse(start, depth,
  direction := 'out', hydrate := false, max_rows := 2147483647)`, as
  pg_warren's `count(*)` over `warren.bfs`. `traverse` takes a depth bound,
  and it reserves memory and work for every visited node's path to that
  depth. So it has no unbounded search, and a depth bound far beyond the
  answer's fails its own resource check. `bfs depths` passes the start's
  eccentricity from the reference (3–756), the smallest bound that reaches
  every node. That favours pgGraph: it is told the answer's depth.
- *Shortest paths.* `max(step)` of `graph.shortest_path(s, t, max_depth
  := 2147483647, hydrate := false)` for each pair, all in one statement as
  pg_warren's; on lesmis, `max(total_cost)` of
  `graph.weighted_shortest_path`. pgGraph's weights are integers, and
  lesmis's are whole numbers. A depth bound of 20, its default, or of 2³¹−1
  timed the same.
- *Components.* `count(DISTINCT component_id)` over
  `graph.connected_components()`, a union-find over the whole graph (weak
  components).

Parallel edges stay in pgGraph's graph (its "bag semantics": it returns
edge identities), where pg_warren folds them. No workload pgGraph runs can
tell the difference, and none did.

### Correctness

Every pgGraph time counted is of an answer that matched the reference
exactly: reach, depths, path lengths and costs, components as partitions.
So did every pg_warren and igraph answer in both runs. Two pgGraph runs did
not finish, both `bfs depths` on the road networks, where the start's
eccentricity is 537 (ss-gleich-usroads-48) and 756 (roadnet-tx). `traverse`
refused them before running (SQLSTATE 54000, PG007): the paths it reserves
for would need 4.5 GB and 68.6 GB, against a 4 GB limit. Raising the limit
past the recommended configuration was not tried; the VM has 15 GB.

### Loading (median seconds of each phase; ms and µs where marked)

| Dataset | Engine | Phases (median seconds) | First query |
| --- | --- | --- | --- |
| lesmis | pg_warren | ingest 0.011, build 2.60 ms | 0.014 |
| lesmis | pggraph | ingest 0.012, key 7.18 ms, register 5.26 ms, build 0.041 | 0.066 |
| lesmis | igraph | ingest 373 µs, build 261 µs | 634 µs |
| polblogs | pg_warren | ingest 0.026, build 7.42 ms | 0.034 |
| polblogs | pggraph | ingest 0.024, key 0.016, register 5.06 ms, build 0.123 | 0.168 |
| polblogs | igraph | ingest 5.89 ms, build 8.25 ms | 0.014 |
| facebook | pg_warren | ingest 0.050, build 0.028 | 0.078 |
| facebook | pggraph | ingest 0.048, key 0.057, register 5.39 ms, build 0.615 | 0.725 |
| facebook | igraph | ingest 0.028, build 0.017 | 0.044 |
| wn18rr | pg_warren | ingest 0.147, build 0.075 | 0.222 |
| wn18rr | pggraph | ingest 0.147, key 0.072, register 5.94 ms, build 0.697 | 0.921 |
| wn18rr | igraph | ingest 0.094, build 0.054 | 0.148 |
| cit-hepth | pg_warren | ingest 0.163, build 0.116 | 0.280 |
| cit-hepth | pggraph | ingest 0.165, key 0.239, register 4.73 ms, build 2.43 | 2.84 |
| cit-hepth | igraph | ingest 0.161, build 0.211 | 0.372 |
| email-enron | pg_warren | ingest 0.210, build 0.129 | 0.339 |
| email-enron | pggraph | ingest 0.169, key 0.341, register 5.43 ms, build 4.28 | 4.80 |
| email-enron | igraph | ingest 0.154, build 0.082 | 0.236 |
| fb15k-237 | pg_warren | ingest 0.302, build 0.196 | 0.498 |
| fb15k-237 | pggraph | ingest 0.348, key 0.216, register 4.86 ms, build 2.33 | 2.90 |
| fb15k-237 | igraph | ingest 0.292, build 0.165 | 0.457 |
| ss-gleich-usroads-48 | pg_warren | ingest 0.161, build 0.079 | 0.240 |
| ss-gleich-usroads-48 | pggraph | ingest 0.161, key 0.093, register 5.17 ms, build 2.13 | 2.39 |
| ss-gleich-usroads-48 | igraph | ingest 0.107, build 0.037 | 0.143 |
| dblp | pg_warren | ingest 0.663, build 0.445 | 1.11 |
| dblp | pggraph | ingest 0.642, key 0.526, register 4.81 ms, build 14.77 | 15.94 |
| dblp | igraph | ingest 0.848, build 0.394 | 1.24 |
| yago3-10 | pg_warren | ingest 0.968, build 0.691 | 1.66 |
| yago3-10 | pggraph | ingest 0.849, key 0.678, register 4.80 ms, build 10.68 | 12.22 |
| yago3-10 | igraph | ingest 1.53, build 0.904 | 2.44 |
| hetionet | pg_warren | ingest 1.41, build 1.04 | 2.45 |
| hetionet | pggraph | ingest 1.11, key 1.38, register 5.68 ms, build 20.73 | 23.23 |
| hetionet | igraph | ingest 1.72, build 2.30 | 4.02 |
| roadnet-tx | pg_warren | ingest 2.39, build 1.37 | 3.76 |
| roadnet-tx | pggraph | ingest 3.02, key 2.44, register 5.55 ms, build 72.37 | 77.83 |
| roadnet-tx | igraph | ingest 5.06, build 1.12 | 6.18 |

pgGraph's time to first query is 0.07–78 s, against pg_warren's
0.01–3.8 s; nearly all of it is `graph.build()`, which on roadnet-tx took
72 s (the median of three), against `warren.refresh`'s 1.4 s.

### BFS and components (cold / warm median, seconds; ms and µs where marked)

| Dataset | Workload | pg_warren | pggraph | igraph | pggraph / pg_warren |
| --- | --- | --- | --- | --- | --- |
| lesmis | bfs depth 6 | 860 µs / 245 µs | 8.30 ms / 6.21 ms | 12 µs / 4 µs | 25 |
| lesmis | bfs depths | 797 µs / 210 µs | 8.15 ms / 6.11 ms | 16 µs / 11 µs | 29 |
| lesmis | components | 831 µs / 184 µs | 4.87 ms / 2.63 ms | 39 µs / 4 µs | 14 |
| polblogs | bfs depth 6 | 1.37 ms / 574 µs | 0.059 / 0.053 | 99 µs / 43 µs | 92 |
| polblogs | bfs depths | 1.38 ms / 550 µs | 0.059 / 0.054 | 209 µs / 167 µs | 99 |
| polblogs | components | 1.49 ms / 623 µs | 8.77 ms / 3.66 ms | 363 µs / 273 µs | 5.9 |
| facebook | bfs depth 6 | 4.33 ms / 1.81 ms | 0.239 / 0.207 | 450 µs / 340 µs | 114 |
| facebook | bfs depths | 3.58 ms / 1.79 ms | 0.240 / 0.209 | 1.00 ms / 953 µs | 117 |
| facebook | components | 3.72 ms / 1.69 ms | 0.042 / 7.08 ms | 858 µs / 92 µs | 4.2 |
| wn18rr | bfs depth 6 | 0.014 / 2.47 ms | 0.649 / 0.635 | 1.06 ms / 261 µs | 257 |
| wn18rr | bfs depths | 0.014 / 4.97 ms | 1.93 / 1.89 | 4.33 ms / 4.07 ms | 381 |
| wn18rr | components | 0.018 / 9.14 ms | 0.040 / 0.015 | 6.19 ms / 5.34 ms | 1.66 |
| cit-hepth | bfs depth 6 | 7.81 ms / 2.64 ms | 0.786 / 0.708 | 1.45 ms / 589 µs | 268 |
| cit-hepth | bfs depths | 8.39 ms / 4.63 ms | 1.68 / 1.93 | 4.12 ms / 3.88 ms | 418 |
| cit-hepth | components | 0.011 / 6.89 ms | 0.119 / 0.016 | 0.011 / 7.54 ms | 2.3 |
| email-enron | bfs depth 6 | 0.014 / 7.75 ms | 2.28 / 2.26 | 3.29 ms / 2.13 ms | 291 |
| email-enron | bfs depths | 0.020 / 7.89 ms | 2.33 / 2.06 | 5.45 ms / 4.98 ms | 261 |
| email-enron | components | 0.012 / 7.61 ms | 0.215 / 0.025 | 4.14 ms / 2.85 ms | 3.3 |
| fb15k-237 | bfs depth 6 | 0.073 / 4.78 ms | 0.880 / 0.727 | 2.46 ms / 1.14 ms | 152 |
| fb15k-237 | bfs depths | 0.067 / 4.90 ms | 0.825 / 0.727 | 2.58 ms / 2.38 ms | 148 |
| fb15k-237 | components | 0.073 / 4.98 ms | 0.113 / 0.012 | 8.45 ms / 6.18 ms | 2.4 |
| ss-gleich-usroads-48 | bfs depth 6 | 8.69 ms / 263 µs | 0.065 / 8.43 ms | 147 µs / 20 µs | 32 |
| ss-gleich-usroads-48 | bfs depths | 0.041 / 0.035 | error (PG007) | 0.016 / 0.015 |  |
| ss-gleich-usroads-48 | components | 0.036 / 0.022 | 0.111 / 0.040 | 0.014 / 2.90 ms | 1.80 |
| dblp | bfs depth 6 | 0.147 / 0.071 | 28.44 / 26.19 | 0.043 / 0.034 | 366 |
| dblp | bfs depths | 0.129 / 0.081 | 29.31 / 28.48 | 0.071 / 0.063 | 351 |
| dblp | components | 0.100 / 0.069 | 0.905 / 0.133 | 0.055 / 7.54 ms | 1.93 |
| yago3-10 | bfs depth 6 | 0.049 / 4.45 ms | 0.991 / 0.506 | 2.16 ms / 932 µs | 114 |
| yago3-10 | bfs depths | 0.042 / 4.95 ms | 1.07 / 0.546 | 0.015 / 0.014 | 110 |
| yago3-10 | components | 0.071 / 0.031 | 0.573 / 0.060 | 0.029 / 0.024 | 1.93 |
| hetionet | bfs depth 6 | 0.067 / 0.015 | 2.81 / 1.51 | 4.77 ms / 3.18 ms | 103 |
| hetionet | bfs depths | 0.064 / 0.015 | 2.85 / 1.53 | 0.011 / 0.010 | 103 |
| hetionet | components | 0.070 / 0.017 | 1.35 / 0.053 | 0.049 / 1.01 ms | 3.2 |
| roadnet-tx | bfs depth 6 | 0.106 / 635 µs | 2.65 / 0.015 | 1.21 ms / 412 µs | 24 |
| roadnet-tx | bfs depths | 0.500 / 0.379 | error (PG007) | 0.217 / 0.211 |  |
| roadnet-tx | components | 0.416 / 0.301 | 2.82 / 0.585 | 0.151 / 0.148 | 1.94 |

The last column divides the warm medians.

### Shortest paths, 25 pairs in one call (`paths/`)

| Dataset | pg_warren | pggraph | igraph (`get_shortest_path`) | igraph / pg_warren | pggraph / pg_warren | igraph (`distances`, main run) |
| --- | --- | --- | --- | --- | --- | --- |
| lesmis | 1.68 ms / 899 µs | 0.059 / 0.055 | 740 µs / 329 µs | 0.37 | 62 | 454 µs / 331 µs |
| polblogs | 2.58 ms / 1.28 ms | 0.064 / 0.057 | 792 µs / 521 µs | 0.41 | 45 | 3.20 ms / 3.00 ms |
| facebook | 5.32 ms / 2.50 ms | 0.098 / 0.065 | 2.60 ms / 2.39 ms | 0.95 | 26 | 0.020 / 0.020 |
| wn18rr | 0.011 / 1.21 ms | 0.124 / 0.107 | 0.016 / 0.014 | 12 | 89 | 0.087 / 0.081 |
| cit-hepth | 9.92 ms / 5.33 ms | 0.216 / 0.114 | 0.018 / 0.015 | 2.9 | 21 | 0.077 / 0.075 |
| email-enron | 8.05 ms / 2.67 ms | 0.245 / 0.058 | 0.020 / 0.019 | 7.1 | 22 | 0.109 / 0.105 |
| fb15k-237 | 0.066 / 3.65 ms | 0.189 / 0.095 | 0.011 / 9.79 ms | 2.7 | 26 | 0.046 / 0.047 |
| ss-gleich-usroads-48 | 0.168 / 0.145 | 0.403 / 0.348 | 0.100 / 0.099 | 0.68 | 2.4 | 0.254 / 0.251 |
| dblp | 0.043 / 5.81 ms | 0.836 / 0.073 | 0.263 / 0.256 | 44 | 13 | 1.33 / 1.40 |
| yago3-10 | 0.044 / 1.24 ms | 0.572 / 0.107 | 0.019 / 0.017 | 14 | 86 | 0.296 / 0.287 |
| hetionet | 0.053 / 2.47 ms | 1.36 / 0.149 | 0.031 / 0.025 | 10 | 60 | 0.194 / 0.188 |
| roadnet-tx | 1.69 / 1.45 | 7.04 / 5.05 | 1.27 / 1.29 | 0.89 | 3.5 | 3.79 / 3.76 |

Cold / warm median, seconds; the ratios divide the warm medians. The last
column is igraph's time in `run/`, before the change below.

*igraph's single-pair call.* The M11 run and `run/` call igraph's
`distances(source, target)` for each pair. In igraph's C core 1.0.0
(`src/paths/unweighted.c`) that search does stop once it reaches the
target, but first builds the whole graph's adjacency list
(`igraph_adjlist_init`), on every call. `get_shortest_path(v, to=...)`
(`igraph_get_shortest_paths`) also stops at the target and reads each
visited node's edges as it goes (`igraph_incident`), so it skips that
pass. Weighted, both run Dijkstra over a lazy incidence list and stop at
the target. `paths/` uses `get_shortest_path` with `output="epath"`: the hop
count is the path's length, a weighted cost its weights' sum, and an
empty path between two different nodes means there is none. It was 2.5–17×
faster than `distances` on the unweighted datasets (dblp 0.26 s against
1.40 s) and the same on lesmis, where both run Dijkstra, and its answers
matched the reference on every dataset. The M11 claim that igraph "runs a
whole single-source search for each pair" was wrong: the adjacency list,
not the search, was the cost.

### Findings

**pgGraph was slower than pg_warren on every workload both run, on every
dataset, warm and cold.** Warm, by:

- *BFS reach and depths*: 24–420×. The cost is in pgGraph's output. Every
  row of `traverse` carries the node's path as JSON. `path_coordinates_json`
  names each step's table through `catalog::validate::relation_name`, an SPI
  query, which gdb samples of a wn18rr run found in the planner. With most of
  a graph returned, that is 45–110 µs a row. pgGraph documents traversal for
  bounded neighbourhoods ("Bounded BFS/DFS with depth, node, and frontier
  circuit breakers"), not for whole-graph sweeps.
- *Shortest paths*: 2.4–89×. A pgGraph call costs about 2.5 ms even on
  lesmis, the same for any depth bound, and 25 calls cost 55 ms there.
  Its closest results are on the road networks: ss-gleich-usroads-48
  (0.35 s against 0.14 s) and roadnet-tx (5.1 s against 1.45 s).
- *Components*: 1.7–14×; closest on wn18rr (15 ms against 9.1 ms),
  ss-gleich-usroads-48 (40 ms against 22 ms), dblp, yago3-10 and roadnet-tx
  (1.9×).
- *Cold*: pgGraph's first call loads the artifact; pg_warren's reads its
  segments. pgGraph's cold times were 1.5–230× pg_warren's.
- *Time to first query*: 5–21× (above).

**Where pgGraph wins.** Against igraph, on shortest paths on dblp: 73 ms
against 0.26 s, since igraph's search covers much of a 317,080-node graph
for each pair and pgGraph's, like pg_warren's, does not. Against igraph's
slower `distances` call in `run/`, pgGraph was also faster on email-enron,
yago3-10 and hetionet; that call is no longer the one compared. Nowhere
else, and nowhere against pg_warren.

**Shortest paths against igraph's fairer call.** pg_warren runs a
bidirectional BFS for each pair, all 25 in one SQL call. igraph's
`get_shortest_path` searches from one end and stops at the target.
pg_warren was faster on 7 of 12 datasets, by 2.7–44× (dblp 5.8 ms
against 0.26 s, yago3-10 1.2 ms against 17 ms). igraph was faster on 5:
lesmis (0.33 ms against 0.90 ms) and polblogs (0.52 ms against 1.28 ms),
where one SQL call's fixed cost dominates; facebook, within 5%
(2.39 ms against 2.50 ms); and the road networks, ss-gleich-usroads-48
(99 ms against 145 ms) and roadnet-tx (1.29 s against 1.45 s), whose
paths run to hundreds of hops. The road networks were not profiled.
Plain Cypher's `shortestPath`, the like-for-like algorithm (it also
searches from both ends), was 2.6–23× slower than pg_warren warm in the
M11 run on the 11 datasets it ran (dblp 26 ms against 6.0 ms,
ss-gleich-usroads-48 0.33 s against 0.13 s).

**pg_warren 0.8.0 against igraph elsewhere in `run/`.** As in M11, igraph
was faster on BFS reach on every dataset (1.5–66×), and on BFS depths and
components on all but three cells: yago3-10's depths (4.9 ms against
14 ms), and cit-hepth's and fb15k-237's components.
