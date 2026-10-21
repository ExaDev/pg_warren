# Compute

Whole-graph computation in pg_warren is **SQL all the way down**: there is
no node-program language and no runner. Each primitive is one SQL call that
works on the whole graph at once; control flow (loops, convergence tests) is
your PL/pgSQL; per-node logic is a SQL expression.

| Tier | Node state | Fan-out and combine | Per-node apply | Cost per superstep |
|---|---|---|---|---|
| 1 | named **vectors** (numeric) | semiring `spmv` / `spmm` | `vector_map` (compiled SQL expression) | milliseconds at 10M nodes (design target) |
| 2 | any table, any SQL type | `gather`, in-engine | any SQL (`UPDATE … FROM gather(…)`) | seconds at 10M nodes: N rows through the executor |

The examples use this graph; they run as written in
`test/sql/docs_examples.sql`.

```sql
CREATE TABLE city (id int PRIMARY KEY, name text NOT NULL, pop int NOT NULL);
CREATE TABLE road (a int NOT NULL, b int NOT NULL, kind text NOT NULL, km float8 NOT NULL);
INSERT INTO city VALUES (1, 'ash', 100), (2, 'birch', 50), (3, 'cedar', 20), (4, 'dale', 80), (5, 'elm', 10);
INSERT INTO road VALUES (1, 2, 'highway', 10), (2, 3, 'lane', 4), (3, 4, 'highway', 7),
                        (1, 4, 'lane', 30), (4, 5, 'lane', 2);
SELECT warren.create_graph('roads', 'SELECT id FROM city', 'SELECT a, b, kind, km FROM road',
                           directed := false, weighted := true, has_relations := true);
SELECT warren.refresh('roads') AS v \gset
```

## Vectors

A **vector** is a dense array with one row per dense id (`id_space` rows),
bound to one version of a graph and named. `dtype` is `f32`, `f64`, `i64` or
`bool` (`real`, `double precision`, `bigint`, `boolean`). A vector of width
d > 1 is an N × d **matrix**, for example embeddings.

