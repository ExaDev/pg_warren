# Semantics

What a graph is, what a traversal sees, and who may do what. The examples
use the README's `social` graph (`people`, `follows`); they run as written in
`test/sql/docs_examples.sql`. `\gset` and `:name` are psql variables, used
to carry version ids between statements.

## Graphs

A **graph** is a name plus two SQL queries (signature, with defaults):

```
SELECT warren.create_graph(name, node_query, edge_query,
                           directed := true, weighted := false, has_relations := false,
                           build_reverse := true, retain_versions := 2);
```

- `node_query` returns one column, the node's **external id**: `integer`,
  `bigint` or `uuid`. Its type is the graph's **id type**.
- `edge_query` returns `src, dst` (of the id type), then a `text`
  **relation** if `has_relations`, then a `double precision` weight if
  `weighted`.
- `create_graph` checks both queries' columns without running them. Every
  `refresh` checks them again, so a source whose column types changed fails
  with `datatype_mismatch`.
- `directed := false` stores each edge in both directions with the same
  weight (a self-loop once); edge counts then count each edge twice.
- `build_reverse` (default true) also stores the reverse CSR. Without it,
  every function that takes `direction := 'in'` or `'both'` refuses it
  (`degree`, `vector_degree`, `degree_vector` and the traversals included),
  except `gather`, which pushes over the forward CSR instead; and some
  compute runs single-threaded ([compute.md](compute.md)).

Edges are cleaned at build time, and counted, never silently:

| What | Becomes | Counted in |
|---|---|---|
| NULL, unknown or removed endpoint; NULL relation or weight the graph needs | dropped | `dropped_edges` |
| Parallel edges (same src, dst, relation) | one edge, weights summed | `deduplicated_edges` |
| Self-loops | kept | |
| NULL node ids | ignored | |

## Versions

`warren.refresh(name)` runs both queries and builds a new **version**: an
immutable copy of the graph's structure. It returns the version's id.

- A version is **staged** until it is promoted. `refresh(name, promote :=
  false)` leaves it staged: fully built and queryable with `version := id`,
  but invisible to calls that use the default.
- `warren.promote(name, version)` makes a version **current**. A graph has
  at most one current version, and every function's `version DEFAULT NULL`
  means "the current version" (before the first promotion that is an error,
  `object_not_in_prerequisite_state`).
- Version ids are unique across all graphs and have gaps (a rolled-back
  build consumes one). `seq` in `info()` numbers a graph's versions 1, 2, ….
- `label` is free text, e.g. `refresh('kg', label := 'corpus-2026-10-17')`.

```sql
SELECT warren.refresh('social', promote := false, label := 'trial') AS staged \gset
SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 3, version := :staged);
SELECT warren.promote('social', :staged);

SELECT version, seq, staged, current, label, node_count, edge_count, vectors
FROM jsonb_to_recordset(warren.info('social') -> 'versions')
     AS v(version bigint, seq bigint, staged bool, current bool, label text,
          node_count bigint, edge_count bigint, vectors int);
```

`warren.info(name)` returns the graph's definition and every version with its
statistics: `node_count` (live nodes), `id_space`, `edge_count`,
`dropped_edges`, `deduplicated_edges`, `bytes`, `built_at`, `built_lsn`,
`staleness`, `compacted_from` and the number of `vectors` bound to it.
`warren.diff(name, v1, v2)` reports nodes and edges added and removed (per
relation) and the shift in the degree distribution.

Refreshes of one graph run one at a time: `refresh` locks the graph's catalog
row until the end of its transaction.

## Stable dense ids and compaction

Inside a version every node has a **dense id**, `0 … id_space − 1`.
`warren.id_map(name, NULL::<id type>)` lists them.

- The first build numbers nodes in id order.
- Later builds start from the current version's id map and keep every
  surviving node's dense id. New nodes are appended
  after the highest dense id. A node that disappears keeps its dense id as a
  **tombstone** (`live = false`); if it comes back, it gets the same dense id.
  Dense ids are never reused.
- So vectors from two versions along one line of versions line up row by
  row, which is what `vector_norm_diff` and `distance` rely on.
- The exception is **sibling staged versions**: two `refresh(promote :=
  false)` from the same current version each append their own new nodes, so
  the same new dense id can name different nodes. `diff`, `distance` and
  `vector_norm_diff` refuse such a pair (22023). Siblings that added the same
  new nodes, or none, compare.

```sql
DELETE FROM follows WHERE dst = 5;  DELETE FROM people WHERE id = 5;
SELECT warren.refresh('social');
SELECT * FROM warren.id_map('social', NULL::bigint) ORDER BY dense_id;
-- 1→0, 2→1, 3→2, 4→3 live; 5→4 tombstoned. A new node 6 would get dense id 5.
```

Tombstones cost a row in every vector and an offset in every CSR. When there
are many, `warren.compact(name)` renumbers the live nodes `0 … node_count − 1`
into a **new id space**, as a new, promoted version (`compacted_from` set),
and returns its id. Compaction is a boundary:

