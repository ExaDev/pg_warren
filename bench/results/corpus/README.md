# Real-graph suite results (M11)

`bench/corpus/suite.py` output (DESIGN.md §12 "Real graphs", §13 M11;
`bench/corpus/README.md` describes the suite). Each file records its
provenance under `provenance`: the full argv, the harness's revision, the
extension's revision and profile as given to the script, the sha256 of the
`pg_warren.so` the cluster loaded, and keepset's provenance of the bytes
(`fetched.provenance`: URL, sha256, size, catalogue 1.33.0).

- `correctness/<dataset>.json`: `suite.py check`, every tiny and medium
  dataset against NetworkX 3.x.
- `<dataset>.json`: `suite.py time --tier medium`, the §12 matrix.
- `ids/<dataset>.json`: `suite.py ids`, the text id lengths and front coding
  (DESIGN.md §5.7, §14 item 41).

## Provenance

| | |
|---|---|
| pg_warren | release build of 1ffdcd9 (origin/main when the branch started), `cargo pgrx package` into a private tree; `pg_warren.so` sha256 `5499e8bca4b5710e1b28f9addaf05fe9538a6100b34dcf8406c30e2ff4ac2174` |
| PostgreSQL | 16.15 (the machine's packages), a scratch cluster per run with the soak's settings (`settings` in each file): `shared_buffers = 1GB`, `warren.threads = 4`, every `warren.max_*_memory = 4GB`, `statement_timeout = 10min`, `fsync = off` |
| Machine | 4-vCPU Intel Xeon VM (no SMT), 15 GiB RAM, no swap, kernel 6.18.44, shared with other agents' builds: load averages are in each file (`hardware.load_average`), 2.6–6.1 at the start of the timed datasets |
| Harness | `check`: 67b54c3; `time`: 1ccaf30 (the code that wrote the files is the branch's). Every file says `dirty: true` because this directory was untracked while they were written, which the harness counts; `bench/corpus/` itself matched the recorded revision |

The timed tier was run twice. The files here are the second run, made after
2b8e27a, which starts a directed graph's traversals from its node with most
out-edges (the first run's yago3-10 BFS started at a node with none and
reached only itself) and records the rows each traversal reached. The two runs
agree to within the machine's noise (the second, under more load, was 0–40%
slower) except `eigen` on the road networks, below.

† ss-gleich-usroads-48 and roadnet-tx were timed a third time, after `eigen`
took the components' eigenvalue exactly (DESIGN.md §6.5, §12): a release
build of dcb9d2b (that change merged with sparse per-relation offsets, 96df7d4),
packaged the same way, `pg_warren.so` sha256 in each file. Their rows here are
that run's; the other datasets' are the second run's.

## Correctness

| Dataset | Tier | Nodes | Stored edges | Checks matched | Not matched | Q: louvain / leiden / NetworkX Louvain | Seconds |
| --- | --- | --- | --- | --- | --- | --- | --- |
| karate | tiny | 34 | 156 | 17 of 17 |  | 0.416 / 0.420 / 0.419 | 0.8 |
| lesmis | tiny | 77 | 508 | 18 of 18 |  | 0.565 / 0.567 / 0.566 | 0.9 |
| celegans | tiny | 131 | 764 | 19 of 19 |  | 0.415 / 0.413 / 0.405 | 0.8 |
| polblogs | tiny | 1490 | 19025 | 18 of 19 | clustering (documented difference) | 0.432 / 0.432 / 0.432 | 1.7 |
| london_transport | tiny | 369 | 860 | 17 of 17 |  | 0.825 / 0.832 / 0.829 | 0.8 |
| transport-airline | tiny | 3425 | 38513 | 16 of 17 | clustering (documented difference) | 0.666 / 0.667 / 0.658 | 1.8 |
| facebook | tiny | 4039 | 176468 | 17 of 17 |  | 0.835 / 0.836 / 0.835 | 5.3 |
| kg-codex-s | tiny | 2034 | 32888 | 19 of 20 | modularity, a pair in several relations (definition difference) | 0.547 / 0.547 / 0.547 | 3.1 |
| wn18rr | tiny | 40943 | 93003 | 18 of 20 | clustering (documented difference), modularity, a pair in several relations (definition difference) | 0.837 / 0.845 / 0.837 | 13.7 |
| cit-hepth | medium | 27770 | 352807 | 18 of 19 | clustering (documented difference) | 0.656 / 0.665 / 0.658 | 30.1 |
| email-enron | medium | 36692 | 367662 | 17 of 17 |  | 0.619 / 0.626 / 0.606 | 18.8 |
| fb15k-237 | medium | 14541 | 310116 | 18 of 20 | clustering (documented difference), modularity, a pair in several relations (definition difference) | 0.526 / 0.555 / 0.537 | 48.3 |
| ss-gleich-usroads-48 | medium | 126146 | 323900 | 17 of 17 |  | 0.979 / 0.980 / 0.979 | 30.0 |
| dblp | medium | 317080 | 2099732 | 17 of 17 |  | 0.821 / 0.830 / 0.821 | 194.9 |
| yago3-10 | medium | 123182 | 1089040 | 19 of 20 | modularity, a pair in several relations (definition difference) | 0.736 / 0.741 / 0.737 | 577.9 |
| hetionet | medium | 45158 | 2250197 | 19 of 20 | modularity, a pair in several relations (definition difference) | 0.340 / 0.345 / 0.341 | 776.6 |
| roadnet-tx | medium | 1379917 | 3843320 | 17 of 17 |  | 0.991 / 0.992 / 0.991 | 457.0 |

Node counts, dropped edges, degrees, BFS depths, components, shortest path
lengths and Dijkstra costs, triangles and core numbers match exactly
everywhere; PageRank within 3·10⁻¹⁵ (checked at 10⁻⁹). Stored edges count
both directions of an undirected graph. The two kinds of difference:

- **clustering at a self-loop's node (documented):** pg_warren's
  `clustering` counts the node's self-loop in its degree; the oracle's simple
  graph drops it. Only nodes with a self-loop differ (polblogs 2,
  transport-airline 1, wn18rr 2, cit-hepth 35, fb15k-237 1,301); every other
  node matches to 10⁻¹².
- **modularity of a graph with relations:** `warren.modularity` weighs an
  unweighted pair linked in *k* relations *k* in its undirected view, where
  `spmv`'s A (and PageRank, which matches NetworkX with A = 1) weighs it 1
  (DESIGN.md §6.3, docs/compute.md). On NetworkX's Louvain partition the two
  definitions differ by −0.0096 to +0.0206 (yago3-10). Reported as a finding
  (a likely inconsistency in pg_warren), not changed here. Against the
  per-relation definition, `warren.modularity` matches NetworkX exactly, and
  `louvain`'s and `leiden`'s returned modularity matches NetworkX's of their
  partitions.

`louvain` and `leiden` are held to NetworkX Louvain's modularity less 0.02;
every dataset passes (closest: fb15k-237's `louvain`, 0.526 against 0.537).

## The §12 matrix on the medium tier (seconds; ms where marked)

`bfs depth 6` and `reachable_set` are server-side medians of three runs, and
`neighbours` of twenty (`bench_seconds`); the rest are single statements timed
from the client. The start node is the node with most out-edges (most edges when
undirected).

| Dataset | refresh | cold load | bfs depth 6 | bfs depth 6 recursive CTE | reachable_set | components | pagerank 20 iterations | eigen k=20 | aggregate_neighbours d=64 | diff |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| cit-hepth | 0.125 | 0.012 | 2.69 ms | 0.139 | 4.38 ms | 6.60 ms | 0.081 | 1.28 | 0.091 | 0.014 |
| email-enron | 0.127 | 5.30 ms | 8.25 ms | 0.808 | 4.66 ms | 8.10 ms | 0.079 | 0.953 | 0.084 | 0.015 |
| fb15k-237 | 0.379 | 0.135 | 8.10 ms | 0.801 | 2.86 ms | 4.50 ms | 0.058 | 0.192 | 0.053 | 0.443 |
| ss-gleich-usroads-48 † | 0.117 | 9.00 ms | 0.23 ms | 0.90 ms | 0.023 | 0.025 | 0.152 | 68.91 | 0.216 | 0.029 |
| dblp | 0.435 | 0.036 | 0.104 | 4.15 | 0.060 | 0.072 | 0.380 | 21.35 | 0.657 | 0.119 |
| yago3-10 | 0.827 | 0.123 | 3.91 ms | 0.249 | 3.58 ms | 0.030 | 0.161 | 1.59 | 0.240 | 0.245 |
| hetionet | 1.19 | 0.090 | 0.014 | 4.90 | 7.70 ms | 0.019 | 0.121 | 0.628 | 0.163 | 0.120 |
| roadnet-tx † | 1.35 | 0.099 | 0.24 ms | 1.50 ms | 0.257 | 0.279 | 1.10 | 0.360 | 8.43 | 0.358 |

| Dataset | neighbours | triangles | clustering | core_number | louvain | leiden |
| --- | --- | --- | --- | --- | --- | --- |
| cit-hepth | 0.10 ms | 0.225 | 0.260 | 0.015 | 0.256 | 0.498 |
| email-enron | 0.13 ms | 0.190 | 0.187 | 0.012 | 0.137 | 0.362 |
| fb15k-237 | 0.18 ms | 0.322 | 0.330 | 0.012 | 0.782 | 1.94 |
| ss-gleich-usroads-48 † | 0.05 ms | 0.015 | 0.024 | 0.016 | 0.280 | 0.644 |
| dblp | 0.09 ms | 0.398 | 0.412 | 0.182 | 1.32 | 2.96 |
| yago3-10 | 0.09 ms | 0.219 | 0.236 | 0.062 | 1.29 | 3.29 |
| hetionet | 1.57 ms | 6.69 | 5.98 | 0.101 | 1.86 | 3.32 |
| roadnet-tx † | 0.06 ms | 0.181 | 0.276 | 0.266 | 3.99 | 9.08 |

What each traversal reached, the version, and memory:

| Dataset | Nodes | Stored edges | Relations | Version (MB) | refresh peak anon (MB) | Highest HWM (GB) | bfs depth 6 rows | reachable_set rows | reachable_set edges/s | Load average (1/5/15 min) |
|---|---|---|---|---|---|---|---|---|---|---|
| cit-hepth | 27770 | 352807 | 0 | 3.7 | 14 | 0.25 | 9732 | 16498 | 81M | 3.4 / 1.6 / 1.7 |
| email-enron | 36692 | 367662 | 0 | 4.1 | 17 | 0.25 | 33678 | 33696 | 79M | 3.3 / 1.6 / 1.7 |
| fb15k-237 | 14541 | 310116 | 237 | 60.5 | 38 | 0.46 | 12828 | 12830 | 108M | 3.6 / 1.7 / 1.8 |
| ss-gleich-usroads-48 † | 126146 | 323900 | 0 | 6.6 | 17 | 0.48 | 68 | 126146 | 14M | 2.6 / 2.6 / 3.5 |
| dblp | 317080 | 2099732 | 0 | 27.0 | 68 | 0.92 | 303357 | 317080 | 35M | 5.2 / 3.3 / 2.4 |
| yago3-10 | 123182 | 1089040 | 37 | 93.5 | 70 | 0.99 | 7300 | 7690 | 304M | 4.4 / 3.2 / 2.4 |
| hetionet | 45158 | 2250197 | 24 | 54.4 | 59 | 0.67 | 35496 | 35496 | 292M | 4.6 / 3.4 / 2.4 |
| roadnet-tx † | 1379917 | 3843320 | 0 | 75.1 | 141 | 2.71 | 144 | 1351137 | 15M | 1.9 / 2.4 / 3.3 |

Findings:

- **`eigen` on road networks.** `eigen(k = 20, normalized_laplacian,
  smallest)` took 65 s (run 1) and 105 s (run 2) on usroads-48's 126K
  nodes and ran past the 10-minute `statement_timeout` on roadnet-tx in both
  runs; on every other graph it is 0.2–21 s. Diagnosed in DESIGN.md §12, two
  causes:
  - roadnet-tx has 424 connected components, so all 20 wanted eigenvalues
    are 0. Lanczos found one copy and spent its steps on the largest
    component's clustered eigenvalues; outside the database it reached
    `max_iter` after 32 minutes with a wrong answer. `eigen` now returns
    each component's eigenvalue exactly: 0.36 s (run 3), twenty zeros,
    largest residual 5.4·10⁻⁸.
  - usroads-48 is one component whose smallest eigenvalues are 10⁻⁶ apart:
    Lanczos's slow case, correct but slow. 68.9 s (run 3; 20,466 steps).
    A larger basis takes it to about 25 s (§14 item 50); not the default,
    for its memory.
- **`aggregate_neighbours` on roadnet-tx took 8.4 s in run 3** (9.0 s
  repeated), against 2.3 s in run 2; usroads-48's went 0.43 → 0.22 s.
  This is noise, not a regression. A controlled A/B on roadnet-tx in the
  suite's session order took 2.15–2.40 s with 1ffdcd9 and 2.24–2.43 s with
  96df7d4 (main after sparse per-relation offsets). The kernel itself
  takes 0.05 s in both; the stage's time goes on its mean `vector_map` and
  on persisting about 700 MB of vectors, which is disk-bound and varied
  2.2× between runs on this shared disk.
- **Versions of graphs with many relations are mostly per-relation offsets.**
  fb15k-237's version is 60.5 MB for 310K stored edges and 14.5K nodes,
  against dblp's 27 MB for 2.1M edges and 317K nodes: 237 relations × 14,542
  offsets × 8 bytes × 2 (forward and reverse) is 55 MB of it. yago3-10 (37
  relations × 123K nodes, 73 MB of 93.5) is the same. Offsets per relation
  grow as relations × nodes, however few edges a relation has.
- **The recursive CTE** is 40–350× slower than `bfs` where the BFS reaches
  thousands of nodes (dblp 4.15 s against 0.10 s, hetionet 4.90 s against
  0.014 s), and close where it reaches few (the road networks, 68 and 144
  nodes in six steps).
- **Memory** stays under 2.7 GB high-water mark (roadnet-tx, `leiden`), and a
  `refresh` peaks at 14–141 MB of anonymous memory.

## Text ids (`ids/`)

| Dataset | Ids | Mean bytes | p50 / p90 / p99 / max | Mean shared prefix | Id map | `id_bytes` front coded | Front-coded map | Map smaller by |
|---|---|---|---|---|---|---|---|---|
| transport-airline | 3425 | 3.0 | 3 / 3 / 3 / 3 | 1.8 | 51 kB | 0.93× | 40 kB | 1.28× |
| kg-codex-s | 2034 | 6.3 | 7 / 7 / 9 / 9 | 3.8 | 37 kB | 1.35× | 27 kB | 1.39× |
| wn18rr | 40943 | 8.0 | 8 / 8 / 8 / 8 | 4.7 | 819 kB | 1.45× | 575 kB | 1.43× |
| fb15k-237 | 14541 | 8.8 | 9 / 10 / 10 / 10 | 6.4 | 302 kB | 1.86× | 192 kB | 1.57× |
| yago3-10 | 123182 | 16.7 | 15 / 26 / 39 / 97 | 7.0 | 3538 kB | 1.38× | 2535 kB | 1.40× |
| hetionet | 45158 | 19.6 | 17 / 30 / 30 / 32 | 18.1 | 1428 kB | 4.31× | 589 kB | 2.42× |

The id map is `id_bytes` + `id_offsets` (u64) + `id_order` (u32 here); the
front-coded map is the coded bytes + a u64 offset per block of 16 + `id_order`
+ a u32 rank permutation. No IRI-keyed graph was fetchable (DESIGN.md §5.7).
