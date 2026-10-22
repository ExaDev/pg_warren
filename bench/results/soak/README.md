# 100M-edge soak results

`bench/soak.py` output, one file per case and run (DESIGN.md §12 "Measured").
Runs from now on record their own provenance under `provenance`: the full
argv, the harness's revision (and whether `bench/` or `test/` differed from
it), and the extension's revision and build profile as given to the
script, with the sha256 of the `pg_warren.so` the cluster loaded.

The five files below predate that. They were recorded on PostgreSQL 16.15
with release builds of pg_warren 0.3.0, at these revisions (from the runs
themselves; a `build` key added to each file by hand was removed):

| File | pg_warren build | Notes |
|---|---|---|
| `a.json` | 6e20074 | before both memory fixes; an earlier harness (no `skip` or `statement_timeout` keys) |
| `a-rerun.json` | 495646e | run without `eigen` (`--no-eigen`, which the harness did not record) |
| `b-unmerged-before-cache-fix.json` | f1987a1 | after the chunk-reader fix, before the held-version fix; an earlier harness; `eigen` cancelled at 769 s (`canceling statement due to user request`) |
| `b-unmerged.json` | 495646e | `--eigen-max-iter 200 --statement-timeout 15min` |
| `b-merged.json` | 495646e | `--eigen-max-iter 200 --statement-timeout 15min --skip second-version` |

Their harness revisions and exact command lines were not recorded. Their
verdicts are as that harness computed them: it judged `eigen` on time
alone (`a.json`'s 65-minute solve did not converge, residual 1.0·10⁻⁴; it
is a miss either way), left out stages that failed other than by a cancel,
and took the settings snapshot before the query sessions' `SET`s (so it
shows `warren.threads=2`, the default, where the queries ran with 4).

## The statement pin at a tenth of the scale (PR #16 review)

Two runs of case `b-unmerged` at `--scale 10` (1M nodes, 10M edges), with
the first pass's cap put just under the version and `pagerank`'s scratch,
as the full-scale run was (3.44 GB + 0.88 GB against 4 GiB):
`--cache-memory-kb 418945` is 429.0 MB, against 344.1 MB cached and 88 MB
of scratch (432.1 MB). Each records its provenance (command, harness
revision c0a1b81, clean, extension revision, library sha256). Both ran on
the same shared 4-vCPU Xeon as the files above, PostgreSQL 16.15, release
builds, one after the other under `/tmp/pgrx-test.lock`.

