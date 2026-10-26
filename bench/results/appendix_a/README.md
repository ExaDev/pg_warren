# Appendix A at 10M nodes

DESIGN.md Appendix A's use cases (A.1–A.5), run by `bench/appendix_a.py`
at about 10M nodes each (DESIGN.md §12, "Appendix A at 10M"; §13 M11).
One JSON file a use case: sizes, every stage (seconds, peak RssAnon and
VmHWM of the backend, free disk before it), every measure and check, the
verdicts against Appendix A's scale targets, the cluster settings and the
provenance.

- `a1.json` … `a5.json`: the full-scale runs.
- `a2-scale10.json`: A.2 at a tenth of the size (1M entities), run after
  the full one to record each staged version's eigenvalues, which the
  full run did not (see A.2 below).

## Provenance and caveats

- pg_warren 0.7.0, a release build of a5a3822 (main 3163b77 and the
  parallel-mode read fix found here), installed into PG 16.
- One shared VM: 4 vCPUs (Intel Xeon), 15.7 GB RAM, about 8.5 GB free
  disk. One use case at a time, nothing else running (load average below
  1 before each).
- The scratch cluster (`test/harness`'s, with `bench/soak.py`'s settings):
  `fsync = off`, `full_page_writes = off`, `synchronous_commit = off`,
  `wal_level = minimal`, `shared_buffers = 1GB`, `work_mem = 64MB`,
  autovacuum off. Writes are therefore cheaper than on a durable server;
  reads and compute are not affected.
- pg_warren at its defaults (`max_build_memory` 4 GB, `max_compute_memory`
  4 GB, `max_cache_memory` 8 GB where a use case sets it, otherwise its
  default), `warren.threads = 4`, the shared cache off (PG 16 without the
  preload). Appendix A's targets assume 8 threads.
- Sizes were chosen to fit the disk, not Appendix A's edge counts; each
  use case's sizes are in its file and below.

## Results

Seconds are a stage's median (a stage run once has one sample); target
after the slash. Memory is the backend's peak RssAnon / VmHWM in GB during
the stage (VmHWM is the process's high-water mark since it started, so it
never falls).

### A.1 fraud rings: 10.07M nodes, 7.0M undirected edges

3.35M honest accounts and their 6.7M own identifiers, 670 planted rings,
three hubs of degree 168,270, 27,088 and 67,280. Appendix A's target is
100M edges; the fixture's structure (each honest account with its own
1–3 identifiers) gives about 0.7 edges a node.

| stage | s | GB |
|---|---|---|
| refresh | 11.3 | 0.89 / 1.85 |
| suspicion program (41 iterations) | 25.2 / 60 | 1.85 / 2.95 |
| label propagation | 2.9 / 30 | 2.21 / 3.30 |
| aggregation | 11.0 / 30 | 2.05 / 3.15 |
| without the hub mask: program, label propagation | 23.5, 5.6 | 2.36 / 3.46 |

Checks, 9 of 9: every expression compiled; converged in 41 iterations
(30–45 expected); no output ring above 500 accounts (largest 40); every
planted ring with a flagged account among the top 837 rings; 99.7% of
unflagged ring members and 0.49% of honest accounts (the flagged ones)
score above 0.01; no masked ring mixes planted rings; without the mask
rings mix and the largest grows 360× (to 14,423 accounts). A persisted
vector write is 83.6 MB (8.3 bytes a node), which is why the program's
vectors are transient here and in DESIGN.md.

### A.2 ingest gate: 10M entities, 50M undirected edges, k = 20

20 communities of 500,000, 4 edges inside and 1 anywhere an entity;
ingest variants of 1M entities and 5M edges (20M for *dense bridge*);
embeddings of width 16. 44 minutes.

