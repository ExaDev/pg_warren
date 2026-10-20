# Change detection

The question: a batch of new data (a corpus, a day's transactions) is about
to land in the graph's tables. Before committing it, how much does it change
the graph's *structure*, not just its size? And can the commit be gated on
the answer?

pg_warren supports two patterns. Both compare two graph versions with
[`diff`, `eigen` and `distance`](compute.md):

| | 1. Staged build in the transaction | 2. Shadow graph |
|---|---|---|
| New data goes into | the real tables, uncommitted | separate tables (a schema or partition) |
| Compared | two versions of one graph, by dense id | versions of two graphs, by external id |
| Decide with | `COMMIT` or `ROLLBACK` | swap the data in, or drop it |
| Cost | one long transaction | a second graph build; no long transaction |
| Use when | the batch and the comparison fit in one transaction | the batch is large or the comparison slow |

The examples run as written in `test/sql/docs_examples.sql`, on two
communities of four entities joined by one edge:

```sql
CREATE TABLE entities  (id bigint PRIMARY KEY);
CREATE TABLE relations (src bigint NOT NULL, dst bigint NOT NULL);
INSERT INTO entities SELECT generate_series(1, 8);
INSERT INTO relations SELECT a, b FROM generate_series(1, 4) a, generate_series(1, 4) b WHERE a < b;
INSERT INTO relations SELECT a, b FROM generate_series(5, 8) a, generate_series(5, 8) b WHERE a < b;
INSERT INTO relations VALUES (4, 5);
SELECT warren.create_graph('kg', 'SELECT id FROM entities', 'SELECT src, dst FROM relations', directed := false);
SELECT warren.refresh('kg') AS base \gset
SELECT * FROM warren.eigen('kg', 2, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap', dtype := 'f64');
```

## Pattern 1: a staged build inside the transaction

`refresh` sees the transaction's own uncommitted writes, and a version built
in a transaction that rolls back disappears with it
([semantics.md](semantics.md#transactions)). So the whole workflow is one
transaction:

```sql
BEGIN;

-- 1. ingest: a new entity that bridges the two communities
INSERT INTO entities VALUES (9);
INSERT INTO relations SELECT 9, id FROM entities WHERE id <> 9;

-- 2. build a staged version from this transaction's view of the tables
SELECT warren.refresh('kg', promote := false, label := 'corpus-2026-10-19') AS candidate \gset

-- 3. the structural delta (cheap: O(N + M))
SELECT warren.diff('kg', :base, :candidate) -> 'nodes'         AS nodes,   -- added 1
       warren.diff('kg', :base, :candidate) -> 'all_relations' AS edges;   -- added 16

-- 4. the spectral comparison
SELECT * FROM warren.eigen('kg', 2, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap',
                           dtype := 'f64', version := :candidate);
SELECT warren.distance('kg', :base, :candidate, 'subspace', vectors_prefix := 'lap', k := 2) AS change \gset
SELECT warren.distance('kg', :base, :candidate, 'spectral', vectors_prefix := 'lap', k := 2,
                       matrix := 'normalized_laplacian') AS gap \gset
SELECT :'change'::jsonb ->> 'projection' AS projection,   -- 0.022
       :'gap'::jsonb -> 'after'          AS eigenvalues;  -- [0, 0.328]: was [0, 0.113]

-- 5. decide
ROLLBACK;   -- the ingest, the staged version and its eigenvectors all vanish
-- or: COMMIT;  then, to make it the default version:  SELECT warren.promote('kg', :candidate);
```

What each step tells you:

- `diff` measures **volume**: nodes and edges added and removed, and the
  degree distribution's shift. It cannot tell "more of the same" from "a new
  structure".
- `distance('subspace')` compares the spans of the leading eigenvectors over
  the nodes present in both versions. A batch that only adds volume barely
  turns them; one that merges communities or adds a new one turns them by
  about one dimension (in `test/sql/a2_spectral.sql`, 0.40 for volume against
  1.27 for a new community and 1.43 for a bridge, at k = 6, over communities
  of 40).
- In the small example above, though, the subspace barely moves (0.022): at
  k = 2 there is one non-trivial eigenvector, and the new entity leaves the
  split between the two communities where it was. **The signal there is the
  spectral gap**, below. Run both measures, and calibrate each on your own
  data, at the `k` you will use.
- `distance('spectral')` compares eigenvalues. Here the bridge triples the
  second-smallest normalised-Laplacian eigenvalue (the spectral gap), because
  the two communities are no longer nearly disconnected.
- `distance('embedding')`, after `aggregate_neighbours` on both versions,
  measures how much neighbour-smoothed embeddings drift.

Pick the thresholds for your data from runs on batches you already judged.

To compare two candidate batches, build each as a staged version from the
same current version and compare each with `:base`. Comparing the two
candidates with each other (`diff`, `distance`, `vector_norm_diff`) is
refused (22023) if they added different new nodes, since both handed out the
same new dense ids.

After `COMMIT` without `promote`, the candidate is a staged version:
queryable with `version := :candidate`, invisible by default, never deleted by
retention. `promote` it or `drop_version` it. Its eigenvectors, and those on
`:base`, are persisted vectors and pin both versions against retention until
dropped ([semantics.md](semantics.md#retention)); pass `persist := false` to
`eigen` if you need them only within the session.

### The long-transaction caveat

While the transaction is open it holds back `VACUUM`'s cleanup horizon:
rows deleted or updated anywhere in the database after it started cannot be
removed until it ends, so tables with heavy write traffic bloat. `eigen` on a large graph can take
minutes (see
[limits.md](limits.md#measured-performance)). If the batch is large or the
comparison slow, use pattern 2.

## Pattern 2: a shadow graph

Load the batch into tables of their own, define a second graph over the
current data plus the batch, and compare the two graphs. `distance` with
`graph2` aligns the second graph's version to the first by **external id**
(both graphs need the same id type), so the comparison gives the same numbers
as pattern 1:

```sql
CREATE SCHEMA shadow;
CREATE TABLE shadow.entities  AS SELECT id FROM entities;
CREATE TABLE shadow.relations AS SELECT src, dst FROM relations;
INSERT INTO shadow.entities VALUES (9);
INSERT INTO shadow.relations SELECT 9, id FROM shadow.entities WHERE id <> 9;

SELECT warren.create_graph('kg_shadow', 'SELECT id FROM shadow.entities',
                           'SELECT src, dst FROM shadow.relations', directed := false);
SELECT warren.refresh('kg_shadow') AS shadow \gset
SELECT * FROM warren.eigen('kg_shadow', 2, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap',
                           dtype := 'f64');

SELECT warren.distance('kg', :base, :shadow, 'subspace', vectors_prefix := 'lap', k := 2,
                       graph2 := 'kg_shadow');                  -- same projection as pattern 1
SELECT warren.distance('kg', :base, :shadow, 'edit', graph2 := 'kg_shadow') -> 'overall';
-- {"added": 16, "union": 42, "removed": 0, "distance": 0.381}
```

Then either move the batch into the real tables and `refresh('kg')`, or
drop it (`drop_graph('kg_shadow')` and the shadow tables). In practice the
shadow graph's queries are usually a `UNION ALL` of the real tables and the
batch (or a partitioned table with a staging partition) rather than copies.

Notes:

- Each step is its own short transaction, so nothing holds back vacuum.
- The shadow graph's dense ids are its own; `graph2` maps them by external id
  (shared nodes keep the first graph's dense ids). A compaction of either
  graph is no barrier here.
- In the A.2 integration test, the shadow comparison matches the in-transaction
  one to 10⁻⁹ (f64 eigenvectors, `tol := 1e-12`, since the two Lanczos runs
  start from different vectors).
- The shadow graph needs its owner's access to the shadow tables, and its own
  `grant`s for readers.