| File | pg_warren build | `pagerank` from cold, 429.0 MB cap | `pagerank` at a 1GB cap |
|---|---|---|---|
| `b-unmerged-scale10-pin.json` | c0a1b81 (the pin, after the review fixes) | **1 load**, 6.14 s (warm, 0 loads: 5.90 s) | 5.77 s cold, 6.08 s warm |
| `b-unmerged-scale10-before-pin.json` | 71e7272 (PR #15's merge, before the pin), `--skip second-version` | **156 loads**, 60.7 s (warm, 155 loads: 61.1 s) | 6.0 s cold, 6.1 s warm |

"Loads" are the `warren.segments` rows a `pagerank` statement read,
divided by those of the pass's first (cold) `prewarm` (88). The
`pagerank from cold` stage evicts the graph first. The pinned build's
`diff` between two versions took 3.6 s.

## Build memory at a tenth of the scale

`a-build-peak-scale10.json`, `b-merged-build-peak-scale10.json` and
`b-unmerged-build-peak-scale10.json`: the three cases at 1M nodes and 10M
edges (`--scale 10 --no-eigen --no-cte --skip second-version`), after the
build was charged to `warren.max_build_memory` and streamed (DESIGN.md
§5.4). Their `refresh` peaks back §5.4's and §12's build-memory figures;
their other rows are not used. Each records its provenance.

## The M7 full-scale re-run (10f68ab)

`a-m7.json`, `b-unmerged-m7.json` and `b-merged-m7.json`: every case at
10M nodes and 100M edges, after all of M7 (the statement pin and idle-pin
reclaim, the charged and streamed build, eigen's new defaults and faster
steps, batched reads, direction-optimising `reachable_set`), with a release
build of 10f68ab on PostgreSQL 16.15. The machine was idle (load average
0.4–1.6 at each case's start and end, recorded in each file): 4 vCPU Intel
Xeon, 15 GiB RAM, no swap. Settings are the harness's (§12 "Measured on":
`shared_buffers = 1GB`, `work_mem = 64MB`, `maintenance_work_mem = 1GB`,
`max_wal_size = 1GB`, `min_wal_size = 256MB`, `wal_level = minimal`, fsync,
`synchronous_commit` and `full_page_writes` off), `warren.threads = 4` and
every `warren.max_*_memory` at its 4GB default, as each file's `settings`
snapshot (now taken after the session's `SET`s) shows. One run per case,
in the order a, b-unmerged, b-merged, each committed as it finished:

    bench/soak.py <case> --pg-config /usr/bin/pg_config --parent /var/tmp \
        --extension-revision 10f68ab --extension-profile release --threads 4 \
        --statement-timeout 20min --label=-m7 --no-eigen-steps \
        --eigen-max-iter 200        # a and b-merged; b-unmerged ran with --no-eigen

`eigen` ran with `max_iter := 200` for its per-step cost only (no full
solve). The harness's `refresh` verdict reads VmHWM, which includes the
~1.1 GB of shared buffers the backend touches; the private peak (RssAnon,
`peak_rss_anon`) is what §12 reports.

On b-merged at the default limits, `eigen` failed with 53400 after its
steps (141.5 s; the 6.0 GB version's 1.71 GB over the cache cap is charged
to compute memory, and the run needs about 3 GB more), so its step cost
was measured separately, with `warren.max_cache_memory = 8GB`, in
`b-merged-m7-eigen-steps.json`; `a-m7-eigen-steps.json` is the same
measurement on case a at the default limits. Each runs `eigen` at
`max_iter` 100 and 200 in one backend on a freshly built graph (from the
generator query) and records the script that produced it in its
provenance; the difference of the two is the cost of the steps alone.

## The 0.4.0 full-scale re-run (5b0bf81)

`a-040.json`, `b-unmerged-040.json`, `b-merged-040.json` and
`a-040-locality.json`: every case at 10M nodes and 100M edges with a
release build of 5b0bf81 (0.4.0 as of the Leiden iterations PR, adf0fff,
plus these harness changes; `pg_warren.so` sha256 48c14e7e…), on the same
4-vCPU Xeon, 15 GiB RAM, no swap, PostgreSQL 16.15, with the M7 re-run's
settings and command lines, plus `--disk-floor 5368709120` (keep 5 GiB
free; M7 kept the default 4 GiB). One run per case, in the order a,
b-unmerged, b-merged, then a in locality order, each committed as it
finished; load average 1.1–1.6 at each start (the run itself), against
0.4–1.6 for M7:

    bench/soak.py <case> --label=-040 --disk-floor 5368709120 --pg-config /usr/bin/pg_config \
        --parent /var/tmp --extension-revision 5b0bf81 --extension-profile release --threads 4 \
        --statement-timeout 20min --no-eigen-steps \
        --eigen-max-iter 200        # a and b-merged; b-unmerged ran with --no-eigen
    bench/soak.py a --label=-040-locality --node-order locality …   # as case a

What the harness adds since M7 (bench/soak.py): after the second version
(and once the edge table and the first version are dropped, to free disk)
each case runs the library added in 0.4.0 on its current version in one
backend at the first pass's cap (groups 1 and 2 of DESIGN.md §6.7: the
iterative centralities for 20 iterations, the sampled ones from 16
sources, `similar_pairs` over every 100th node), then `louvain` and
`leiden` (default two iterations) each in its own backend, with a disk
guard that cancels a community run before it takes the disk below the
floor (its levels keep their rows until VACUUM, after the statement).
Left out: `label_propagation_majority` (tier 2, minutes and gigabytes of
executor sort per iteration at this scale), and `topological_order` and
`dag_longest_path` (the graph has cycles).

### Against M7 (seconds unless stated; M7 → 0.4.0)

| Workload (target) | a | b-unmerged | b-merged | a, locality order |
|---|---|---|---|---|
| `refresh` (< 8 min, < 4 GB) | 153 → 160, 1.55 GB | 228 → 239, 3.26 GB | 260 → 233, 3.26 GB | 163, **2.51 GB** (ordering scratch) |
| cold load (< 15 s) | 1.9 → 2.4 | 10.3 → 6.2 | 23.0 → 22.4 | 2.2 |
| BFS depth 6 vs CTE (≥ 20×) | 11.0× → 10.0× (0.67 → 0.73) | 0.71 → 0.76 | 23.4 → 24.9 (the load) | 11.2× (0.68) |
| `reachable_set` (> 50M edges/s) | 26M → 23M/s (3.86 → 4.30) | 16M → 15M/s (6.15 → 6.58) | 3.8M → 3.9M/s (the load) | 25M/s (3.95) |
| `components()` (< 10 s) | 6.2 → 6.9 | **10.1 → 11.8** (10.6 → 10.2 at 8GB) | 29.2 → 28.6 (the load) | 6.7 |
| `pagerank`, 20 it. (< 30 s) | 15.6 → 16.5 | 35.6 → 36.9 (32.7 → 36.7 at 8GB) | 40.1 → 38.4 (17.4 → 14.4 at 8GB) | 14.8 |
| `eigen`, a step (200 steps) | **0.61 → 0.81 s** (122 → 162) | not run | refused (53400): after 141.5 s → up front, 21.2 s (the load) | 0.62 s (124) |
| `diff` (< 30 s) | 17.8 → 20.5 | 58.4 → 59.7 | refused (53400) → not run (disk) | 18.5 |
| `distance` edit / degree | 0.58 / 0.16 → 0.55 / 0.15 | 11.6 / 23.4 → 12.7 / 28.2 | refused → not run (disk) | 0.57 / 0.15 |

Every peak is unchanged (to 0.01 GB). The times moved by up to ±17%
either way, more than the code explains: case a ran in a slow stretch of
this VM (the A/B below finds no stage reproducibly slower), and the
b cases' differences are of the same size as one build's run-to-run
spread. Where a row crosses a target, it says so: b-unmerged's
`components` now misses (11.8 s; 10.2 s in the same run at 8GB, so
borderline as at M7). b-merged's second version was
skipped for disk (8.82 GB free against 5.92 GB plus the floor); M7's run
had more free disk and showed `diff` and `distance` refused with 53400 at
these limits, which nothing since changes. b-merged's `eigen` is now
refused before its first step (PR #22), so only the 21 s load is spent.

### Is anything slower than at M7? (the A/B)

In `a-040.json` most stages ran 4–15% slower than in `a-m7.json`, and
`eigen` 33% (162 against 122 s for 200 steps), with every peak unchanged.
To tell the code from the machine, case a ran three times with each
build, alternately, the M7 build first, without the CTE baseline, the
second version, the library and the community runs (`--no-cte --skip
second-version`, and for 0.4.0 `--skip library --skip communities`):
`a-ab-10f68ab-{1,2,3}.json`, a release build of 10f68ab under its own
harness (bench/ at 10f68ab, `pg_warren.so` sha256 827ba5b4…), and
`a-ab-040-{1,2,3}.json`, the 5b0bf81 library of the runs above (sha256
48c14e7e…; run 3 records a dirty harness: this README was being edited,
nothing the harness runs). Seconds, runs 1 / 2 / 3, and the ratio of the
medians:

| Stage | 10f68ab (M7) | 5b0bf81 (0.4.0) | 0.4.0 / M7 |
|---|---|---|---|
| `refresh` | 138.9 / 144.0 / 150.8 | 125.4 / 150.3 / 143.4 | 1.00 |
| `refresh` from the generator | 151.1 / 177.6 / 172.3 | 142.4 / 175.6 / 179.8 | 1.02 |
| cold load (`prewarm`) | 1.99 / 1.91 / 2.00 | 1.89 / 2.16 / 1.99 | 1.00 |
| BFS depth 6 | 0.68 / 0.67 / 0.75 | 0.64 / 0.66 / 0.68 | 0.97 |
| `reachable_set` | 3.80 / 3.79 / 4.05 | 3.43 / 3.66 / 3.84 | 0.96 |
| `components` | 5.71 / 5.24 / 4.71 | 4.34 / 4.55 / 5.40 | 0.87 |
| `pagerank` | 14.1 / 15.9 / 14.1 | 12.5 / 15.1 / 14.8 | 1.04 |
| `pagerank` from cold | 15.5 / 16.4 / 15.7 | 15.5 / 15.8 / 16.6 | 1.01 |
| `eigen`, 200 steps | 112.9 / 121.2 / 122.8 | 127.3 / 121.8 / 142.6 | 1.05 |

No stage is reproducibly slower: the same build varies by 10–20% from
run to run on this VM (the build from the generator: 151–178 s at
10f68ab), and `a-040.json`'s run fell in a slow stretch (its build from
the generator, which no 0.4.0 change touches, took 196 s). `eigen` alone
is slower on median, by 5% (0.61–0.71 s a step against 0.56–0.61 s),
within that spread; 0.4.0's eigen changes (PR #22: the working set
checked before the first step, the projected and Ritz matrices charged)
cost once a solve or once a restart, not a step. Not a regression we can
show; a fresh pair of runs on a quieter machine would settle the 5%.