- Persisted vectors bound to the source version are **moved** to the
  compacted one, rows renumbered (a `NOTICE` names them). Transient vectors
  are not moved.
- The source version can no longer be promoted or refreshed from, and
  `diff`, `distance` and `vector_norm_diff` refuse to compare across the
  boundary (`invalid_parameter_value`). Later refreshes continue in the new id
  space.

Compact only when no cross-version comparison is pending.

## Staleness: the version versus the tables

A traversal or compute call reads a **version**, never the tables. Writes to
the source tables are invisible until the next `refresh` (and `promote`, if
staged):

```sql
INSERT INTO people VALUES (7, 'gus');  INSERT INTO follows VALUES (6, 7);
SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 10);   -- gus is not there yet
SELECT warren.refresh('social');                                    -- now he is
```

You choose how stale is acceptable and when to refresh (on a schedule, after
a batch load, from application code). pg_warren makes staleness observable:

- `info() ->> 'staleness'` is `now() − built_at` of the current version (and
  each version has its own). It measures age, not change: a version an hour
  old over tables nobody touched is not out of date.
- `built_lsn` is the WAL position when the version was built. Compared with
  `pg_current_wal_lsn()`, it says whether anything in the cluster has been
  written since: a cheap, coarse test.

```sql
SELECT warren.info('social') ->> 'staleness'                AS staleness,
       warren.info('social') #>> '{versions,-1,built_lsn}'  AS built_lsn,
       pg_current_wal_lsn()                                  AS now_lsn;
```

Which version is current is read at the start of every call, under the
statement's snapshot. So a promotion is seen by the next statement, and a
`REPEATABLE READ` transaction keeps seeing the version that was current when
its snapshot was taken. A set-returning call keeps the version it started
with until it finishes, even if that version is promoted away or evicted.

## Transactions

Versions, segments and persisted vectors are rows in ordinary extension
tables. They are WAL-logged, replicated to standbys, included in
`pg_dump`, and **transactional**:

- `refresh` runs both queries under the caller's snapshot, so it **sees the
  caller's uncommitted writes**.
- If the transaction rolls back, the version and every persisted vector written
  in it disappear, with nothing to clean up.

```sql
BEGIN;
INSERT INTO people VALUES (8, 'hal');  INSERT INTO follows VALUES (7, 8);
SELECT warren.refresh('social', promote := false) AS trial \gset
SELECT count(*) FROM warren.bfs('social', ARRAY[1::bigint], 10, version := :trial);  -- sees hal
ROLLBACK;                                                          -- version :trial is gone
```

This is what makes [change detection](change-detection.md) possible.
Two exceptions:

- **Transient vectors** (`persist := false`, [compute.md](compute.md)) live
  in the backend's memory and are not transactional: a rollback does not
  remove one written in it, nor restore a transient vector it replaced. A
  transient write over a *persisted* name deletes the persisted row, and
  that delete does roll back: the persisted vector is restored, but stays
  hidden behind the transient one until `drop_vector` drops the transient
  one.
- The backend-local cache is not transactional either, but it holds only
  immutable versions keyed by version id, so it is never wrong; entries for
  versions that no longer exist are dropped at the next load.

## Retention

Each promotion (`refresh` with `promote`, or `promote`) applies retention:

- The current version and the `retain_versions − 1` newest other promoted
  versions are kept; older promoted versions are deleted with their segments.
- **Staged versions are never deleted** automatically. Drop the ones you do
  not promote with `warren.drop_version(name, version)`.
- **A version with persisted vectors bound to it is kept**, with a `NOTICE`
  (`version 1 of graph "social" is kept beyond retain_versions: 1 vector is
  bound to it`). This keeps a version somebody is comparing against. Drop
  the vectors (`drop_vector(…, version := …)`) or the version to release it.
  Eigenvectors stored by `eigen` count, so a version you ran `eigen` on stays
  until you drop them.
- Transient vectors never pin a version.
- After `compact`, the source's vectors have moved, so it is no longer
  pinned: retention deletes it once it falls outside `retain_versions` (with
  the default of 2, the source is the one other version kept, until the next
  promotion).

`drop_version` deletes any version except the current one, vectors included.
`drop_graph` deletes the graph and everything built from it.

## Access

A graph exposes topology computed with its **owner's** visibility, like a
materialised view. Row-level security on the source tables does not apply to
what a traversal reveals. So access is per graph:

