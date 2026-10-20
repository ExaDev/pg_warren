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
