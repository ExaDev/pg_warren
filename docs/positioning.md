# Where pg_warren fits

pg_warren helps you analyse the relationships in your Postgres data: how
things connect, what matters most, and which groups form. Your data stays
in your existing tables, and you work in SQL.

<span id="pggraph-and-pg_warren"></span>

## Isn’t this just pgGraph?

They overlap. Both read your existing tables and answer graph questions
in SQL. **pgGraph focuses on path and relationship queries over changing
data; pg_warren adds analysis of the whole network and “what if” scenarios.**

**pgGraph may suit you better if** you mostly ask “How is A connected to B?”
on data that changes frequently, want changes picked up through its sync
features, or want its supported Cypher or GQL query syntax.

**pg_warren may suit you better if** you want to rank important nodes,
find communities, or try a change before committing it. You can build a
graph from your proposed edits, compare it with the current graph, then
keep the change or roll it back. Saved analysis results stay attached to
the exact graph version that produced them.

For example: “If this warehouse closes, which locations lose a route to
supply?” Or: “Which accounts form unusually connected groups, and how has
that changed since yesterday?”

We haven’t benchmarked pg_warren against pgGraph, so we make no speed
claim. pg_warren is also pre-1.0; check that it meets your operational needs.

Try the complete [community detection](examples/communities.md),
[exposure and path tracing](examples/taint.md), and
[what-if scenario](examples/versions.md) examples.

## Other options

- **A few hops over a small part of your data:** ordinary SQL joins or a
  recursive query are often enough.
- **A property graph with Cypher inside Postgres:** [Apache AGE](https://age.apache.org/).
- **Sparse matrix algebra through GraphBLAS:** [OneSparse](https://onesparse.com/).
- **Routing on maps alongside PostGIS:** [pgRouting](https://pgrouting.org/).

Choose using the questions you need to answer, how often your data changes,
and your security and deployment requirements. For pg_warren, start with
[the runnable examples](examples/index.md) or [installation](../README.md#install).

## In detail

Both projects build compressed sparse row (CSR) adjacency structures from
relational data and expose graph operations through SQL. Keeping your
tables, using Rust, and avoiding a separate graph service are shared
properties. pgGraph also supports connected components and aggregations;
its scope extends beyond individual path queries.

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


### Graph computation and saved results

pg_warren uses the same built adjacency for traversal and computation.
Its library includes PageRank, Louvain, Leiden and centralities; semiring
products let you write your own propagation algorithms. Named result
vectors stay bound to the graph version that produced them and join back
to your business tables. See the [semiring example](examples/min-plus.md).

### Comparing proposed changes

Make a change to your tables in a transaction, build a staged version,
compute against it, compare it with the current graph, then promote and
commit or roll back. Pass the candidate version id consistently when
analysing it; omitting the id reads the current graph.

### Storage and operations

Graph versions and persisted vectors live in PostgreSQL tables and
participate in transactions and physical replication. This also costs
storage and WAL; retained versions and large analytical outputs need
budgeting. See [operations](https://exadev.github.io/pg_warren/docs/operations/)
for backup permissions, upgrades, pooling and replicas.

### Query features and access

pgGraph provides schema and relationship discovery, trigger-based sync,
source-row hydration, query-time source RLS, and bounded GQL/openCypher
profiles. Those are useful capabilities if your priority is an application
querying a changing relational network. pg_warren uses explicit node and
edge SQL, graph grants, and refreshes; `create_graph_from_keys` writes that
SQL from the primary and foreign keys of the tables you list
([graphs from keys](semantics.md#graphs-from-keys)), but it searches no
schema for them. It does not offer a Cypher or GQL query language. Review its [access model](semantics.md#access) before
assuming the authorization behaviour is interchangeable.
[pgGraph supported features](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/supported_features.mdx),
[limits and fit](https://github.com/Evokoa/pgGraph/blob/v1.2.1/docs/user_guide/limitations-and-fit.mdx).


The published [performance comparisons](../README.md#how-it-compares)
cover igraph and Neo4j, not pgGraph. They do not establish a speed advantage
over pgGraph.
