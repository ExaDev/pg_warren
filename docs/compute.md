# Compute

Whole-graph computation in pg_warren is **SQL all the way down**: there is
no vertex-program language and no runner. Each primitive is one SQL call that
works on the whole graph at once; control flow (loops, convergence tests) is
your PL/pgSQL; per-node logic is a SQL expression.

| Tier | Vertex state | Fan-out and combine | Per-node apply | Cost per superstep |
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

- **Persisted or transient.** Every function that writes a vector takes
  `persist := true`. A persisted vector is a row in `warren.vectors`
  (transactional, survives the session, costs WAL; 8 bytes × `id_space` per
  `f64` column) and its function returns its id. A **transient** vector
  (`persist := false`) lives only in this backend, for this role, returns
  NULL, costs no WAL, is not transactional, does not pin its version, and is
  freed at session end or by `evict` or `drop_vector`. Keep loop scratch
  transient and persist results.
- A name resolves to this role's transient vector first, then the persisted
  one. Writing a name replaces the vector of that name; a persisted vector
  may be replaced only by its writer or the graph owner.
- `vector_from_query` runs the query as you, under your `search_path`. It
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
  vectors of two versions, aligned by dense id.

## Semiring products: `spmv` and `spmm`

```
warren.spmv(graph, x, y, semiring := 'plus_times', transpose := false, rels := NULL,
            weighted := NULL, mask := NULL, mask_complement := false, replace := false,
            accumulate := NULL, persist := true, version := NULL)
```

`spmv` computes `y = A ⊗ x`, where `A[i][j]` is the weight of edge `i → j`
(1 when unweighted, summed over the selected `rels`). With `transpose :=
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
  estimate of 100–500 ns per row; JIT-compiled per the `jit` settings), as you and under
  your `search_path`. Both paths give the same results and the same errors.
  `explain_map` says which path an expression takes, and why.
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
  LOOP
    i := i + 1;
    PERFORM warren.vector_map(graph, 'msg', 'pr / NULLIF(deg, 0)', persist := false);
    PERFORM warren.spmv(graph, 'msg', 'gathered', 'plus_second', transpose := true, persist := false);
    PERFORM warren.vector_map(graph, 'next', format('%s + %s * gathered', 1 - damping, damping),
                              persist := false);
    delta := warren.vector_norm_diff(graph, 'next', 'pr', 'l1');
    PERFORM warren.vector_map(graph, 'pr', 'next', persist := false);
    EXIT WHEN delta < tol OR i >= 100;
  END LOOP;
  RETURN i;
END $$;
```

`plus_second` ignores edge weights; the default `plus_times` would multiply
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

- `messages` is the name of a vector of the version (every live node sends
  its value), else a table or view name, else a query. A table or query
  returns `(id, message)` and runs as you, under your `search_path`; a node
  may send several messages, or none. Rows with a NULL, unknown or
  tombstoned id, or a NULL message, send nothing.
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
| `sssp_vector(graph, out, source, rels, weighted, max_iter)` | distance from `source` (Bellman-Ford); unreachable is `Infinity` | iterations |
| `degree_vector(graph, out, direction := 'out', rels)` | distinct-neighbour degree | vector id |
| `aggregate_neighbours(graph, x, out, agg := 'mean', hops := 1, rels, direction := 'out')` | `mean`, `sum` or `max` of neighbours' rows of matrix `x` (a "one-layer GNN") | void |
| `triangles(graph, out, rels := NULL, persist := true)` | per-node triangle count in the undirected simple graph | vector id |

All take `version := …`. `label_propagation` is deterministic min-label
propagation, not majority-vote LPA.

```sql
SELECT warren.pagerank('roads', 'pr', weighted := false);
SELECT warren.sssp_vector('roads', 'dist', 1);                         -- 0, 10, 14, 21, 23
SELECT warren.vector_from_query('roads', 'emb', 'SELECT id, ARRAY[pop, 100 - pop]::real[] FROM city',
                                dtype := 'f32');
SELECT warren.aggregate_neighbours('roads', 'emb', 'emb_nbr');
SELECT * FROM warren.matrix_rows('roads', 'emb_nbr', NULL::int);       -- elm: {80,20}
```

