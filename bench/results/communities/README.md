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
