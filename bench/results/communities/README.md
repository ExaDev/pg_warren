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
