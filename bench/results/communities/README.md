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

`blocks-1m-communities.json` is the matrix re-run on the same machine at
a later commit (its `git` field), with the community workloads (`modularity_of_blocks`, `louvain`, `leiden`, and each
with `max_levels` 1, 2 and 3: a level's time is the difference of two
runs). `blocks-1m-levels.json` adds, per run, the partition's modularity,
its number of communities and the number of levels (from
`levels_prefix`). `blocks-10m-communities.json` is the 10M graph with the
contraction workloads left out (disk: the matrix's 10M run of every
workload in one database needs more than the 15 GB this machine had);
`leiden_max_levels_3` ran out of disk there too, after the other runs'
vectors and dead segment rows had accumulated, and is recorded with its
error. At 10M nodes a fresh run gave `louvain` modularity 0.840 (1,425
communities, 94 s); `leiden` after it in the same database filled the
disk (DESIGN.md §6.7: a run's levels keep their rows until VACUUM).
DESIGN.md §6.7 (group 3) and docs/limits.md read these.
