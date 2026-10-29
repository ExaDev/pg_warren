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
                           build_reverse := true, retain_versions := 2,
                           store_merged := true, node_order := 'id');
```

- `node_query` returns one column, the node's **external id**: `integer`,
  `bigint`, `uuid` or `text` (`varchar` is read as `text`). Its type is
  the graph's **id type** ([text ids](#text-ids)).
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
- `store_merged` (default true) also stores the merged CSR of a graph with
  several relations, which reads over all relations use. Without it they
  merge the relations' rows on the fly: same results, less memory, slower
  ([limits.md](limits.md#the-merged-csr-store_merged)).
- `node_order` (default `'id'`) is how dense ids are numbered where they
  may change: `'id'` in id order, `'locality'` so that neighbours get
  nearby dense ids, which makes large graphs' kernels faster
  ([limits.md](limits.md#node-order-locality)). Results are the same.
  Until a graph is first promoted, every `refresh` is a first build, so
  each staged version of a never-promoted `'locality'` graph gets its own
  order, from its own edges; `diff`, `distance` and `vector_norm_diff`
  refuse two of them whose orders differ (22023, as for sibling staged
  versions, below).

Edges are cleaned at build time, and counted, never silently:

| What | Becomes | Counted in |
|---|---|---|
| NULL, unknown or removed endpoint; NULL relation or weight the graph needs | dropped | `dropped_edges` |
| Parallel edges (same src, dst, relation) | one edge, weights summed | `deduplicated_edges` |
| Self-loops | kept | |
| NULL node ids | ignored | |

### A pair in several relations

Two nodes can be linked in several relations at once, `1 → 2` as both
`phone` and `address`, or two towns by both `road` and `rail`. Functions
whose result this can change take `per_relation boolean DEFAULT false`.
What the default does depends on what the function reads weights as:

> **Strengths add; costs don't.** A pair linked by phone (2) and by
> address (3) is linked with strength 5. A road of 5 km and a railway of
> 3 km are a route of 3 km, not 8: two parallel routes are not a longer
> route than either.

| The function reads weights as | Functions | By default, a pair in several relations is | `per_relation := true` |
|---|---|---|---|
| **strengths** | `spmv`, `spmm`, `pagerank`, `katz_centrality`, `eigenvector_centrality`, `hits`, `modularity`, `louvain`, `leiden`, `local_moving`, `leiden_refine`, `contract`, `eigen`, `distance`, `degree`, `vector_degree`, `degree_vector`, `aggregate_neighbours`, `gather`, `label_propagation_majority` | one edge, its relations' weights **summed** (1 when unweighted) | an edge in each relation |
| **costs** (lengths) | `dijkstra`, `sssp_vector`, `betweenness_centrality`, `closeness_centrality`, `harmonic_centrality`, `minimum_spanning_forest` | one edge, its **cheapest** relation's weight | the same result |
| **costs** (lengths), longest | `dag_longest_path` | one edge, its **heaviest** relation's weight | the same result |

With `road` 5 and `rail` 3 from town 1 to town 2:

```sql
SELECT cost FROM warren.dijkstra('towns', 1, 2);       -- 3: the railway
SELECT cost FROM warren.dijkstra('towns', 1, 2, per_relation := true);  -- 3 as well
SELECT warren.spmv('towns', 'x', 'y');                  -- A[1][2] = 8: strengths add
SELECT warren.pagerank('towns', 'pr');                  -- 1 → 2 weighs 8
```

For the cost functions `per_relation := true` reads the pair as two
parallel edges, a road of 5 and a railway of 3, and a shortest path takes
the cheaper of them (a longest path the heavier), so both values give the
same result: a parallel edge is never an extra shortest path either. They
take the argument anyway, so `per_relation` means one thing everywhere,
and refuse NULL as the others do. Unweighted, every edge costs 1 and a pair
is one hop however many relations link it, so nothing changes there.

For the strength functions, **the default is the simple graph** of
distinct pairs, what topology wants, and **`per_relation := true` makes
the pair an edge in each relation**, each with its relation's weight (1
when unweighted), as if they were parallel edges: an unweighted pair in
three relations weighs 3 in modularity, and a sum of weights is the same
either way. Use it when the multiplicity is the signal, such as two
accounts sharing a phone, an address and a device.

`spmv` and `spmm` are primitives over the matrix `A`, whose entry is the
pair's summed weight whatever the semiring, so `min_plus` reads 8 for the
pair above by default; pass `per_relation := true` to have its `min` take
the cheapest relation (`sssp_vector` does this itself).

`rels` restricts both readings to the selected relations. Through 0.6.0
the community functions counted such a pair once per relation and the
rest once; `dijkstra` took the cheapest relation, as it still does, but
`sssp_vector`, the shortest-path centralities, `dag_longest_path` and
`minimum_spanning_forest` summed the weights. See DESIGN.md §5.3 for the
whole table.

| Functions | `per_relation` |
|---|---|
| `modularity`, `louvain`, `leiden`, `local_moving`, `leiden_refine` | yes |
| `contract` | yes: counting (`weighted := false`, or an unweighted graph), the default counts the pair once, in the first of its relations; per relation in each. Weights are summed either way |
| `spmv`, `spmm` | yes: an entry in each relation, which `⊕` folds (`min_plus` then takes the cheapest) |
| `pagerank`, `katz_centrality`, `eigenvector_centrality`, `hits` | yes: unweighted, the pair weighs k |
| `sssp_vector`, `dijkstra` | accepted, same result: the cheapest of its relations either way |
| `betweenness_centrality`, `closeness_centrality`, `harmonic_centrality` | accepted, same result: weighted, paths take the cheapest of its relations either way; unweighted it changes nothing |
| `dag_longest_path` | accepted, same result: weighted, the heaviest of its relations either way |
| `minimum_spanning_forest` | accepted, same result: weighted, the lightest of its relations either way |
| `degree`, `vector_degree`, `degree_vector` | yes: a neighbour once in each relation linking it (with `direction := 'both'`, once per relation even if linked both ways) |
| `aggregate_neighbours` | yes: a neighbour's row once in each relation linking it |
| `eigen`, `distance` (`spectral`, `degree`) | yes: unweighted, a pair weighs the number of its relations; `distance('edit')` and `diff` already count (relation, source, target) |
| `gather` | yes: a sender once in each relation linking it, so its messages arrive once per relation ([compute.md](compute.md#gather)) |
| `label_propagation_majority` | yes: a neighbour votes once in each relation linking it |

The rest take no `per_relation`, because the count cannot change what
they return: reachability and hop counts (`bfs`, `reachable`,
`reachable_set`, `shortest_path`, `neighbours`, `has_edge`), components
and orders (`components`, `components_vector`, `strong_components_vector`,
`topological_order`), `label_propagation` (a minimum), and the functions
defined on the simple graph of distinct neighbours, as NetworkX defines
them (`triangles`, `clustering`, `core_number`, `similarity`,
`most_similar`, `similar_pairs`). `walk` already names each hop's
relation, so a pair in k relations is k walks.

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

- The first build numbers nodes in id order, or, with `node_order :=
  'locality'`, so that neighbours get nearby dense ids.
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

`warren.compact(name, node_order := 'locality')` (or `'id'`) compacts in
that order, which becomes the graph's; NULL, the default, keeps the
graph's order. Compacting is how an existing graph gets locality order, and
how a locality graph gets it back after many refreshes have appended nodes.

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
| Owner (the role that ran `create_graph`, or was given the graph with `set_owner`, and its members) | everything: `refresh`, `promote`, `drop_version`, `compact`, `drop_graph`, `grant`, `revoke`, `set_owner`, `set_vector_quota`, and all reads |
| Reader (a role in `readers`, and its members) | traversal, compute, `info`, `diff`, `distance`, `eigen`, `prewarm`, `vector_usage`, and writing vectors (within its quota) |
| Anyone | `evict` (it frees only the caller's memory), `last_stats`, `version` |

```sql
SELECT warren.grant('social', 'analyst');     -- owner only
SELECT warren.revoke('social', 'analyst');
```

Everyone else gets `insufficient_privilege` (42501). The `warren.*` tables
are revoked from `PUBLIC`; all access goes through the functions.

**A graph a superuser owns is refreshed only by a superuser.** Its source
queries would run as that superuser, so `refresh` requires that the caller
is a superuser, both as the current user and as the session user: a
member of the superuser role (superuser is not inherited, even after `SET
ROLE`) and a superuser's `SECURITY DEFINER` function called from another
role's session are refused with 42501, with a hint. The same holds for
`set_owner` to a superuser, which opens the queries as the new owner. To
let other roles refresh such a graph, give it to a role that is not a
superuser (`set_owner`, `reassign_owned`). This keeps a role that can
write the catalog directly (`pg_write_all_data`, as a restore uses) from
running SQL as a superuser through a graph (docs/security.md, "Dumps and
restores"; DESIGN.md §7).

**Ownership transfer.** `set_owner` gives a graph a new owner, as `ALTER
MATERIALIZED VIEW … OWNER TO` does a materialised view:

```sql
SELECT warren.set_owner('social', 'analytics');   -- owner, and may SET ROLE analytics
```

- The caller must be a member of the current owner, and able to `SET ROLE`
  to the new one: on PostgreSQL 16+, a membership with the `SET` option; on
  15, a membership. A superuser always may. Otherwise 42501.
- The source queries are opened (not run) as the new owner, under the
  graph's `search_path`, as `refresh` will run them. A new owner that cannot
  read them is refused with 42501, and queries whose columns no longer fit
  the graph with 42804, before anything changes. `create_graph`'s `NOTICE`
  about writable schemas on the `search_path` is raised again, judged for
  the new owner. A graph derived by `contract` has no source queries.
- Built versions and vectors stay as they are: they were computed with the
  old owner's visibility, until the next `refresh` (as a materialised view
  keeps its data). Readers are kept, except the new owner, which no longer
  needs to be one. The old owner keeps no access unless it is granted.
  Vectors keep the role that wrote them; the new owner may replace or drop
  any of them.
- It waits for a running `refresh`, `compact`, `promote`, `drop_version`,
  `grant`, `revoke` or `drop_graph` of the graph, and they wait for it. Each checks ownership again once it has
  the graph's row, so a call queued behind a transfer by a role that no
  longer owns the graph is refused (42501).

**Dropping a role.** A graph's owner, its readers and a vector's creator
are recorded as role OIDs, which `DROP ROLE` cannot see: it neither refuses
nor cleans up, and a role created later may receive a dropped role's OID,
and with it what that OID still names. Postgres's procedure for dropping a
role that owns objects is `REASSIGN OWNED` then `DROP OWNED` in each
database, then `DROP ROLE`; but `REASSIGN OWNED` and `DROP ROLE` run no
extension code, so `REASSIGN OWNED` moves no graph. pg_warren has SQL
counterparts of the two per-database commands:

```sql
SELECT warren.reassign_owned('alice', 'bob');  -- alice's graphs and vectors become bob's
SELECT warren.drop_owned('alice');             -- drops alice's graphs, and the rest below
```

- `reassign_owned(old, new)` gives every graph `old` owns to `new` with
  `set_owner`'s rules, except that, like `REASSIGN OWNED`, it opens no source
  query (the writable-schema `NOTICE` is still raised), and makes `new` the
  creator of every vector `old` wrote. The vectors moved count against
  `new`'s quotas on readers' vectors as `new`'s own writes would: a move
  that would take `new` past its quota on a graph is refused (53400), and
  nothing moves. `old`'s reader entries stay, as its privileges do under
  `REASSIGN OWNED`. It returns the number of graphs moved.
- `drop_owned(role)` drops every graph `role` owns (as `drop_graph` does,
  with their versions and everyone's vectors), deletes the vectors it wrote
  on other graphs, and removes it from every graph's readers and its
  quotas on graphs whose owner the caller is a member of. So a role
  cannot shed a quota another owner set on it by dropping what it owns;
  the graph's owner, a superuser or the extension's owner can, and once the
  role is dropped, `orphans()` lists the quota. It returns the number of
  graphs dropped.
- Both need the privileges of the role they move from or drop (a member
  that does not inherit them has not), as the commands do, and
  `reassign_owned` also the right to `SET ROLE` to the new owner (on
  PostgreSQL 15, membership): otherwise 42501.

**In each database that has pg_warren, before `DROP ROLE alice`:**

```sql
REASSIGN OWNED BY alice TO bob;
SELECT warren.reassign_owned('alice', 'bob');   -- or warren.drop_owned('alice')
DROP OWNED BY alice;
-- then, once, in any database: DROP ROLE alice;
```

**The optional event trigger.** A superuser may make `DROP OWNED` itself
take care of graphs, in each database that has pg_warren:

```sql
CREATE EVENT TRIGGER warren_drop_owned ON ddl_command_start
    WHEN TAG IN ('DROP OWNED') EXECUTE FUNCTION warren.on_drop_owned();
