# Contraction at 1M and 10M nodes

`blocks-1m.json` and `blocks-10m.json` are `bench/matrix.py --matrix
bench/results/communities/matrix.json blocks-1m blocks-10m` on a shared
4-vCPU Xeon, release build of 94ffd68, PostgreSQL 16, `warren.threads = 4`.
The graph is generated inside Postgres (no edge table): N nodes in blocks of
1,000, five undirected edges per node, 90% inside its block (relation `in`)
and 10% to a uniform node (`out`): 5N undirected edges, 10N stored, no
reverse or merged CSR. Labels come from `components_vector` over `in`, one
per block (N / 1,000 communities); `contract_by_dense_ids` contracts by each
node's dense id, a quotient as large as the graph (the worst case).
Each workload runs in a fresh session, so its time includes loading the
version (`prewarm`). DESIGN.md §6.6 and docs/limits.md read these.

## Louvain and Leiden

`blocks-1m-communities.json` is the matrix re-run on the same machine at a
later commit (its `git` field), with the community workloads
(`modularity_of_blocks`, `louvain`, `leiden`, and each with `max_levels`
1, 2 and 3: a level's time is the difference of two runs).
`blocks-1m-levels.json` adds, per run, the partition's modularity, its
number of communities and the number of levels (from `levels_prefix`).
At 10M nodes each algorithm ran in a database of its own, without the
contraction workloads, to fit the machine's disk (a run's dropped level
graphs keep their rows until `VACUUM`): `blocks-10m-louvain.json` and
`blocks-10m-leiden.json`, with their `-levels.json` summaries. The
database held 6.7 GB after the four `louvain` runs and 9.1 GB after the
four `leiden` runs. Runs with `max_levels` below the number of levels
`leiden` takes return the refined partition, hence their low modularity.
DESIGN.md §6.7 (group 3) and docs/limits.md read these.

## Leiden's iterations

`blocks-1m-iterations.json` is the matrix re-run at a later commit (its
`git` field), after `leiden` gained `iterations` (default 2): the
`leiden_max_levels_*` workloads pass `iterations := 1`, so they still
time one iteration's levels, and `leiden_iterations_1` times the single
pass. `blocks-1m-iterations-levels.json` summarises it as
`blocks-1m-levels.json` does, with each run's segments and WAL written;
those, and the `iterations := -1` run, come from a separate session on a
scratch cluster of the same build (`seconds` there is that run's). The
machine was slower that day than for `blocks-1m-communities.json`:
`louvain`, unchanged, took 9.1 s against 5.8 s, and Leiden's single pass
10.4 s against 8.3 s for the same partition, bit for bit. Two iterations
took 20.9 s and raised modularity from 0.837 to 0.898 (the planted blocks
score 0.899); until stable, 0.898 in 40.7 s. The 10M figures are still
the single pass. The 0.4.0 soak (`bench/results/soak/README.md`) ran
both at the default limits on its 10M-node random graphs, where neither
completes (`leiden` refused with 53400, `louvain` stopped for disk), so
two iterations at 10M were measured on the block graph instead.

## The defaults at 10M (0.4.0 soak)

`blocks-10m-defaults.json`: `louvain` and `leiden` at their defaults
(`leiden`'s two iterations) on `blocks-10m`'s graph (the same
`create_graph`, 0.72 GB of segments), each in a fresh backend, through
`bench/soak.py`'s community pass on a scratch cluster with the soak's
settings, release build of 827b94f (5b0bf81's code), PostgreSQL 16.15,
`warren.threads = 4`, default memory limits; the file records the script
that ran it. `louvain` 115.4 s (93 s in `blocks-10m-louvain.json`, the same
partition: modularity 0.8401, 1,425 communities, 5 levels), peak private
memory 4.31 GB, 2.48 GB of segments and 2.61 GB of WAL written. `leiden`
246.5 s, 18 levels, modularity 0.8996 (the planted blocks: 0.8995),
5,528 communities, peak 5.28 GB, 7.81 GB of segments and 8.22 GB of WAL.
