---
title: Architecture
description: How pg_warren turns SQL-defined graphs into versioned adjacency structures and SQL-callable computation.
editUrl: https://github.com/ExaDev/pg_warren/edit/main/site/src/content/docs/docs/architecture.md
---

pg_warren is a graph index and compute engine inside PostgreSQL. It derives
a compact adjacency structure from ordinary tables. Source tables remain
authoritative, and SQL functions expose the graph to your queries.

:::note[Development documentation]
This overview follows `main`. See the [release notes](/pg_warren/docs/changelog/)
for changes between software releases. The full engineering specification is
[DESIGN.md](https://github.com/ExaDev/pg_warren/blob/main/DESIGN.md).
:::

## From tables to a graph version

| Stage | What happens |
| --- | --- |
| Define | A node query returns external ids. An edge query returns source and destination ids, optionally followed by a relation and weight. |
| Build | `refresh` reads the source queries under one snapshot, maps ids, cleans edges, and builds compressed sparse row (CSR) adjacency structures. |
| Persist | The version's metadata, id map, and adjacency segments are stored in extension-owned tables, in chunks. |
| Read | A query opens the requested version, loads or maps its cached representation, and runs a traversal or computation. |
| Return | Node-returning traversals and vector reads use external ids. You join their results to application tables with SQL; scalar lookups return values directly. |

A build records dropped and deduplicated edges. It does not silently ignore
invalid endpoints without reporting their counts. Directed graphs can store
both forward and reverse adjacency. A graph with several relations can
store merged adjacency for faster all-relations reads, at a memory cost.
See [graph semantics](/pg_warren/docs/semantics/#graphs).

## Why graph versions matter

A built graph is immutable. Updating an application table does not mutate
an existing graph version. A refresh creates another version, which can
become current immediately or remain staged for comparison.

Building inside a transaction means the table changes and graph version
can commit or roll back together. The graph's persistent state is ordinary
PostgreSQL data, covered by WAL and physical replication. This also makes
staged builds and structural change detection possible without exporting
data to a separate service.

External ids are mapped to dense ids for computation. Existing dense ids
remain stable across refreshes; removed nodes leave tombstones. Explicit
compaction creates a new id space and limits which cross-version
comparisons are valid. See [dense ids and compaction](/pg_warren/docs/semantics/#stable-dense-ids-and-compaction).

## One representation for traversal and compute

CSR stores each node's neighbours in contiguous arrays. Traversals follow
these adjacency rows rather than repeatedly crossing from a recursive SQL
query into index lookups.

The same representation acts as a sparse matrix for computations over
named vectors and matrices. SQL functions provide semiring products,
per-node expressions, reductions, and message gathering. Algorithm control
flow can remain in PL/pgSQL. PageRank, community algorithms, and spectral
analysis build on this model.

Compute workers operate on extension-owned buffers. PostgreSQL calls and
interrupt handling remain on the backend's main thread. Memory budgets
bound builds, caches, and individual computations. The
[settings and memory model](/pg_warren/docs/limits/) explain what each limit
includes and how a refusal is reported.

## Caches and connections

By default, loaded graph versions and persisted vectors can be held in a
cluster-wide shared cache, mapped by the backends that read them. A
backend falls back to its private cache when an entry cannot be shared.
Transient vectors are local to a session.

The shared cache reduces duplicate graph copies across pooled
connections. It does not change graph freshness, PostgreSQL privileges,
or transaction semantics. See [operations](/pg_warren/docs/operations/) and
[the shared cache](/pg_warren/docs/limits/#the-shared-cache).

## Implementation boundaries

The Rust workspace separates a PostgreSQL-free domain crate,
`warren-core`, from the `pg_warren` extension adapter. The core owns graph
structures, traversal, compute kernels, communities, spectral work, and
version comparisons. The adapter handles PostgreSQL types, SQL entry
points, catalog persistence, access checks, and memory integration.

See [contributing](/pg_warren/docs/contributing/) for the build and test
workflow, and [the design specification](https://github.com/ExaDev/pg_warren/blob/main/DESIGN.md)
for detailed decisions and their rationale.