### The 0.4.0 library at 10M nodes and 100M edges

Seconds, and the backend's private peak (RssAnon, the version held in the
cache included), each call at the default limits, `persist := false`
where the call takes it:

| Call | a | a, locality | b-unmerged | b-merged (version over the cap: each call loads it, ~22 s) |
|---|---|---|---|---|
| `eigenvector_centrality`, 20 it. | 15.0, 1.78 GB | 14.0 | 39.4, 4.02 GB | 32.1, 7.29 GB |
| `katz_centrality`, 20 it. | 9.8, 1.95 GB | 8.6 | 26.2, 4.19 GB | 27.4, 7.37 GB |
| `hits`, 20 it. | 17.3, 1.87 GB | 14.8 | 49.0, 4.11 GB | 40.7, 7.36 GB |
| `clustering` | 47.5, 3.04 GB | 40.9 | 66.6, 5.27 GB | 67.1, 7.84 GB |
| `core_number` | 22.8, 3.20 GB | 19.8 | 38.2, 5.43 GB | 40.9, 8.00 GB |
| `strong_components_vector` | 15.8, 1.65 GB | 14.0 | 24.8, 3.89 GB | 31.8, 7.36 GB |
| `minimum_spanning_forest` (rows counted) | 54.4, 1.92 GB | 48.5 | 103.1 (weighted), 4.16 GB | 73.7 (weighted), 7.36 GB |
| `betweenness_centrality`, k = 16 | 61.1, 2.64 GB | 58.9 | 146.3 (weighted), 5.42 GB | 111.2 (weighted), 7.98 GB |
| `closeness_centrality`, k = 16 | 29.4, 2.08 GB | 27.7 | 104.1 (weighted), 4.86 GB | 96.2 (weighted), 7.42 GB |
| `harmonic_centrality`, k = 16 | 30.9, 2.00 GB | 27.9 | 102.8 (weighted), 4.78 GB | 96.3 (weighted), 7.36 GB |
| `most_similar`, one node | 0.002 | 0.001 | 0.002 | 17.1 (the load) |
| `similarity`, 100,000 pairs | 0.55 | 0.46 | 0.71 | 19.2 (the load) |
| `similar_pairs`, every 100th node | 10.2, 3.58 GB | 8.7 | 25.0, 5.82 GB | **refused (53400)**: the symmetric adjacency beside the 1.71 GB the version is over the cap |

