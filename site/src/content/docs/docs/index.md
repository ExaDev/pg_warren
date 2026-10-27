---
title: Documentation
description: Install pg_warren, build a graph from existing tables, and query it with SQL.
editUrl: https://github.com/ExaDev/pg_warren/edit/main/site/src/content/docs/docs/index.md
---

pg_warren provides deep traversal and whole-graph computation inside
PostgreSQL, over nodes and edges in ordinary tables. Think pgvector, but for
graphs: a graph index and compute engine beside your data, queried through
SQL functions.

:::note[Development documentation]
These pages follow `main`. They can describe features and package channels
that are not in a published release yet. Check the [release notes](/pg_warren/docs/changelog/)
and the [published releases](https://github.com/ExaDev/pg_warren/releases)
when installing or upgrading.
:::

## Start with a working example

1. [Install the extension](/pg_warren/docs/installation/) for your PostgreSQL
   major and platform. PostgreSQL 15–18 are supported on Linux. Managed
   providers must adopt the extension before you can use it there.
2. [Create your first graph](/pg_warren/docs/quickstart/) from two small
   tables, run a breadth-first search, and compute PageRank.
3. Read [graphs and versions](/pg_warren/docs/semantics/) before connecting
   a changing application dataset. Queries read a built graph version;
   a later table change appears after a refresh.

## Choose the right query

For a shallow query with a small frontier, indexed joins or a recursive CTE
are usually the right starting point. pg_warren is useful when a traversal
touches many rows, or when a computation repeatedly processes the graph.
Frontier size matters more than hop count: three hops can mean a thousand
intermediate rows or a billion, depending on degree.

| Task | Read |
| --- | --- |
| Find neighbours, reachable nodes, paths, or typed walks | [Traversal](/pg_warren/docs/traversal/) |
| Rank nodes, work with vectors, or write iterative algorithms | [Compute](/pg_warren/docs/compute/) |
| Find communities or build community-level graphs | [Communities](/pg_warren/docs/communities/) |
| Compare a staged build before committing a data change | [Change detection](/pg_warren/docs/change-detection/) |
| Check a function's exact arguments and return type | [SQL reference](/pg_warren/docs/reference/) |
| Configure memory, the shared cache, or compute threads | [Settings and limits](/pg_warren/docs/limits/) |

Traversals that return nodes use external ids that you can join back to your
tables. Scalar lookups return values such as a degree or a reachability result.
Computations work with named vectors bound to a graph version;
their results can also be read as SQL rows. The functions compose with the
rest of a query.

## Understand the operating model

A graph is defined by a node query and an edge query. A refresh builds a
new immutable version, persisted transactionally in extension tables.
Versions can be staged, compared, promoted, retained, or dropped. The
tables remain the source of truth.

Read the [architecture overview](/pg_warren/docs/architecture/) for the
storage and computation model, and [operations](/pg_warren/docs/operations/)
for pooling, backups, upgrades, replicas, and troubleshooting.

## Evaluate performance

The [performance comparison](/pg_warren/docs/performance/) summarises measured
results against igraph and Neo4j, with and without GDS. The
[benchmark methodology and full results](/pg_warren/docs/benchmarks/) describe
the datasets, engine settings, correctness checks, and comparison limits.
Performance depends on the workload; the results include both wins and losses.

For development prerequisites, checks, and release procedures, see
[contributing](/pg_warren/docs/contributing/).