| Role | May |
|---|---|
| Owner (the role that ran `create_graph`, and its members) | everything: `refresh`, `promote`, `drop_version`, `compact`, `drop_graph`, `grant`, `revoke`, and all reads |
| Reader (a role in `readers`, and its members) | traversal, compute, `info`, `diff`, `distance`, `eigen`, `prewarm`, and writing vectors |
| Anyone | `evict` (it frees only the caller's memory), `last_stats`, `version` |

```sql
SELECT warren.grant('social', 'analyst');     -- owner only
SELECT warren.revoke('social', 'analyst');
```

Everyone else gets `insufficient_privilege` (42501). The `warren.*` tables
are revoked from `PUBLIC`; all access goes through the functions. Ownership
cannot be transferred.

**Vectors.** Readers share one name space of vectors per version. A persisted
vector may be replaced or dropped only by the role that wrote it or by the
graph's owner. Transient vectors are private to the role that wrote them.

**Whose SQL runs as whom, under which `search_path`.**

| SQL | Runs as | `search_path` |
|---|---|---|
| `node_query`, `edge_query` (in `refresh`) | the graph owner | the creator's, captured at `create_graph` (`info() ->> 'search_path'`) |
| `vector_from_query` and `gather` queries | the caller | the caller's |
| `vector_map` expressions (on the executor path) | the caller | the caller's |
| pg_warren's own catalog SQL | the extension owner | pinned to `pg_catalog, pg_temp` |

So a caller cannot redirect a graph's unqualified table names with their
own `search_path`, and no user SQL runs with elevated rights.

Source queries, `vector_from_query`'s query, `gather`'s messages and
`vector_map` expressions on the executor path run in
Postgres's **read-only transaction state**, as under `SET TRANSACTION READ
ONLY`. A function they call cannot write (25006), `nextval` on a
non-temporary sequence is refused the same way, and a data-modifying CTE is
refused (0A000). The library functions call `vector_map`, so the same holds
for them. This guards the integrity of what a build or kernel reads;
it is **not a sandbox**. Writes to existing temporary tables, `pg_notify`,
`LOCK`, `COPY … TO PROGRAM` for roles allowed it, and writes over another
connection (`dblink_exec`) still work. None of these gains privilege, since
the SQL runs as the caller (or, for source queries, as the owner). Schema-qualify
names in source queries if you will change the creator's `search_path`
later. Functions are `SECURITY INVOKER`.

## Id types and type witnesses

Functions are polymorphic over the id type and check it at run time.

- **An id argument must have exactly the graph's id type**, because the
  result type comes from it: on a `bigint` graph write `ARRAY[1::bigint]`
  and `3::bigint`, not `1` (`datatype_mismatch`, 42804, names the cast).
- Lookups that return no ids (`degree`, `has_edge`, `reachable`) widen an
  `integer` or `smallint` argument, so `warren.degree('social', 3)` works.
- Functions that return ids but take none (`components`, `vector_values`,
  `matrix_rows`, `vector_topk`, `id_map`, `gather`) take a **type witness**,
  a typed NULL: `warren.components('social', NULL::bigint)`. `gather` takes a
  second witness for its result type.
- `uuid` graphs work the same way (`ARRAY[$1::uuid]`, `NULL::uuid`).

```sql
SELECT warren.neighbours('social', 3::bigint);
SELECT warren.degree('social', 3);
SELECT * FROM warren.components('social', NULL::bigint);
SELECT * FROM warren.vector_values('social', 'pr', NULL::bigint);   -- after pagerank('social', 'pr')
```

## Unknown and tombstoned ids

An external id the version does not have, or has only as a tombstone, is
**not a node of that version**. It is not an error, because ids come and go
between versions, as a missing join partner does in SQL:

| Where | Effect |
|---|---|
| A start, a `node_filter` or step `filter` member | skipped (NULL too) |
| `neighbours` / `degree` | empty / 0 |
| `has_edge`, `reachable` | false (an unknown node does not reach itself) |
| `shortest_path` / `dijkstra` | NULL / no row |
| `vector_from_query`, `gather` message rows | skipped |
| `sssp_vector` source | error (22023) |

Relation names a version does not have are skipped likewise; `rels :=
'{}'` selects nothing. Rows read back from vectors (`vector_values`,
`vector_topk`, `vector_stats`, `vector_reduce`) cover live nodes only; a
tombstoned node's row holds zero.

## Parallel query labels

pg_warren's cache is per backend: a parallel worker would start with an
empty one (and cold-load the whole version), could not see transient
vectors, and could not hand `last_stats()` back. So:

| Label | Functions |
|---|---|
| `PARALLEL RESTRICTED` (run in the leader, never in a worker) | everything that reads a graph or a vector: traversal, `info`, `diff`, `distance`, `prewarm`, `evict`, `last_stats`, `vector_values`, `matrix_rows`, `vector_topk`, `vector_stats`, `vector_reduce`, `vector_norm_diff`, `id_map`, `explain_map` |
| `PARALLEL UNSAFE` (no parallel plan) | everything that writes or may persist: graph management, the vector constructors, `spmv`, `spmm`, `vector_map`, `gather`, `triangles`, `eigen`, the library, `drop_vector` |
| `PARALLEL SAFE` | `version`, `s`, `steps` (they read no graph) |

Restricted functions work inside parallel plans: the rest of the query runs
in parallel, and the warren call runs in the leader.
