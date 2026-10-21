# Library group 1 at 1M nodes

`random-1m.json` is `bench/matrix.py --matrix bench/results/library/matrix.json
random-1m` (G(n, m) with 1M nodes and 10M undirected edges, so 20M stored
edges; `warren.threads = 4`) on a shared 4-vCPU Xeon, release build of
e0afd04, PostgreSQL 16. Each workload runs in a fresh session, so its time
includes loading the version (`prewarm`: 0.35 s). docs/limits.md and
DESIGN.md §6.7 read it.