| stage | s | GB |
|---|---|---|
| base refresh | 33.0 / 480 | 1.47 / 1.76 |
| base eigen | 117.6 / 600 | 4.01 / 5.11 |
| staged refresh (4 variants) | 31.8–40.5 / 480 | up to 6.32 / 7.42 |
| diff | 8.3–9.1 / 30 | up to 4.75 / 5.85 |
| staged eigen, volume / bridge / novel | 118.8–128.1 / 600 | up to 7.56 / 8.65 |
| staged eigen, dense bridge | **1,331.8 / 600** | 7.78 / 8.88 |
| subspace distance | 1.0–1.1 / 30 | up to 5.48 / 6.58 |
| aggregate_neighbours (width 16) | 9.8–14.9 | up to 8.14 / 9.24 |
| base eigen again, shadow eigen | 113.0, 119.4 / 600 | 7.54 / 8.66 |

Checks, 31 of 35. Passed: `diff` adds 1M nodes and its edges are within
0.0006% of twice the generator's; every pre-existing entity keeps its
dense id; each comparison run twice gives the same distance; after
`ROLLBACK` no rows remain for the staged version; a committed staged
version is not current, is readable by `version`, and adds no node to
the default id map; a second `eigen` on the base gives bitwise-identical
eigenvalues and eigenvectors; the shadow-schema comparison gives the same
subspace distance (difference 0); subspace distance: volume 0.44, novel
1.41 (3.2× volume), dense bridge 1.45 (3.3×, the largest).

Failed:
- *bridge turns the subspace further than volume, and furthest*: 0.40
  against volume's 0.44. At A.2's 5 edges an entity against a base degree
  of 10, the bridge links communities 3 and 11 about as strongly as any
  two are linked already; it adds one low eigenvalue (0.091 at a tenth of
  the size, `a2-scale10.json`) without merging them. With 20 edges an
  entity (*dense bridge*, as `test/sql/a2_spectral.sql` ingests) it is
  the largest, as A.2 expects.
- *bridge moves embeddings most*: mean L2 drift 0.018 against volume's
  0.030 and dense bridge's 0.033: the bridge touches 2 of 20 communities.
- *shadow only adds* (`distance('edit')` across the two graphs): refused
  with 53400, "compute memory limit exceeded: 10911947136 bytes
  requested". The rebuild of the second graph's CSRs in the shared ids is
  charged by an over-estimate (96 bytes an edge plus 16 a node:
  `warren_core::spectral::distance::graphs::rebuild_bytes`, which counts
  f64 weights and a merged CSR the unweighted rebuild does not hold), so
  at 110M directed edges it needs 10.9 GB of `max_compute_memory`, more
  than this machine has (docs/limits.md).

The dense bridge's solve: no warning and residuals of 1.5·10⁻⁶ (others
7·10⁻⁸), but 11× the steps. At a tenth of the size its 20 smallest
eigenvalues are 0, 0.048, 18 community eigenvalues near 0.165, and 0.405,
the lower edge of the bulk (the base's 20th is a community eigenvalue,
0.166): the k-th wanted eigenvalue moved into the bulk's crowded edge,
where Lanczos needs many more steps (DESIGN.md §6.5).

### A.3 entity resolution: 9.92M nodes, 11.5M edges

1.45M entities, 4.35M records, 5.57M identifiers. Appendix A's target is
10M records and 50M edges.

| stage | s | GB |
|---|---|---|
| refresh | 12.4 | 0.60 / 1.11 |
| superstep 1 (`text[]` messages) | **29.3 / 20** | 1.49 / 2.34 |
| superstep 1's messages as `bigint` | 11.7 | 0.94 / 1.77 |
| its message query alone | 4.7 | 0.77 / 1.60 |
| superstep 2, superstep 3 | 7.7, 7.8 / 20 | 1.48 / 2.35 |
| trust SQL (carried, links, conflicts, held, trusted links) | 50.4 | 1.36 / 2.50 |
| components, trusted / untrusted | 1.3, 1.9 / 30 | 1.50 / 2.60 |

