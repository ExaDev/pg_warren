# Where pg_warren fits

pg_warren is a **versioned graph compute engine beside your Postgres tables**.
Use it when you need deep traversal, whole-graph algorithms, and the ability
to build and compare a proposed graph before committing the data change.

pgGraph already accelerates traversal over ordinary PostgreSQL tables.
pg_warren's reason to exist is the combination of **graph computation and
explicit transactional versions**: run PageRank, Louvain, Leiden or a
semiring program, save its results against a graph version, and compare
versions inside your SQL workflow.

## pgGraph and pg_warren

There is real overlap. Both projects build compressed sparse row (CSR)
adjacency structures from relational data and expose graph operations
through SQL. Keeping your tables, using Rust, and avoiding a separate
graph service are shared properties, not reasons to choose one over the
other. pgGraph also supports connected components and aggregations; its
scope extends beyond individual path queries.

The comparison below uses **pgGraph 1.2.1**, its latest published release
checked on **29 October 2026**, and pg_warren's current documented API.
It compares supported workflows, not measured speed.
[pgGraph release and overview](https://github.com/Evokoa/pgGraph/tree/v1.2.1),
[SQL API](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/api-reference.mdx).

| Requirement | pgGraph 1.2.1 | pg_warren |
| --- | --- | --- |
| Traverse existing relational data | BFS/DFS, shortest paths, search, hydration and filters | BFS, reachability, shortest paths and typed walks; results join back by your ids |
| Whole-graph computation | Components and aggregations; no documented PageRank, Louvain/Leiden or semiring programming API | PageRank, Louvain/Leiden, centralities, named vectors, semiring products and spectral analysis |
| Keep graph reads close to source writes | Trigger sync and mutable overlays, with automatic query catch-up under documented limits | Explicit refresh produces an immutable version; later source changes wait for another refresh |
| Select and compare graph versions | Published generations managed by the projection runtime and MVCC | Explicit version ids, staged builds, promotion, retention, structural diffs and version-bound vectors |
| Build from your own uncommitted source changes | Build/vacuum requires committing those changes first | Refresh can build a staged version from them; source writes and graph build can roll back together |
| Persist graph structure | Derived artifact files and projection segments | Transactional extension tables covered by PostgreSQL WAL; graphs can be read on streaming standbys |

These differences follow from the two storage and freshness models.
pgGraph documents durable sync, transaction-local deltas and MVCC-visible
generations: saying it has "no transactions" would be wrong. Its
**build** operation specifically rejects a transaction already holding
write-capable locks on its sources. That is different from pg_warren's
staged-build workflow.
[pgGraph sync and build contract](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/sync-and-maintenance.mdx),
[persistence and standby boundary](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/build-and-persistence.mdx).

For pg_warren's corresponding contracts, see [versions and transactions](semantics.md#versions),
[the compute library](compute.md#the-algorithm-library),
[community detection](communities.md#finding-communities-louvain-and-leiden),
and [replicas](https://exadev.github.io/pg_warren/docs/operations/).

## Where pg_warren has an advantage

**Analysis that goes beyond finding a path.** Run community detection,
rank the entire graph, or express propagation with sparse matrix products.
The same built adjacency supports traversal and computation. Named result
vectors stay bound to the graph version that produced them and can be
joined back to your business tables.

**Reproducible scenarios.** Make a proposed change to your tables, build a
staged version, compute against it, compare it with the current graph,
then promote and commit or roll back. For example: "If this facility
closes, which locations become unreachable, and how do the network's
communities change?" An analysis must pass the candidate version id
consistently; omitting it reads the current graph.

**A PostgreSQL storage lifecycle for the graph.** Graph versions and
persisted vectors live in tables. They participate in transactions and
physical replication. This also costs storage and WAL; retained versions
and large analytical outputs need budgeting. See [operations](https://exadev.github.io/pg_warren/docs/operations/)
for backup permissions, upgrades, pooling and replicas.

Try the complete [community detection](examples/communities.md),
[semiring](examples/min-plus.md), [exposure and path tracing](examples/taint.md),
and [staged-version](examples/versions.md) examples.

## When pgGraph may fit better

pgGraph provides schema and relationship discovery, trigger-based sync,
source-row hydration, query-time source RLS, and bounded GQL/openCypher
profiles. Those are useful capabilities if your priority is an application
querying a changing relational network. pg_warren uses explicit node and
edge SQL, graph grants, and refreshes; it does not offer a Cypher or GQL
query language. Review its [access model](semantics.md#access) before
assuming the authorization behaviour is interchangeable.
[pgGraph supported features](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/supported_features.mdx),
[limits and fit](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/limitations-and-fit.mdx).

pg_warren is pre-1.0. A larger algorithm vocabulary alone does not establish
production maturity or a faster implementation of a shared operation.
The published [performance comparisons](../README.md#how-it-compares) cover
igraph and Neo4j, **not pgGraph**. We do not claim a measured speed advantage
over pgGraph.

## The wider landscape

| Your main need | A starting point |
| --- | --- |
| A few indexed hops with small frontiers | Ordinary joins or recursive SQL; avoid a graph build unless repeated work justifies it |
| Bounded graph queries over a changing relational schema, with discovery and sync | [pgGraph](https://github.com/Evokoa/pgGraph/tree/v1.2.1) |
| Reusable graph computation, named analytical results and transactional what-if versions over your tables | pg_warren |
| A property-graph model and openCypher within PostgreSQL | [Apache AGE](https://age.apache.org/) |
| Sparse matrix and vector algebra through GraphBLAS in PostgreSQL | [OneSparse](https://onesparse.com/) |
| Geospatial routing and specialist network algorithms alongside PostGIS | [pgRouting](https://pgrouting.org/) |

These projects overlap, but their interfaces and operating models differ.
Choose using the query, freshness, security and deployment requirements
of your workload. For pg_warren, start with [the runnable examples](examples/index.md)
or [installation](../README.md#install).
