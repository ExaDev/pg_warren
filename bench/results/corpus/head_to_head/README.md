# Head-to-head: pg_warren, igraph, Neo4j and Neo4j GDS (M11)

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
partition's modularity.

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
    target runs a full single-source search per pair; pg_warren's
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
  igraph's `distances` with a target runs a whole single-source search for
  each pair. pg_warren also beat GDS Dijkstra by 5–370×, and Cypher's
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
- *Triangles.* igraph, on one thread, was 1.4–13× faster on every graph
  except the road networks and wn18rr: facebook 12 ms against 154 ms, email-enron 23 ms against
  185 ms, and hetionet 0.49 s against 4.17 s. GDS was also faster on five
  of them. `warren-core/src/compute/triangles.rs` intersects every
  neighbour's row, which is O(Σ d²) on skewed degrees, where igraph and GDS
  orient edges by degree first.
- *Leiden.* igraph was 1.9–180× faster everywhere (fb15k-237 0.14 s
  against 4.42 s, yago3-10 0.79 s against 4.19 s), and GDS was faster on 7 of 11
  datasets (roadnet-tx 2.5 s against 8.1 s). pg_warren's Leiden runs a
  level at a time through `warren.contract`, persisting each quotient graph
  as a version in the catalog. igraph and GDS keep every level in memory.
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