### Louvain and Leiden at 10M nodes and 100M edges

None finished at the default limits on this machine:

| | a | a, locality | b-unmerged | b-merged |
|---|---|---|---|---|
| `louvain` | **stopped for disk** after 383 s: 7.52 GB of segments and 9.1 GB of WAL written, peak 7.65 GB | stopped for disk after 335 s, 7.48 GB written | stopped for disk after 302 s, 2.67 GB of segments written (less free disk: the b versions are 3× larger), peak 9.50 GB | **refused (53400)** after the 23 s load: the undirected view (3.2 GB) beside the 1.71 GB over the cap |
| `leiden` (2 iterations) | **refused (53400)** after 59 s, in level 1's refinement: 880 MB more with 3.52 GB in use | refused after 54 s, as a | refused after 76 s, as a | refused after 22 s, as `louvain` |

On a uniform random graph Louvain's levels stay large: at 1M nodes and
10M edges (the scale-10 case a, run while preparing this; not committed)
`louvain` took 22–33 s in 5–6 levels and wrote 0.61 GB of segments,
`leiden` 52–66 s in 16–17 levels and 1.98–2.01 GB, finding 14–36
communities of modularity 0.14–0.20 (the graph has no communities to
find). At 10M the stopped `louvain` had written 12× the 1M run's segments
and was still going: the disk a run needs grows faster than the graph,
because every level writes a quotient of the whole graph that a random
graph barely shrinks. Leiden's refinement needs 88 B a node more than
fits beside the 3.2 GB undirected view at the 4 GB compute limit. The peak
past `warren.max_cache_memory` plus `warren.max_compute_memory` (b-unmerged
`louvain`, 9.50 GB against 8.59 GB) is `contract`'s build scratch, charged
to `warren.max_build_memory`, verified below.

