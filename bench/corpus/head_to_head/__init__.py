"""The head-to-head (DESIGN.md §12 "Real graphs", §13 M11): pg_warren timed
against pgGraph, igraph and Neo4j, with plain Cypher and with the Graph
Data Science library, on the same machine and the suite's datasets.

Each engine is an adapter behind one port (`engine`), so the runner
(`timing`) knows none of them: it loads a dataset into an engine, then runs
each workload (`workloads`) cold once and warm several times, and checks
the engine's answer against a reference (`answers`, `pagerank`) before its
time counts. `report` writes the results as Markdown. The engines:
`warren_engine` and `pggraph_engine` (SQL in one backend of the same
cluster), `igraph_engine` (python-igraph in this process), and
`cypher_engine` and `gds_engine`, which share one Neo4j server
(`neo4j_server`: the pinned download, its configuration, the import,
starts and stops)."""