The library names its transient intermediates `_warren_<function>_<part>`;
do not use names starting with `_warren_` for your own vectors. A failed run
leaves them in the session until `evict`.

## `eigen`

```
warren.eigen(graph, k, matrix := 'adjacency', which := 'largest', rels := NULL,
             weighted := NULL, symmetrize := true, tol := 1e-8, max_iter := NULL,
             vectors_prefix := NULL, version := NULL, dtype := 'f32', persist := true)
  RETURNS TABLE (i int, eigenvalue float8, residual float8)
```

Top-k eigenpairs by Lanczos with full reorthogonalisation and thick restart.

- `matrix`: `adjacency` (A), `laplacian` (D − A), `normalized_laplacian`
  (I − D^-1/2 A D^-1/2) or `random_walk` (D^-1 A). A directed graph is
  symmetrised as (A + Aᵀ)/2; `symmetrize := false` is accepted only for a
  graph that is already symmetric (undirected), and then halves the work.
- `which`: `largest` (by magnitude) or `smallest` (algebraic; use it for the
  Laplacians).
- `residual` is ‖Mx − λx‖₂ for the eigenvector as stored. With `f32` vectors
  (the default) it floors near 10⁻⁷ · ‖M‖; `dtype := 'f64'` doubles the
  memory and goes further.
- `vectors_prefix := 'lap'` stores the eigenvectors as vectors `lap_1 …
  lap_k` (persisted unless `persist := false`). Persisted eigenvectors pin
  their version against retention.
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
                compared_rows := 'shared', matrix := 'adjacency', which := 'largest', rels := NULL,
                weighted := NULL, symmetrize := true, tol := 1e-8, max_iter := NULL, dtype := 'f32',
                graph2 := NULL)
  RETURNS jsonb   -- {"method", "from", "to", ["graph2"], …the method's scalars}
```

Compares version `v1` with version `v2` (of `graph`, or of `graph2`). Rows
are aligned by dense id; across graphs, by external id.

| Method | Needs | Returns |
|---|---|---|
| `edit` | nothing | per relation and `overall`: `added`, `removed`, `union`, `distance` (Jaccard, `(added + removed) / union`); weights ignored |
| `degree` | nothing | `wasserstein` (W₁, in degrees), `ks`, over live nodes' out-degrees |
| `spectral` | `k`; optionally `vectors_prefix` | `l2`, `relative`, `max` of the eigenvalue difference, and `before`, `after`. With a prefix it uses the stored `<prefix>_1..k` (their Rayleigh quotients under `matrix`); without, it solves `k` eigenpairs on each side |
| `subspace` | `vectors_prefix`, `k` (run `eigen` on both versions first) | `projection` (‖P₁ − P₂‖_F), `max_angle`, `angles`: principal angles between the two eigenvector spans; the most sensitive to restructuring, the least to volume |
| `embedding` | `x` (matrix of `v1`), `y` (of `v2`, default `x`) | `mean_l2`, `max_l2`, `mean_cosine`, `max_cosine` (1 − cos), `rows` compared |

- `compared_rows` (`subspace`, `embedding`): `shared` (default) compares
  only nodes present on both sides; `all` counts absent rows as zero, so
  added and removed nodes dominate.
- Versions of one graph separated by a `compact` are refused.
- With `graph2`, both graphs need the same id type; a compaction is no
  barrier there.

```sql
SELECT warren.distance('roads', :v, :v, 'edit');
-- {"to": …, "from": …, "method": "edit", "overall": {"added": 0, "union": 10, "removed": 0, "distance": 0.0},
--  "relations": [{"relation": "highway", …}, {"relation": "lane", …}]}
```

[change-detection.md](change-detection.md) puts `eigen` and `distance` to
work.