### Locality order on the random graph

`a-040-locality.json` against `a-040.json`: every query stage and library
call ran 2–23% faster in locality order, `eigen` the most (0.62 against
0.81 s a step). But `a-040.json` ran in a slow stretch (above), and
against the A/B's id-order runs the locality run is neither clearly faster
nor slower (BFS 0.68 s against 0.64–0.68, `reachable_set` 3.95 against
3.43–3.84, `components` 6.7 against 4.3–5.4, `pagerank` 14.8 against
12.5–15.1, `eigen` 124 against 122–143 s): one run each cannot show a gain
of the size the A/B's spread hides, and DESIGN.md §14 item 22 expects none
on a uniform random graph, which has no locality for an ordering to
recover. Its first build peaked at 2.51 GB private (VmHWM 3.66 GB) against
1.55 GB, as DESIGN.md §14 item 22 predicts for the ordering's scratch, and
took 163 against 160 s, within the 4 GB and 8 min targets.

### Louvain's peak against its charges

`b-unmerged-scale10-louvain-peak.json` checks the 9.50 GB `louvain` peak
above at a tenth of the scale (case b-unmerged at 1M nodes and 10M edges,
the same build and machine; the file records the script that ran it).
`louvain` at the default limits peaks at 1.081 GB private. Bisecting each
limit in turn (to within 3%), it still runs with `warren.max_cache_memory`
at 28 MB (its transient vectors; pinned versions past the cap go to
compute), then `warren.max_compute_memory` at 710 MB, then
`warren.max_build_memory` at 302 MB; with all three at those values it
runs at a peak of 1.083 GB, 15 MB (1.4%) past their 1.065 GB sum and
3.2 MB of baseline: Postgres's own memory (executor, SPI, PL/pgSQL), which
no warren setting covers. So nothing warren holds is uncharged, and the
part of the peak past cache and compute is `contract`'s build scratch: a
statement that runs `contract` is bounded by the three limits together.

## The region loader at a tenth of the scale (M9 step 3)

`a-scale10-step3-506b5c3-{1,2,3}.json` (main before step 3, `pg_warren.so`
sha256 fb99e1ab…) and `a-scale10-step3-3613402-{1,2,3}.json` (the region
loader, DESIGN.md §5.6 step 3, sha256 6e2e61d3…): case `a` at `--scale 10`
(1M nodes, 10M edges, a 120.1 MB version), release builds, PostgreSQL
16.15, the same 4-vCPU Xeon, run alternately (main first) under
`/tmp/pgrx-test.lock`, load average 0.6–1.3:

    bench/soak.py a --scale 10 --pg-config /usr/bin/pg_config --parent /var/tmp \
        --extension-revision <rev> --extension-profile release --threads 4 --no-cte \
        --no-eigen --skip second-version --skip library --skip communities \
        --disk-floor 4294967296