The version was prewarmed before superstep 1. Superstep 1 misses its
target: its `text` messages take the binary send/receive path (DESIGN.md
§6.4), 2.5× the same messages as `bigint`. The three supersteps and
components take 46 s against the 2-minute target.

Checks, 8 of 8: each superstep equals the SQL join + `array_agg` in
collect order on a 0.1% sample (4,274, 4,955 and 4,274 nodes); with
`max_compute_memory` at 64 MB a superstep fails with 53400; pairwise
precision 0.994 and recall 0.964 with the trust rules; precision 0.835
without them.

### A.4 typed retrieval: 10.0M nodes, 32.5M edges

8M papers (24M citations), 1.9M people, 100,000 institutions; IRIs, then
`bigint` ids.

| stage | s | GB |
|---|---|---|
| refresh, IRIs / bigint | 104.2 / 22.3 | 2.68 / 3.77 |
| three-step walk, cold / warm (IRIs) | 2.6 / 0.54 (target 2) | 1.77 / 2.87 |
| three-step walk, cold / warm (bigint) | 2.1 / 0.18 (target 2) | 1.71 / 2.81 |
| 10-hop first segment: walk / CTE | 0.57 (target 1) / 0.19 | 1.78 / 2.87 |
| reversed walk | 3.1 | 1.78 / 2.87 |

Checks, 11 of 11: `ends` (37 institutions) equals the chain of recursive
CTEs, for IRIs and bigint ids; `count` equals `ends`; `paths` bounded,
every hop an edge, every intermediate node passes its filter; dropping
the step-two filter visits more nodes (`last_stats`); 10 hops reach at
least what 3 do; the reversed walk reaches the root and equals its own
CTEs (2,841,158 papers). The 10-hop CTE took 0.19 s, not more than 60 s:
the root's 10-hop neighbourhood is small (citations point to older
papers), and a deduplicating CTE is fast there.

### A.5 embedding propagation: 10M entities, 50M undirected edges

The block model of A.2 with three relations; `aggregate_neighbours` over
`related_to` and `part_of`. d = 8 for the program and its round trip
through a `real[]` column (pgvector is not installed), and `aggregate_
neighbours` also at d = 32 from a transient matrix. Appendix A's d = 768
is 31 GB a matrix at 10M rows. The disk fell to 0.3 GB free during the
run.

| stage | s | GB |
|---|---|---|
| refresh | 46.6 | 2.44 / 2.97 |
| aggregate_neighbours, d = 32 | 27.3 / 300 | 7.20 / 8.30 |
| embeddings from the table, d = 8 | 7.3 | 2.99 / 4.08 |
| aggregate_neighbours, d = 8 | 7.5 / 300 | 3.62 / 4.72 |
| smooth (`vector_map`, persisted) | 8.3 | 4.18 / 5.27 |
| write back (`UPDATE` from `matrix_rows`) | 40.4 | 3.62 / 4.72 |

The round trip (in, aggregate, smooth, out) takes 64 s against 10
minutes. Checks, 6 of 7: the expression compiled; every row written back;
`aggregate_neighbours` equals SQL `AVG` within 1.5·10⁻⁷ on 10,000 sampled
nodes; within-community cosine similarity rises from 0.814 to 0.885;
broadcasting is elementwise; mismatched widths are refused (22023).
Failed: *between-community similarity grows by at most 10% of the
within-community gain*: it grows from 0.043 to 0.057, 20% of the gain.
At d = 8 the 20 random centroids are far from orthogonal (their mean
cosine is 0.04 here), and smoothing, which averages the noise away, moves
between-community pairs toward their centroids' similarity; the bound
needs a d at which random centroids are nearly orthogonal. 872 entities
have no neighbour in the two relations; their mean is NaN (SQL `AVG` of
nothing), which A.5's program as first written turned into a NaN
smoothed embedding. The program now keeps their own row
(`COALESCE(NULLIF(emb_nbr, 'NaN'), emb)`, compiled).