```

With it, `DROP OWNED BY alice` is refused (2BP01, as `DROP ROLE` refuses
while a role owns objects) while `alice` owns graphs, naming them. So the
standard procedure, run without `warren.reassign_owned`, stops rather than
leaving graphs behind (`REASSIGN OWNED` did not move them): run
`warren.reassign_owned` or `warren.drop_owned`, then `DROP OWNED` again.
When `alice` owns no graph, `DROP OWNED` also removes her reader entries
and quotas (those `drop_owned` would remove) and deletes the vectors she wrote, as `drop_owned` does. A role that does
not exist, or whose privileges the caller lacks, is left to `DROP OWNED`'s
own error. The trigger reads who owns what without waiting for other
transactions: a `set_owner` or `reassign_owned` to the role still
uncommitted when `DROP OWNED` runs is not seen, so its graph is orphaned at
`DROP ROLE`, and `orphans()` lists it (run the sweep after such overlaps).
The extension never creates the trigger, and nothing needs it:
only a superuser can create an event trigger, and a trusted install works
without it. While it exists, `DROP EXTENSION pg_warren` needs `CASCADE`,
which drops the trigger too. `pg_dump` dumps it, so restoring such a dump
as a non-superuser fails at its `CREATE EVENT TRIGGER` (and `ALTER EVENT
TRIGGER … OWNER TO`) statements; everything else restores. Event triggers
do not fire in single-user mode, nor with `event_triggers = off`
(PostgreSQL 17+). Calling `warren.on_drop_owned()` directly is refused
(39P03).

**After a `DROP ROLE` that skipped all this** (or ran before the trigger
existed), sweep:

```sql
SELECT * FROM warren.orphans();       -- (kind, graph, role): what names a dropped role
SELECT warren.drop_orphans();         -- drops orphaned graphs, prunes the rest
```

`orphans()` lists graphs whose owner no longer exists (`kind` `graph`),
quotas (`quota`) and reader entries (`reader`) naming dropped roles, and
vectors whose creator was dropped (`vector`, one row per graph and role),
each with the dropped role's OID. A superuser and the extension's owner
(the role that ran `CREATE EXTENSION`: the installer, in a trusted
install), or a member that has its privileges or may `SET ROLE` to it, see
every row; others see the `quota`, `reader` and `vector` rows of graphs
they own. `drop_orphans()` drops the orphaned graphs, removes the quotas
and reader entries, deletes the vectors, and returns the number of graphs
dropped. Run it
soon: once a dropped role's OID is reused, nothing can tell its graphs
from the new role's.

Who may act on a graph whose owner was dropped (an *orphan*): a superuser
or the extension's owner (as above) may drop it (`drop_graph`,
`drop_orphans`); only a superuser may give it a new owner with
`set_owner` (adopt it), since its versions were built with what the
dropped owner could see (42501 for anyone else). The extension's owner
could drop every graph with `DROP EXTENSION` anyway, so dropping an orphan
gives it nothing new. Every other call on an orphan fails (42704).

**Vectors.** Readers share one name space of vectors per version. A persisted
vector may be replaced or dropped only by the role that wrote it or by the
graph's owner. Transient vectors are private to the role that wrote them.

**Quotas on readers' vectors.** Persisted vectors are stored in the
catalog owner's tables, so a graph's owner may bound what each other role
persists on the graph:

```sql
SELECT warren.set_vector_quota('social', 100000000);             -- default for every role: 100 MB
SELECT warren.set_vector_quota('social', 1000000000, 'analyst'); -- analyst's own: 1 GB
SELECT warren.set_vector_quota('social', NULL, 'analyst');       -- analyst back to the default
SELECT warren.set_vector_quota('social', NULL);                  -- no default
SELECT * FROM warren.vector_usage('social');                     -- (role, vectors, bytes, quota)
```

- A role's usage is the elements' bytes (`length × width × size(dtype)`:
  8 for `f64` and `i64`, 4 for `f32`, 1 for `bool`) of every persisted
  vector it wrote on the graph, across all versions: what the vectors hold
  when loaded, not their storage on disk. Transient vectors are not
  counted (`warren.max_cache_memory` bounds them per backend).
- Every persisted write (`persist := true`, the default, of every
  vector-writing function, and the library's results) is refused with
  53400 (`configuration_limit_exceeded`) if it would take the writer's
  usage past its quota *and* above what it was. Replacing a vector with one
  no larger always works, and a quota lowered below what is in use blocks
  only growth. The message names the sizes and the remedies: drop vectors,
  write with `persist := false`, or ask the owner.
- The quota is the role's own, else the graph's default (`info`'s
  `reader_vector_quota`); none applies to members of the owner role, which
  pays for its own storage. A vector counts against the role that wrote it,
  and `reassign_owned` moves it to the new role, within that role's quota. `set_vector_quota` is for
  the owner. `vector_usage` shows the owner every role that has vectors or
  a quota of its own, and a reader its own row.
- One role's writes to one graph run one at a time, under a quota or not:
  each takes a lock on (graph, role) until its transaction ends (and
  `reassign_owned` takes the new role's), and counts what is
  committed when it has it, whatever its isolation level, so two sessions
  of one role cannot both fit under the quota. The lock shows in
  `pg_locks` as `advisory`, with `objsubid` 22353 (applications' advisory
  locks have 1 or 2), and `pg_advisory_unlock_all()` does not release it.
- Not counted: `compact` moving vectors to its new version, drops and
  retention. Quotas are dumped with the graph and dropped with it, and
  `drop_owned` (and the `DROP OWNED` trigger) remove a role's where the
  caller may (above).
- A quota bounds what a role stores on *another owner's* graph, and only
  that. It is not a bound on the catalog's total storage: any role that
  may create a graph stores what it likes on its own graphs, including the
  quotient `contract` makes for it, which it owns. A quotient does not
  inherit its parent's quotas; its owner sets its own.

**Whose SQL runs as whom, under which `search_path`.**

| SQL | Runs as | `search_path` |
|---|---|---|
| `node_query`, `edge_query` (in `refresh`) | the graph owner (a superuser only for a superuser) | the creator's, captured at `create_graph` (`info() ->> 'search_path'`) |
| `vector_from_query` and `gather` queries | the caller | the caller's |
| `vector_map` expressions (on the executor path) | the caller | the caller's |
| pg_warren's own catalog SQL | the catalog owner: the role that owns the `warren.*` tables (the installing superuser, or the bootstrap superuser in a trusted install) | pinned to `pg_catalog, pg_temp` |

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
- `text` graphs too (`'https://example.org/a'::text`, `NULL::text`): a
  lone string literal needs its `::text`, since Postgres cannot resolve an
  untyped literal to `anyelement`, but `ARRAY['…']` is already `text[]`.
  A `varchar` argument is widened where the result holds no ids.

```sql
SELECT warren.neighbours('social', 3::bigint);
SELECT warren.degree('social', 3);
SELECT * FROM warren.components('social', NULL::bigint);
SELECT * FROM warren.vector_values('social', 'pr', NULL::bigint);   -- after pagerank('social', 'pr')
```

### Text ids

- Ids compare **bytewise** in the database encoding, which is `COLLATE "C"`
  order, whatever the column's collation. Under a deterministic collation
  (every default one) that is SQL's own equality, so an edge joins the
  node SQL would join it to. A nondeterministic collation (for example
  ICU's case-insensitive ones) makes `'Alice' = 'alice'`, which bytewise
  ids cannot follow, so `create_graph` and `refresh` refuse such an id
  column (`datatype_mismatch`, 42804): write `id COLLATE "C"` for exact
  ids, or normalise in the query (`lower(id)`). `bpchar`, `name` and
  `citext` ids are refused too, each with the cast to write.
- The first build numbers dense ids in byte order (`node_order := 'id'`).
- Ids have no length limit beyond memory; the empty string is an id, and a
  NULL id is skipped, as for every id type.
- A version records the database encoding, and one restored into a
  database of another encoding is refused (55000): create the graph again
  there.
- A text graph's id map holds each id's bytes once plus 12 bytes a node
  (at 10M nodes, 120 MB plus the ids; `bigint` takes 240 MB). Calls that
  map more than 65,536 ids at once (`vector_from_query`, `gather`'s
  messages, `similarity`'s pairs) build a hash index of 8 bytes a node for
  the call, charged to `warren.max_compute_memory`, and search without it
  if it does not fit.

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

A parallel worker is a process of its own. With the shared cache it maps
the version the leader reads; wherever the version is read privately
(below) it loads its own copy. A worker cannot see the leader's transient
vectors, and what it records in `last_stats()` would never reach the
leader. So:

| Label | Functions |
|---|---|
| `PARALLEL SAFE` (may run in a worker) | the graph reads that use no vector: `neighbours`, `degree`, `has_edge`, `id_map`, `info`, and the traversals `reachable`, `shortest_path`, `dijkstra`, `bfs`, `reachable_set`, `components`, `walk`; and `version`, `s`, `steps`, `shared_cache`, `orphans`, which read no graph |
| `PARALLEL RESTRICTED` (run in the leader, never in a worker) | the vector reads (`vector_values`, `matrix_rows`, `vector_topk`, `vector_stats`, `vector_reduce`, `vector_norm_diff`, `modularity`, `minimum_spanning_forest`, `similarity`, `most_similar`, `similar_pairs`), `explain_map`, `diff`, `distance`, `prewarm`, `evict`, `last_stats` |
| `PARALLEL UNSAFE` (no parallel plan) | everything that writes or may persist: graph management, the vector constructors, `spmv`, `spmm`, `vector_map`, `gather`, `triangles`, the structural kernels that write a vector (`core_number`, `strong_components_vector`, `topological_order`, `dag_longest_path`), `eigen`, the library, `drop_vector` |

So a scan that calls `has_edge` or `degree` per row can run in parallel,
each worker calling it on its share of the rows (over 1M rows, with the
shared cache, 2.6–2.8× faster with 2 workers and 3.2–3.6× with 4:
docs/limits.md). Restricted functions
work inside parallel plans too: the rest of the query runs in parallel, and
the warren call runs in the leader.

- **`last_stats()` reports only calls the leader ran.** A traversal that
  runs in a worker records nothing, so after a parallel plan `last_stats()`
  still describes the leader's last recording call. Run the traversal
  without a parallel plan (`SET max_parallel_workers_per_gather = 0`)
  when you need its stats.
- **The cache's `NOTICE`s come from the leader only.** A worker raises
  none of them (a version over `warren.max_cache_memory`, the shared cache
  refusing it or unavailable), since Postgres would pass each on once per
  worker; the leader raises them where it reads the version itself, so a
  plan whose leader makes no call (a single-copy `Gather`, as under
  `debug_parallel_query`) shows none.
- **Wherever a version is read privately, each worker loads its own
  copy.** That is not only PostgreSQL 15 and 16 without
  `shared_preload_libraries = 'pg_warren'` or the shared cache turned off
  (`warren.shared_cache_memory = 0`): with the shared cache working, a
  version larger than `warren.shared_cache_memory`, one it has no room
  for (beside what is pinned or loading, or in `/dev/shm`: Docker's
  default 64 MB holds little), and one the current transaction created
  (built or refreshed and not yet committed) are read privately too, by
  the leader and by every worker. A parallel plan then costs one load per
  worker, and each worker holds its copy until the query ends, within
  its own `warren.max_cache_memory` and `warren.max_compute_memory`
  together (past both, the query fails with 53400, as it would in the
  leader). So a parallel plan can hold one copy per worker plus the
  leader's: with 4 workers, five copies of the version. A short query run
  in parallel can also be slower than without workers (1.8–3× over 10k
  rows, without the preload: docs/limits.md). The remedies:

  - Preload pg_warren on PostgreSQL 15 and 16, and give the shared cache
    room (`warren.shared_cache_memory`, `/dev/shm`: docs/limits.md).
  - Keep a query or a role off parallel plans, which any role can do:

    ```sql
    SET max_parallel_workers_per_gather = 0;             -- this session
    SET LOCAL max_parallel_workers_per_gather = 0;       -- this transaction
    ALTER ROLE app SET max_parallel_workers_per_gather = 0;  -- a role's sessions
    ```

  - Restore the leader-only behaviour of 0.4.0 for the functions you call
    per row. `ALTER FUNCTION` needs the function's owner, a superuser
    (the one that ran `CREATE EXTENSION`, or the bootstrap superuser in a
    trusted install, whose installer cannot):

    ```sql
    ALTER FUNCTION warren.has_edge(text, anyelement, anyelement, text, bigint) PARALLEL RESTRICTED;
    ```

    (`\df warren.*` lists every signature.) Such a change is not part of
    the extension's own script: `pg_dump` does not keep it, and an update
    to a later release may set the label again, so repeat it after a
    restore or an update.