Seconds, runs 1 / 2 / 3, the private peak (RssAnon) over the three, and
the ratio of the medians:

| Stage | 506b5c3 | 3613402 | ratio |
|---|---|---|---|
| cold load (`prewarm`) | 0.196 / 0.185 / 0.188, 164.8 MiB | 0.160 / 0.161 / 0.155, **126.6 MiB** | 0.85 |
| cold load probe (64kB cap) | 0.193 / 0.190 / 0.183, 164.6 MiB | 0.182 / 0.153 / 0.151, 126.3 MiB | 0.81 |
| BFS depth 6 | 0.066 / 0.062 / 0.065 | 0.075 / 0.068 / 0.063 | 1.04 |
| `reachable_set` | 0.274 / 0.261 / 0.278 | 0.283 / 0.268 / 0.273 | 0.99 |
| `components` | 0.357 / 0.318 / 0.316 | 0.325 / 0.331 / 0.323 | 1.02 |
| `pagerank` | 1.194 / 1.230 / 1.190 | 1.161 / 1.220 / 1.202 | 1.01 |
| `pagerank` from cold | 1.342 / 1.322 / 1.248 | 1.294 / 1.278 / 1.284 | 0.97 |

A cold load copies each chunk straight into the version's region instead
of assembling each segment and then decoding it, so it is about 15%
faster and its peak is the version (114.6 MiB) plus 12 MiB, not plus
50 MiB (most of it one CSR's raw segments: 38 MiB of targets and 8 MiB of
offsets). Before the product kernels borrowed each CSR's
arrays once (3613402's parent, 5a07152), `pagerank` ran 25–30% slower
over the region's shared views (1.42–1.56 s), since every access to a
shared view is a virtual call; the other stages are within run-to-run
noise.

### The region zeroed lazily (PR #41 review)

`a-scale10-step3-fix-673eb64-{1,2,3}.json` (the region loader as
reviewed, `pg_warren.so` sha256 821f8fbf…) and
`a-scale10-step3-fix-ea7ae37-{1,2,3}.json` (the region allocated
byte-aligned and 64 bytes longer, so that `alloc_zeroed` is `calloc`, sha256
a6d40999…): the same command and machine, `--disk-floor 1073741824`, run
alternately (673eb64 first) under `/tmp/pgrx-test.lock`, load average
0.8–1.3. A 64-byte-aligned `alloc_zeroed` is `posix_memalign` and a
`memset` of the whole region, a pass over its 120 MB that the fill then
writes again; `calloc`'s large allocations are fresh zero pages.

| Stage | 673eb64 | ea7ae37 | ratio |
|---|---|---|---|
| cold load (`prewarm`) | 0.136 / 0.155 / 0.157, 126.6 MiB | 0.128 / 0.143 / 0.136, 126.6 MiB | 0.87 |
| cold load probe (64kB cap) | 0.150 / 0.154 / 0.156, 126.3 MiB | 0.143 / 0.150 / 0.146, 126.3 MiB | 0.95 |
| BFS depth 6 | 0.064 / 0.064 / 0.069 | 0.061 / 0.072 / 0.065 | 1.01 |
| `reachable_set` | 0.258 / 0.273 / 0.277 | 0.250 / 0.275 / 0.262 | 0.96 |
| `components` | 0.314 / 0.323 / 0.330 | 0.309 / 0.329 / 0.315 | 0.98 |
| `pagerank` | 1.118 / 1.085 / 1.176 | 1.142 / 1.311 / 1.101 | 1.02 |
| `pagerank` from cold | 1.196 / 1.333 / 1.246 | 1.260 / 1.301 / 1.252 | 1.01 |

The cold load is 5–13% faster; its peak is unchanged, since the fill
writes every page of the region either way. The other stages are within
run-to-run noise.
