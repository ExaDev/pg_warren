---
title: Operations
description: Run pg_warren with connection pools, bounded memory, upgrades, backups, and physical replicas.
editUrl: https://github.com/ExaDev/pg_warren/edit/main/site/src/content/docs/docs/operations.md
---

This guide connects the operating procedures documented in
[installation](/pg_warren/docs/installation/),
[graph semantics](/pg_warren/docs/semantics/), and
[settings and limits](/pg_warren/docs/limits/).

:::note[Development documentation]
This page follows `main`. Read the [release-specific upgrade notes](/pg_warren/docs/changelog/)
before installing a new software release; restart and graph-refresh
requirements can change between releases.
:::

## Configure the shared cache

The shared cache holds graph versions and persisted vectors once per
cluster, rather than once per backend. It is enabled by default with a
`warren.shared_cache_memory` limit of `4GB`. PostgreSQL 15 and 16 need
`pg_warren` in `shared_preload_libraries` and a server restart to share
loaded graphs. PostgreSQL 17 and 18 can use it without preloading.

If shared memory cannot hold an entry, the backend falls back to its
private cache. Notices depend on the cause and whether the cache cap was
configured explicitly; some default fallbacks are silent. In containers, size `/dev/shm` for
the intended shared cache; Docker's default of 64 MB is small for graph
workloads. The installation instructions explain `--shm-size` and the
required server configuration.

For cache inspection, eviction, prewarming, and fallback behaviour, read
[the shared cache](/pg_warren/docs/limits/#the-shared-cache). Exact
introspection signatures are in the [SQL reference](/pg_warren/docs/reference/).

## Budget total memory

There are separate budgets for the shared cache, the private cache of
each backend, builds, and individual computations. A `4GB` setting is
not a `4GB` limit on the whole PostgreSQL process or cluster.

Concurrent backends can each hold private graph copies, transient vectors,
and compute scratch. Several simultaneous builds or computations add to
the cluster's memory demand. Start with the
[measured sizing examples](/pg_warren/docs/limits/#sizing) and account for
the number of active connections as well as graph size.

A version that cannot remain in a private cache may be held for one
statement and reloaded by the next. Frequent reloads indicate a working
set that does not fit, rather than a reason to refresh the graph again.
See [the memory model](/pg_warren/docs/limits/#memory-model).

## Use connection pools deliberately

Persisted graph versions and vectors are available to authorised readers
from other connections. Transient vectors belong to the session that
created them. Keep a multi-statement program that uses transient vectors
on one backend; transaction pooling only keeps that association for the
duration of a transaction.

In transaction pooling, create, read, and explicitly drop transient vectors
with `warren.drop_vector` within the same transaction. They are session state,
so a transaction boundary alone does not clear them. Leftover vectors can
remain on a backend for a later client of the same role. Use `SET LOCAL` for
per-transaction settings, and read `last_stats()` in the traversal's transaction.

`DISCARD ALL` resets pg_warren session state, including transient vectors
and `last_stats()`. If a pool runs it between clients, do not rely on
transient data surviving the handoff. Cached graph versions and persisted
vectors remain available for reuse.

Use the introspection functions `warren.backends()`, `warren.cache()`,
`warren.shared_cache()`, and `warren.progress()` to inspect activity and
loaded state. For the actual return columns and access rules, use the
[SQL reference](/pg_warren/docs/reference/) and
[shared-cache documentation](/pg_warren/docs/limits/#the-shared-cache).

## Plan refreshes and retention

Graph queries read the current graph version unless given an explicit
version. Table changes do not appear until a refresh. Choose refresh
timing to match the freshness your application needs, and make that
choice visible to readers.

A staged version is queryable by its explicit id while ordinary reads
continue to use the current version. Build, compare, and promote when
appropriate. A staged build in a long transaction can retain an old
snapshot and delay PostgreSQL cleanup; see the
[long-transaction caveat](/pg_warren/docs/change-detection/#the-long-transaction-caveat).

Review [retention](/pg_warren/docs/semantics/#retention), especially for
staged versions, vectors, and community lineage that can keep older data
referenced. Compaction changes the id space; it is not a substitute for a
refresh when the source tables have changed.

## Upgrade the software

Install the new build and follow that release's restart instructions.
Run `ALTER EXTENSION pg_warren UPDATE` in each database that has the
extension, before calling its functions with the new library.

Some releases also require refreshing graphs or recreating vectors due
to stored-format changes. The development `0.7.0` notes, for example,
describe a new CSR segment format, shared-cache registry changes,
function dependencies during the extension update, and a required refresh
of every graph. Do not assume an extension update alone converts stored
versions. Follow the [changelog](/pg_warren/docs/changelog/) for the exact
software release you are installing.

## Back up and restore

Graph definitions, versions, and persisted vectors are stored in
extension tables. A superuser's `pg_dump` includes them. Create the
owning and reading roles on the destination before restoring.

Restore normally. PostgreSQL can warn about circular foreign-key
constraints on `warren.versions` because it references itself; the
documented restore procedure does not disable pg_warren's integrity
checks.

A non-superuser cannot read the catalog tables and therefore cannot dump
their data. Either dump as a superuser, or use the documented
`--exclude-table-data='warren.*'` option and rebuild graphs after the
restore. See [installation](/pg_warren/docs/installation/) for the full
procedure and its access implications.

## Read from physical replicas

Physical streaming replication carries persistent catalog, graph, and
vector data to standbys. Each server has its own caches. A standby can
read versions it has replayed and run computations that do not persist
results; functions that would write the catalog fail in its read-only
transaction state.

Algorithms that always persist their output cannot run on a standby just
because their input is already cached. The PL/pgSQL library functions
such as `pagerank` and `louvain` persist their output.

Standby queries can conflict with cleanup replay when a version is retired
on the primary. This follows PostgreSQL's snapshot and recovery-conflict
rules. See [the standby design](https://github.com/ExaDev/pg_warren/blob/main/DESIGN.md#51-catalog)
for replay, cancellation, and promotion behaviour.

## Diagnose a refused or unexpectedly large query

| Symptom | First place to look |
| --- | --- |
| `53400` memory refusal | The error's named budget and [memory model](/pg_warren/docs/limits/#memory-model) |
| Repeated private-cache loads | Graph and vector working set, cache sizing, and [shared-cache fallback](/pg_warren/docs/limits/#the-shared-cache) |
| Fewer walk rows than expected | Truncation notices, `max_results`, `warren.max_walk_paths`, and `last_stats` in [traversal](/pg_warren/docs/traversal/#last_stats) |
| A new row is absent from a traversal | [Graph freshness](/pg_warren/docs/semantics/#staleness-the-version-versus-the-tables) and the version being read |
| `LIMIT` does not bound traversal work | Set-returning function placement and [known limitations](/pg_warren/docs/limits/#known-limitations) |
| An id argument or result type fails | [Id types and type witnesses](/pg_warren/docs/semantics/#id-types-and-type-witnesses) |
| A role cannot access a graph | Graph ownership and grants in [access](/pg_warren/docs/semantics/#access) |

Long calls check for interrupts. Use PostgreSQL's `statement_timeout` or
query cancellation to stop work. A depth or result bound limits traversal
work more predictably than applying an outer `LIMIT` after a function in
`FROM` has run to completion.