| Make | Read back |
|---|---|
| `vector_from_query(graph, name, query, dtype := 'f64', fill := 0)` | `vector_values(graph, name, NULL::<id type>)` → `(node_id, value)` |
| `vector_const(graph, name, value, dtype := 'f64')` | `matrix_rows(graph, name, NULL::<id type>)` → `(node_id, row float8[])` |
| `vector_degree(graph, name, direction := 'out', rels := NULL)` | `vector_topk(graph, name, k, NULL::<id type>, descending := true)` |
| `vector_dense_id(graph, name)` (each node's own dense id, `i64`) | `vector_stats(graph, name)` → `{count, min, max, mean, sd, l2, nnz}` |
| results of `spmv`, `spmm`, `vector_map`, the library, `eigen` | `vector_reduce(graph, x, op)`: `sum`, `max`, `min`, `l1`, `l2`, `nnz` |

```sql
SELECT warren.vector_from_query('roads', 'pop', 'SELECT id, pop FROM city');
SELECT warren.vector_const('roads', 'one', 1, persist := false);      -- transient: returns NULL
SELECT warren.vector_degree('roads', 'deg');
SELECT * FROM warren.vector_values('roads', 'pop', NULL::int) ORDER BY node_id;
SELECT warren.vector_stats('roads', 'pop');                           -- mean 52, ...
```

Rules:

- **Persisted or transient.** The constructors (`vector_from_query`,
  `vector_const`, `vector_degree`, `vector_dense_id`), `spmv`, `spmm`,
  `vector_map`, `triangles`, the [structural kernels](#structural-kernels)
  and `eigen` take `persist := true`. The library
  functions take no `persist`: they always persist `out`. A persisted vector
  is a row in `warren.vectors` (transactional, survives the session, costs
  WAL; 8 bytes × `id_space` per `f64` column).
- **Return values.** The constructors, `triangles` and `topological_order`
  return the persisted vector's id, or NULL when transient; `core_number`,
  `strong_components_vector` and `dag_longest_path` return a summary (the
  degeneracy, the component count, the longest path's length). `spmv`, `spmm`, `vector_map` and
  `aggregate_neighbours` return nothing (`void`). The iterative library
  functions return the iterations they ran, and `degree_vector` the vector
  id.
- A **transient** vector (`persist := false`) lives only in this backend,
  for this role. It costs no WAL, is not transactional, does not pin its
  version, and counts against `warren.max_cache_memory` (it is never
  evicted). It is freed at session end, or by `evict` or `drop_vector`
  (`if_exists := true` makes a missing name a no-op). Keep loop scratch
  transient and persist results.
- A name resolves to this role's transient vector first, then the persisted
  one. Writing a name replaces the vector of that name; a persisted vector
  may be replaced only by its writer or the graph owner.
- **A transient write over a persisted name deletes the persisted row, and
  that delete is transactional.** If the transaction rolls back, the
  persisted vector comes back, but the transient one survives the rollback
  and hides it until you `drop_vector` the name (which drops the transient
  one first).
- `vector_from_query` runs the query as you, under your `search_path`, in
  read-only transaction state (see [semantics.md](semantics.md#access)). It
  returns `(id, value)`; the id must cast *implicitly* to the id type (so
  ids are never rounded), and the value converts with SQL's own cast (2.5
  becomes 3 in `i64`). An array value makes a matrix row; all rows must have
  the same width. Nodes without a row get `fill`; two rows for one node are
  an error.
- **Vectors hold no NULL.** A NULL is stored as `NaN` in a float vector and
  is an error (22004) in an `i64` or `bool` vector.
- Tombstoned rows hold zero, and reads, rankings, statistics and reductions
  cover live nodes only.
- Every function takes `version := …` (default: current); names are looked
  up among the vectors of that version, and results are bound to it.
  `vector_norm_diff(graph, a, b, norm, a_version, b_version)` compares
  vectors of two versions, aligned by dense id. Like `distance`, it refuses
  (22023) versions whose shared dense ids name different nodes: across a
  `compact`, or two sibling staged versions that added different nodes.

## Semiring products: `spmv` and `spmm`

```
warren.spmv(graph, x, y, semiring := 'plus_times', transpose := false, rels := NULL,
            weighted := NULL, mask := NULL, mask_complement := false, replace := false,
            accumulate := NULL, persist := true, version := NULL)
```

`spmv` computes `y = A ⊗ x`, where `A[i][j]` is the weight of edge `i → j`,
summed over the selected `rels`; when the graph is unweighted it is 1, even
for a pair linked in several relations. With `transpose :=
false`, `y_i` combines node i's **out**-neighbours' values; with `transpose :=
true`, its **in**-neighbours' (as PageRank does). `spmm` is the same over a
width-d matrix, row by row.

A semiring is `<add>_<mult>`, with GraphBLAS names. `⊗` is applied as
`⊗(edge weight, x_j)`, so `first` takes the weight and `second` the
neighbour's value:

| Semiring | `y_i` = | Typical use |
|---|---|---|
| `plus_times` | Σ weight · x_j | PageRank, propagation |
| `plus_second` | Σ x_j | neighbour sums |
| `plus_first` | Σ weight | weighted degree |
| `plus_pair` | number of distinct neighbours | degree under `rels` |
| `min_plus`, `max_plus` | min / max of x_j + weight | shortest paths |
| `min_times`, `max_times` | min / max of weight · x_j | |
| `min_first`, `min_second`, `max_second` | min / max of weight, or of x_j | label propagation (`min_second`) |
| `lor_land` (`or_and`) | true if some neighbour's value is true | reachability |
| `any_pair` | 1 if there is any entry | frontier expansion |

```sql
SELECT warren.spmv('roads', 'pop', 'nbr_pop', 'plus_second');                 -- 130, 120, 130, 130, 80
SELECT warren.spmv('roads', 'one', 'highways', 'plus_pair', rels := ARRAY['highway']);  -- 1, 1, 1, 1, 0
SELECT warren.spmv('roads', 'one', 'km', 'plus_first');                       -- 40, 14, 11, 39, 2
```

A node with no entries gets the `⊕` identity: 0 for `plus`, +∞ for `min`, −∞
for `max`, false for `lor`. With `i64` vectors, `min`'s and `max`'s identities
(`bigint`'s extremes) stand for ±∞ and absorb `plus`, so repeated integer
`min_plus` leaves unreached nodes at the identity rather than overflowing.

### Masks, `replace` and `accumulate`

These follow GraphBLAS:

- `mask` names a width-1 vector; `y_i` is computed only where it is non-zero
  (NaN counts as non-zero). `mask_complement := true` inverts it.
- Outside the mask, `y` keeps its old values; with `replace := true` it gets
  the `⊕` identity instead.
- `accumulate := 'plus'` (or `min`, `max`, `times`, `first`, `second`, …)
  combines the product into the old `y` instead of overwriting it; a row with
  no entries then leaves `y_i` unchanged.

```sql
SELECT warren.vector_map('roads', 'big', 'CASE WHEN pop >= 50 THEN 1 ELSE 0 END');
SELECT warren.spmv('roads', 'pop', 'best', 'max_second');                    -- 80, 100, 80, 100, 80
SELECT warren.spmv('roads', 'pop', 'best', 'min_second', mask := 'big');     -- 50, 20, 80, 10, 80
SELECT warren.spmv('roads', 'pop', 'best', 'min_second', mask := 'big', replace := true);
                                                                             -- 50, 20, ∞, 10, ∞
SELECT warren.vector_const('roads', 'acc', 0);
SELECT warren.spmv('roads', 'pop', 'acc', 'plus_second', accumulate := 'plus');
SELECT warren.spmv('roads', 'pop', 'acc', 'plus_second', accumulate := 'plus');  -- 260, 240, 260, 260, 160
```

When `y`'s old values are read (outside the mask, or with `accumulate`), `y`
must have `x`'s dtype. Without a reverse CSR, `transpose := true` runs on one
thread.

## `vector_map` and `explain_map`

`vector_map(graph, y, expr)` sets `y` to a SQL expression evaluated per
node, over vector names (as columns) and literals:

```sql
SELECT warren.vector_map('roads', 'share', 'pop / NULLIF(nbr_pop, 0)');
SELECT warren.explain_map('roads', 'pop / NULLIF(nbr_pop, 0)');
-- {"path": "compiled", "dtype": "f64", "reason": "every operation is in the compiled subset"}
SELECT warren.explain_map('roads', 'pop::numeric + 1');
-- {"path": "executor", "dtype": "f64", "reason": "function numeric is not in the compiled subset"}
```

- Any scalar SQL expression works. An expression in the **compiled
  subset** runs natively across threads: arithmetic and comparisons on
  `double precision`, `real` and `bigint`; boolean operators; `CASE`,
  `NULLIF`, `COALESCE`, `LEAST`, `GREATEST`; casts among `double precision`,
  `real`, `bigint`, `integer`, `smallint` and `boolean`; and `abs`, `sqrt`, `exp`, `ln`, `log`, `power`, `floor`,
  `ceil`, `round`, `sign`. Numeric literals such as `0.85 * x` compile.
- Anything else runs through the **executor**, row by row (a design
  estimate of 100–500 ns per row; JIT-compiled per the `jit` settings). It
  runs as you, under your `search_path`, and in read-only transaction state:
  a function the expression calls cannot write, and `nextval` on a
  non-temporary sequence is refused (25006). Both paths give the same
  results and the same errors. `explain_map` says which path an expression
  takes, and why.
- Aggregates, window functions, set-returning functions and subqueries are
  refused.
- The result's dtype is the expression's type (`integer` becomes `bigint`,
  `numeric` becomes `double precision`). Width broadcasts: literals and
  width-1 vectors repeat across a matrix's rows (`emb / deg`); other width
  mismatches are errors. `0.7 * emb` over an `f32` matrix is `f64`; cast it
  (`(…)::real`) to keep `f32`.
- Errors match SQL's: division by zero, overflow, `sqrt` of a negative.
  `CASE WHEN x = 0 THEN 0 ELSE 1 / x END` does not fail, because only the
  branches SQL would evaluate are evaluated.

A tier-1 program is a loop over these calls:

```sql
CREATE FUNCTION my_pagerank(graph text, damping float8 DEFAULT 0.85, tol float8 DEFAULT 1e-6)
RETURNS int LANGUAGE plpgsql AS $$
DECLARE i int := 0; delta float8;
BEGIN
  PERFORM warren.vector_degree(graph, 'deg', persist := false);
  PERFORM warren.vector_const(graph, 'pr', 1.0, persist := false);
  PERFORM warren.vector_const(graph, 'teleport', 1 - damping, persist := false);
  PERFORM warren.vector_const(graph, 'damping', damping, persist := false);
  LOOP
    i := i + 1;
    PERFORM warren.vector_map(graph, 'msg', 'pr / NULLIF(deg, 0)', persist := false);
    PERFORM warren.spmv(graph, 'msg', 'gathered', 'plus_second', transpose := true, persist := false);
    PERFORM warren.vector_map(graph, 'next', 'teleport + damping * gathered', persist := false);
    delta := warren.vector_norm_diff(graph, 'next', 'pr', 'l1');
    PERFORM warren.vector_map(graph, 'pr', 'next', persist := false);
    EXIT WHEN delta < tol OR i >= 100;
  END LOOP;
  RETURN i;
END $$;
```

Constants enter the expression as constant vectors (`teleport`, `damping`),
not as formatted text, so no client setting such as `extra_float_digits`
rounds them. `plus_second` ignores edge weights; the default `plus_times` would multiply
by them on a weighted graph. Pin the version (`version := …` on every call)
if a concurrent `refresh` could promote a new one mid-loop; the library
functions do.

## `gather`

`gather` is tier 2: messages of **any SQL type** fanned out over the graph
and combined at each receiver, in-engine.

```
warren.gather(graph, messages, NULL::<id type>, NULL::<result type>,
              combine := 'collect', direction := 'in', rels := NULL,
              mask := NULL, mask_complement := false, version := NULL)
  RETURNS TABLE (node_id, value)
```

- `messages` is read by its form:
  - Text that is not an identifier (or `schema.identifier`) is a **query**.
  - An identifier is a **vector** bound to the version, if one has exactly
    that name (every live node sends its value; it must have width 1, so
    send a matrix's rows from a query), else a **table or view** on your
    `search_path`. A name that is both is an error (42725): vector names
    are shared by the graph's writers, so one could otherwise redirect your
    `gather`.
  - `TABLE <name>` always means the table or view.
- A table or query returns `(id, message)`, runs as you under your
  `search_path`, and is read in read-only transaction state: a data-modifying
  CTE is refused (0A000), and so is a write by a function it calls (25006).
  You must be a reader of the graph. A node may send several messages, or
  none; rows with a NULL, unknown or tombstoned id, or a NULL message, send
  nothing.
- Each receiver gets one copy of each message from each of its **senders**:
  its distinct neighbours in `direction` (`in`: nodes with an edge to it;
  `out`; `both`) over `rels`. Weights are not read. Receivers with no
  message, or outside `mask`, return no row.
- Messages are combined in ascending sender dense id, then in the order
  read, so results are the same for any `warren.threads`.

| `combine` | Messages | Result type (the second witness) |
|---|---|---|
| `collect` | any | `T[]` (as `array_agg`); array messages gain a dimension |
| `count` | any | `bigint` |
| `first` | any | the message type |
| `sum` | integer types, `real`, `double precision` | `bigint`, `real`, `double precision` |
| `min`, `max` | integer types, `real`, `double precision` | the message type |
| `bool_or`, `bool_and` | `boolean` | `boolean` |
| `concat` | `text`, `bytea` | `text`, `bytea` |

A wrong witness is an error that names the right one (42804).

```sql
SELECT * FROM warren.gather('roads', 'SELECT id, name FROM city', NULL::int, NULL::text[]);
-- 4 | {ash,cedar,elm}  ...
SELECT * FROM warren.gather('roads', 'pop', NULL::int, NULL::float8, 'max');
SELECT * FROM warren.gather('roads', 'SELECT id, pop FROM city', NULL::int, NULL::bigint, 'sum',
                            rels := ARRAY['lane']);                    -- 80, 20, 50, 110, 80
```

A tier-2 superstep is a `gather` plus an `UPDATE … FROM` over a state table,
so each superstep costs N rows through the executor. To collect sets of
different sizes, send their elements as separate messages
(`SELECT id, unnest(tags) FROM state`).

## The algorithm library

PL/pgSQL functions over the primitives above (read their source with `\sf
warren.pagerank`; it doubles as a template). Each pins one version, keeps its
intermediates transient, and persists only `out`.

| Function | Computes | Returns |
|---|---|---|
| `pagerank(graph, out, damping := 0.85, tol := 1e-6, max_iter := 100, seed := NULL, rels, weighted)` | PageRank as NetworkX computes it; `seed` (a vector) personalises it; `out` sums to 1 | iterations |
| `components_vector(graph, out, rels)` | each node's component, as the minimum dense id in it (min-label propagation, both directions) | iterations |
| `label_propagation(graph, out, seed := NULL, max_iter := 100, mask := NULL, rels)` | min-label propagation from `seed` labels, restricted to `mask` | iterations |
| `sssp_vector(graph, out, source, rels, weighted, max_iter)` | distance from `source` (Bellman-Ford); unreachable is `Infinity`; a negative cycle reachable from `source` is an error (22023) | iterations |
| `degree_vector(graph, out, direction := 'out', rels)` | distinct-neighbour degree | vector id |
| `aggregate_neighbours(graph, x, out, agg := 'mean', hops := 1, rels, direction := 'out')` | `mean`, `sum` or `max` of neighbours' rows of matrix `x` (a "one-layer GNN") | void |
| `triangles(graph, out, rels := NULL, persist := true)` | per-node triangle count in the undirected simple graph | vector id |
| `eigenvector_centrality(graph, out, tol := 1e-6, max_iter := 100, rels, weighted)` | eigenvector centrality as NetworkX computes it (in-edges, unit L2 norm) | iterations |
| `katz_centrality(graph, out, alpha := 0.1, beta := 1.0, seed := NULL, normalized := true, tol := 1e-6, max_iter := 1000, rels, weighted)` | Katz centrality as NetworkX computes it; `seed` (a vector) makes β per node: `beta × seed` | iterations |
| `hits(graph, hubs, authorities, tol := 1e-8, max_iter := 100, normalized := true, rels, weighted)` | HITS hub and authority scores as NetworkX computes them, into two vectors | iterations |
| `clustering(graph, out, rels)` | local clustering coefficient of the undirected unweighted graph, as `nx.clustering` | void |
| `label_propagation_majority(graph, out, seed := NULL, max_iter := 100, rels)` | majority-vote label propagation: synchronous, smallest label on a tie | iterations |
| `core_number(graph, out, rels := NULL, persist := true)` | each node's k-core number in the undirected simple graph | the degeneracy |
| `strong_components_vector(graph, out, rels := NULL, persist := true)` | each node's strongly connected component, as the smallest dense id in it | the component count |
| `topological_order(graph, out, rels := NULL, persist := true)` | each node's position, from 0, in the topological order (smallest dense id first among ready nodes) | vector id |
| `dag_longest_path(graph, out, rels := NULL, weighted := NULL, persist := true)` | the length of the longest path ending at each node | the longest length |
| `minimum_spanning_forest(graph, id_type, rels := NULL, weighted := NULL)` | rows `(source, target, weight)`: a minimum spanning tree of each component | rows |

All take `version := …`. `label_propagation` is deterministic min-label
propagation; `label_propagation_majority` is the majority vote. `triangles`
and the structural kernels are native kernels rather than PL/pgSQL
([below](#structural-kernels)).

The centralities follow NetworkX exactly: the same start, iteration,
normalisation and stopping rule, so with the same `tol` and `max_iter` they
stop at the same iteration and agree to rounding (tested on random graphs).
`weighted` follows `spmv`: NULL reads the weights if the graph has them
(NetworkX's `weight=None` default for eigenvector and Katz centrality is
`weighted := false` here). Where NetworkX raises
`PowerIterationFailedConvergence`, these write the last iterate and raise a
`WARNING` (`… did not converge in 100 iterations`); the iterations returned
are then `max_iter`. A run whose values stop being finite is an error
(22003, `… diverged`) and writes nothing. Katz diverges when `alpha` is at
least 1 / the largest eigenvalue of the adjacency matrix (about 1 / average
degree on a random graph): its default 0.1 is too large for a graph of
average degree 10 or more. Eigenvector centrality and HITS diverge only
when products of edge weights overflow `double precision`.

A NULL argument is an error (22004), as are `tol < 0` and `max_iter < 1`
(22023). The three centralities refuse a graph with no live nodes (22023;
NetworkX raises only for eigenvector centrality and returns nothing for the
other two); `clustering` and `label_propagation_majority` write an empty
`out` for it, the latter after 0 iterations.

How they differ from NetworkX:

- `clustering` counts a node with a self-loop among its own neighbours
  (`vector_degree` does), where NetworkX leaves the self-loop out, so such a
  node in a triangle scores lower. It has no `weighted`: NetworkX's weighted
  coefficient needs each triangle's edge weights, which `triangles` does not
  read.
- `label_propagation_majority` has no NetworkX equivalent. NetworkX's
  `asynchronous_label_propagation` visits nodes in random order and breaks
  ties at random; `label_propagation_communities` is semi-synchronous over a
  graph colouring. Here every node updates at once, from its own label and
  each neighbour's (direction ignored, a self-loop counting its own label
  once more), to the most frequent, the smallest on a tie, until no label
  changes. Being synchronous, it can cycle between two labellings (six
  nodes are enough); it then stops at `max_iter` with the `WARNING`. Edge
  weights are not read. It is a tier-2 program (`gather` with `collect`,
  votes counted in SQL), so an iteration costs N + M rows through the
  executor, seconds at a million nodes (see `docs/limits.md`).

```sql
SELECT warren.pagerank('roads', 'pr', weighted := false);
SELECT warren.sssp_vector('roads', 'dist', 1);                         -- 0, 10, 14, 21, 23
SELECT warren.vector_from_query('roads', 'emb', 'SELECT id, ARRAY[pop, 100 - pop]::real[] FROM city',
                                dtype := 'f32');
SELECT warren.aggregate_neighbours('roads', 'emb', 'emb_nbr');
SELECT * FROM warren.matrix_rows('roads', 'emb_nbr', NULL::int);       -- elm: {80,20}
```

The library's intermediates are transient vectors named
`_warren_<function>_<part>`. They are dropped when the function returns, and
also when it fails or is cancelled, so a run leaves nothing behind but `out`.
The `_warren_` prefix is **reserved**: no persisted vector may have such a
name (`reserved_name`, 42939), including `eigen`'s `<vectors_prefix>_i`, and
the library refuses it for `out` and its inputs. Transient vectors of your
own may still use it, but there is no reason to.

### Structural kernels

`core_number`, `strong_components_vector`, `topological_order`,
`dag_longest_path` and `minimum_spanning_forest` are native kernels: each is
one pass (or, for the forest, a few) over the version's CSRs, as NetworkX's
function of the same name computes it. None needs a reverse CSR. They run on
one compute thread (the sort of `core_number`'s adjacency uses them all),
check for cancellation as they go, and charge their scratch to
`warren.max_compute_memory`: about 16 bytes per stored edge plus 32 per node
for `core_number`, at most 56 per node for `strong_components_vector`, 16
for `topological_order` and `dag_longest_path`, and 40 for
`minimum_spanning_forest` plus up to 48 for its result, and, for the last
three when they read several relations' CSRs, a row scratch of 16 bytes per
entry of the longest row (DESIGN.md §6.7).
Edges are read under `rels` as the products read them: an edge in several
selected relations is one edge, its weights summed.

- `core_number` and `minimum_spanning_forest` ignore direction and
  self-loops. `core_number` returns the graph's degeneracy (its largest core
  number).
- `strong_components_vector` labels components as `components_vector` does,
  by the smallest dense id in each, and returns how many there are.
- `topological_order` takes, among the nodes whose predecessors are all
  placed, the smallest dense id first: NetworkX's
  `lexicographical_topological_sort` by dense id (on a graph's first build in
  `'id'` order, by node id). `dag_longest_path` gives the longest path *ending
  at* each node: the sum of its edges' weights (with `weighted := false`, or
  on an unweighted graph, its number of edges), and never below 0, since a
  path restarts after a prefix of negative length, as NetworkX's
  `dag_longest_path` does. Its return value is NetworkX's
  `dag_longest_path_length`. On a graph with a cycle (a self-loop included)
  both fail with 22023 and write nothing.
- `minimum_spanning_forest` returns each forest edge in its stored direction,
  ordered by source and target. Where `u → v` and `v → u` both exist, the
  lighter can be chosen. Edges are compared by weight in SQL order (NaN
  heaviest, `-0 = 0`) and then by their endpoints' dense ids, so the forest is
  the same however it is computed; with tied weights, another algorithm may
  pick other edges of the same total. `id_type` fixes the id columns' type,
  as for `components`; it is `PARALLEL RESTRICTED` and writes nothing.

```sql
SELECT warren.core_number('roads', 'core');                            -- 2: the cycle ash–birch–cedar–dale
SELECT * FROM warren.minimum_spanning_forest('roads', NULL::int);      -- 4 roads, 23 km: all but ash–dale

CREATE TABLE course (id int PRIMARY KEY);
CREATE TABLE prereq (first int, next int, hours float8);
INSERT INTO course VALUES (1), (2), (3), (4), (5);
INSERT INTO prereq VALUES (4, 1, 2), (3, 1, 1), (1, 2, 3), (3, 5, 4);
SELECT warren.create_graph('plan', 'SELECT id FROM course', 'SELECT first, next, hours FROM prereq',
                           weighted := true);
SELECT warren.refresh('plan');
SELECT warren.topological_order('plan', 'step');                       -- 3, 4, 1, 2, 5
SELECT warren.dag_longest_path('plan', 'hours');                       -- 5: 4 → 1 → 2
SELECT warren.strong_components_vector('plan', 'scc');                 -- 5: no cycles
```

### Shortest-path centralities

`betweenness_centrality`, `closeness_centrality` and `harmonic_centrality`
write an `f64` vector `out` and return how many sources they used. Each is
NetworkX's function of the same name over the shortest paths under `rels`:
along edges (`directed := true`, the default) or in the undirected simple
graph (`directed := false`; a pair joined both ways weighs the lighter of
its two), weighted if `weighted` (NULL: if the graph has weights; paths are
then the lightest, by Dijkstra, and every weight on a path must be positive
and finite, else 22023), else by edge count. An edge in several selected
relations is one edge, its weights summed. Closeness and harmonic
centrality use the distances *to* each node, as NetworkX does on a directed
graph.

With `k := NULL` (the default) every live node is a source and the result
is exact: O(N·(N + M)), for graphs up to some thousands of nodes. With `k`,
`k` sources are sampled with `seed` (default 0): the same seed samples the
same nodes, on any thread count and after a refresh, for the nodes that
survive it. `sources := '<vector>'` restricts the sources to the nodes a
vector selects (all of them if `k` is NULL), so a sample can be chosen in
SQL, or the same sources given to another tool.

- **Betweenness** is rescaled exactly as NetworkX 3.7 rescales it for
  `normalized` (default true), `endpoints` (default false) and a sample of
  `k` sources: a node's sum over the sampled sources is scaled up to all
  sources, and without `endpoints` a sampled source, which is never its own
  path's end, is scaled by one source fewer (NaN when `k = 1` and
  `normalized`, as NetworkX). Unnormalised undirected betweenness counts each
  unordered pair once.
- **Closeness** is `(r − 1) / Σ d(v, u)` over the `r − 1` nodes `v` that
  reach `u`, times `(r − 1) / (n − 1)` with `wf_improved` (default true);
  **harmonic** is `Σ 1 / d(v, u)`. From a sample `S`, each node's sources
  other than itself, `k_u = |S \ {u}|`, stand for all `n − 1` other nodes:
  closeness is `m_u / Σ_{s ∈ S} d(s, u)` over the `m_u` sources that reach
  it, times `m_u / k_u`, and harmonic is scaled by `(n − 1) / k_u`. With
  distances bounded by the diameter Δ, the sampled mean distance to a node
  is within ε·Δ of the true mean with probability at least `1 − 2e^(−2kε²)`
  (Hoeffding; Eppstein and Wang), so `k = O(log n / ε²)` sources estimate
  every node's closeness to within ε·Δ in its average distance; harmonic
  centrality's terms are in [0, 1], so `harmonic / (n − 1)` is within ε of
  the truth with the same probability.

The sources run one per compute thread at a time, each search touching only
the nodes it reaches; each thread's scratch is 16 bytes per node in the id
space (32 for betweenness), charged to `warren.max_compute_memory`, plus a
Dijkstra queue when weighted, and `directed := false` builds the symmetric
adjacency (16 bytes per stored edge, 32 weighted). Contributions are added
in ascending source order whatever the thread count.

```sql
SELECT warren.betweenness_centrality('roads', 'between', normalized := false, directed := false);
                                         -- 5 sources: 0, 3, 4, 3, 0 (birch, cedar, dale carry the paths)
SELECT warren.closeness_centrality('roads', 'close');                  -- cedar closest: 2/17
SELECT warren.harmonic_centrality('roads', 'reach', k := 3, seed := 42, weighted := false);  -- 3 sources
```

### Neighbourhood similarity

`similarity`, `most_similar` and `similar_pairs` score pairs of nodes for
link prediction by `measure`: `'jaccard'` (the default), `'adamic_adar'` or
`'common_neighbours'`, over the undirected simple graph under `rels`
(directions and self-loops ignored), as NetworkX's
`jaccard_coefficient`, `adamic_adar_index` and `common_neighbors`. They
return rows of external ids, write nothing and are `PARALLEL RESTRICTED`.

- `similarity(graph, pairs, id_type, measure, rels, version)` scores the
  pairs a query returns (NetworkX's `ebunch`): two id columns, run as the
  caller under their `search_path`, read-only. Rows come in the query's
  order; a pair with a NULL, unknown or deleted id is skipped.
- `most_similar(graph, node, k DEFAULT 10, measure, rels, version)` ranks
  one node's candidates: the nodes sharing a neighbour with it that are
  neither it nor its neighbours (NetworkX's non-edges, less those scoring
  0), best first, ties by dense id.
- `similar_pairs(graph, id_type, k DEFAULT 10, measure, nodes, rels,
  version)` ranks each node's candidates the same way (only for the nodes
  vector `nodes` selects, if given): at most `k` rows per node. All pairs
  are never scored: that is N² rows.

Pairs are scored by intersecting two sorted neighbour rows, merged or, for
a hub, binary-searched, as `triangles` does; rankings expand two hops from
each node, `Σ_{w ∈ N(u)} |N(w)|` steps, so a node next to a hub pays for
the hub's row. When every selected relation has a reverse CSR, `similarity`
and `most_similar` read the rows they need directly, so one node's ranking
costs its two-hop neighbourhood; otherwise, and always for `similar_pairs`,
the symmetric adjacency is built first (16 bytes per stored edge, and 8 per
node for degrees), costing about a `triangles` call. `similar_pairs` keeps
20 bytes per node per compute thread. Adamic–Adar's term for a common
neighbour `w` is `1 / ln |N(w)|`; only a node paired with itself can share a
neighbour of degree 1, whose term is +∞ (NetworkX raises
`ZeroDivisionError`).

```sql
SELECT * FROM warren.most_similar('roads', 1);                         -- cedar 1, elm 0.5
SELECT * FROM warren.similarity('roads', 'VALUES (2, 4), (1, 5)', NULL::int, 'adamic_adar');
                                                                       -- 2.885…, 0.910…
```

## `eigen`

```
warren.eigen(graph, k, matrix := 'adjacency', which := 'largest', rels := NULL,
             weighted := NULL, symmetrize := true, tol := 1e-6, max_iter := NULL,
             vectors_prefix := NULL, version := NULL, dtype := 'f32', persist := true)
  RETURNS TABLE (i int, eigenvalue float8, residual float8)
```

Top-k eigenpairs by Lanczos with full reorthogonalisation and thick restart.

- `matrix`: `adjacency` (A), `laplacian` (D − A), `normalized_laplacian`
  (I − D^-1/2 A D^-1/2) or `random_walk` (D^-1 A). A directed graph is
  symmetrised as (A + Aᵀ)/2; `symmetrize := false` is accepted only when the
  matrix is already symmetric (an undirected graph, or a directed one whose
  edges all come in both directions), and then halves the work.
- `which`: `largest` (by magnitude) or `smallest` (algebraic; use it for the
  Laplacians). Under `largest`, magnitudes that differ only by rounding
  (10⁻⁶ relative for `f32`, 10⁻¹² for `f64`; never `tol`) are ties, and a
  run of ties lists its positive eigenvalues first, so `λ` comes before `−λ`
  whichever rounding made larger.
- A pair has converged when its residual estimate is at most `tol` times
  the largest Ritz value (about ‖M‖). The default `tol` of 10⁻⁶ is about the
  rounding of `f32` vectors; pass a smaller one with `dtype := 'f64'`.
  `max_iter` counts operator applications; it defaults to 1,000 per basis
  vector (41,000 for k = 20).
- **Not converging is a warning, not an error.** If `max_iter` runs out,
  `eigen` returns the pairs it has, with a `WARNING` naming the steps taken
  and the largest residual; each row's `residual` says which pairs are
  unconverged. Raise `max_iter` or `tol`.
- When the active nodes form several connected components, `eigen` warns
  too: their repeated eigenvalues may not all be found (see
  [limits.md](limits.md#known-limitations)).
- `residual` is ‖Mx − λx‖₂ for the eigenvector as stored. With `f32` vectors
  (the default) it floors near 10⁻⁷ · ‖M‖; `dtype := 'f64'` doubles the
  memory and goes further.
- `vectors_prefix := 'lap'` stores the eigenvectors as vectors `lap_1 …
  lap_k` (persisted unless `persist := false`). Persisted eigenvectors pin
  their version against retention. A prefix starting with `_warren_` is
  refused (42939) unless `persist := false`.
- The problem is over **active** nodes (those with an edge under `rels`);
  eigenvectors are 0 at isolated and tombstoned nodes, and `k` may not
  exceed the active node count.
- Results are bitwise identical across runs and thread counts.
- Memory: about `max(2k + 1, 20)` vectors of `id_space` rows of `dtype`,
  charged to `warren.max_compute_memory`.

```sql
SELECT * FROM warren.eigen('roads', 2, 'laplacian', 'smallest', vectors_prefix := 'lap', dtype := 'f64');
-- i = 1: eigenvalue 0 (one connected component)
```

## `distance`

```
warren.distance(graph, v1, v2, method, vectors_prefix := NULL, k := NULL, x := NULL, y := NULL,
                compared_rows := 'shared', matrix := NULL, which := 'largest', rels := NULL,
                weighted := NULL, symmetrize := true, tol := 1e-6, max_iter := NULL, dtype := 'f32',
                graph2 := NULL)
  RETURNS jsonb   -- {"method", "from", "to", ["graph2"], …the method's scalars}
```

Compares version `v1` with version `v2` (of `graph`, or of `graph2`). Rows
are aligned by dense id; across graphs, by external id.

| Method | Needs | Returns |
|---|---|---|
| `edit` | nothing | per relation and `overall`: `added`, `removed`, `union`, `distance` (Jaccard, `(added + removed) / union`); weights ignored |
| `degree` | nothing | `wasserstein` (W₁, in degrees), `ks`, over live nodes' out-degrees |
| `spectral` | `k`; optionally `vectors_prefix` | `l2`, `relative`, `max` of the eigenvalue difference, and `before`, `after`. With a prefix it uses the stored `<prefix>_1..k` (their Rayleigh quotients under `matrix`, which you must then name, since a stored vector does not record its operator); without, it solves `k` eigenpairs of `matrix` (default `adjacency`) on each side and adds `converged` (true when both solves converged) |
| `subspace` | `vectors_prefix`, `k` (run `eigen` on both versions first) | `projection` (‖P₁ − P₂‖_F), `max_angle`, `angles`: principal angles between the two eigenvector spans; the most sensitive to restructuring, the least to volume |
| `embedding` | `x` (matrix of `v1`), `y` (of `v2`, default `x`) | `mean_l2`, `max_l2`, `mean_cosine`, `max_cosine` (1 − cos), `rows` compared |

- `compared_rows` (`subspace`, `embedding`): `shared` (default) compares
  only nodes present on both sides; `all` counts absent rows as zero, so
  added and removed nodes dominate.
- Versions of one graph are aligned by dense id, so `distance` refuses
  (22023) when their shared dense ids name different nodes: across a
  `compact`, and between two **sibling staged versions** (two
  `refresh(promote := false)` from one current version) whose new nodes got
  the same dense ids. Siblings that added the same new nodes, or none,
  compare. Relations never cause a refusal: they are matched by name.
- With `graph2`, both graphs need the same id type; the second version is
  mapped into the first's ids by external id, so a compaction is no barrier.
  For `edit`, the second graph's CSRs are rebuilt in the first's ids, which
  is charged to `warren.max_compute_memory` (about the size of its forward
  CSR) and can be cancelled.

```sql
SELECT warren.distance('roads', :v, :v, 'edit');
-- {"to": …, "from": …, "method": "edit", "overall": {"added": 0, "union": 10, "removed": 0, "distance": 0.0},
--  "relations": [{"relation": "highway", …}, {"relation": "lane", …}]}
```

[change-detection.md](change-detection.md) puts `eigen` and `distance` to
work.
