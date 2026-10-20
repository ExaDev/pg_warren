# Traversal

Traversal functions take external ids in and return rows keyed by external
id, so you join their results back to your tables. They read a version,
never the live tables ([semantics.md](semantics.md)).

The examples use the `roads` graph of [compute.md](compute.md) (undirected,
weighted, `integer` ids, relations `highway` and `lane`) and the README's
`social` graph (directed, `bigint` ids), in the state semantics.md leaves it
in: `follows` is 1→2, 2→3, 3→1, 3→4, 4→6, 6→7. They run as written in
`test/sql/docs_examples.sql`.

## Common arguments

| Argument | Meaning |
|---|---|
| `graph` | the graph's name, always first |
| start / node ids | external ids of the graph's id type, exactly (`1::bigint` on a `bigint` graph; [semantics.md](semantics.md#id-types-and-type-witnesses)) |
| `direction` | `out` (default: along edges), `in` (against them; needs the reverse CSR), `both` |
| `rels` | relations to follow; NULL (default) means all, `'{}'` none; unknown names are skipped |
| `max_depth` | hops at most; NULL means unbounded |
| `version` | the version to read; NULL (default) means the current one |

Ids the version does not have, or has only as tombstones, are skipped as
starts and find nothing as lookups; they are never errors. A traversal over
an undirected graph sees each edge in both directions.

## Lookups

| Function | Returns |
|---|---|
| `neighbours(graph, node, direction := 'out', rels := NULL)` | each distinct neighbour once, in dense-id order |
| `degree(graph, node, direction := 'out', rels := NULL)` | the number of distinct neighbours (`bigint`) |
| `has_edge(graph, src, dst, rel := NULL)` | whether `src → dst` exists, in relation `rel` if given |
| `reachable(graph, start, target, max_depth := NULL, rels := NULL, direction := 'out')` | whether `target` can be reached (an unknown node reaches nothing, not even itself) |

```sql
SELECT warren.neighbours('roads', 4, rels := ARRAY['lane']);          -- 1, 5
SELECT warren.degree('roads', 4), warren.degree('roads', 4, rels := ARRAY['highway']);   -- 3, 1
SELECT warren.has_edge('roads', 1, 2), warren.has_edge('roads', 1, 2, 'lane');           -- true, false
SELECT warren.reachable('roads', 1, 5, max_depth := 1), warren.reachable('roads', 1, 5, max_depth := 2);
                                                                      -- false, true
SELECT warren.neighbours('social', 3::bigint, 'both');                -- 1, 2, 4
```

`degree`, `has_edge` and `reachable` return no ids, so an `integer` id works
on a `bigint` graph; `neighbours` needs the exact type.

## Breadth-first search

```
warren.bfs(graph, start anyarray, max_depth int, direction := 'out', rels := NULL,
           node_filter := NULL, max_results := NULL, version := NULL)
  RETURNS TABLE (node_id, depth int, via)
```

One row per node reached within `max_depth` hops of any start, with its
depth and `via`, the node it was first reached from (NULL for a start).
Starts are depth 0, and a start listed twice counts once.

- `node_filter` (an array of ids) is the set of nodes the search may
  **enter**: it prunes the search rather than filtering its output. Starts
  are always admitted.
- `max_results` stops the search after that many rows. Stopping is
  **truncation**: a `NOTICE` says so and `last_stats() -> 'truncated'` is
  true. Reaching `max_depth` is not truncation.

```sql
SELECT * FROM warren.bfs('roads', ARRAY[1], 2) ORDER BY depth, node_id;
-- (1, 0, NULL), (2, 1, 1), (4, 1, 1), (3, 2, 2), (5, 2, 4)
SELECT * FROM warren.bfs('roads', ARRAY[1], 3, node_filter := ARRAY[1, 2, 3]) ORDER BY depth, node_id;
-- (1, 0, NULL), (2, 1, 1), (3, 2, 2): 4 and 5 are never entered
SELECT * FROM warren.bfs('social', ARRAY[3::bigint], 2, direction := 'in') ORDER BY depth, node_id;
-- (3, 0, NULL), (2, 1, 3), (1, 2, 2): who reaches 3
```

`reachable_set(graph, start anyarray, max_depth := NULL, rels := NULL,
direction := 'out')` returns the set of nodes reached, starts included,
without depths.

```sql
SELECT warren.reachable_set('roads', ARRAY[3], rels := ARRAY['highway']);   -- 3, 4
```

`bfs` and `reachable_set` stream, one level at a time, but only in the
select list. In `FROM`, Postgres runs them to completion before `LIMIT`
applies ([limits.md](limits.md#known-limitations)); bound them with
`max_depth` or `max_results` there.

## Paths

| Function | Returns |
|---|---|
| `shortest_path(graph, start, target, max_depth := NULL, rels := NULL, direction := 'out')` | the fewest-hop path as an id array, start to target; NULL if none |
| `dijkstra(graph, start, target, rels := NULL, direction := 'out')` | one row `(path, cost)`: the cheapest path by summed weight (1 per edge when unweighted); no row if none |

```sql
SELECT warren.shortest_path('roads', 1, 5);                           -- {1,4,5}: 2 hops
SELECT * FROM warren.dijkstra('roads', 1, 5);                         -- {1,2,3,4,5}, 23 km
SELECT * FROM warren.dijkstra('roads', 1, 5, rels := ARRAY['lane']);  -- {1,4,5}, 32 km
SELECT warren.shortest_path('social', 7::bigint, 1::bigint, direction := 'in');   -- {7,6,4,3,2,1}
```

- `shortest_path` is a bidirectional BFS; ties go to the smallest dense id,
  so the result is deterministic.
- `dijkstra` uses each relation's own weights (an edge in two selected
  relations costs the cheaper one). A negative or NaN weight is an error
  when the search reaches it; an infinite cost is never followed.

## Components

`components(graph, NULL::<id type>)` labels every live node with its
connected component, ignoring direction (weakly connected). A component's id
is its smallest dense id, so labels are deterministic and comparable across
versions until a `compact`.

```sql
SELECT * FROM warren.components('roads', NULL::int);                  -- all five: component 0
```

`components_vector` ([compute.md](compute.md#the-algorithm-library))
computes the same labels as a vector.

## `walk`: typed path patterns

A walk is an ordered list of **steps**. Each step follows some relations, in
a direction, for a range of hops, and may only enter nodes in its filter. The
whole chain runs in the engine, so a predicate on an intermediate step prunes
the traversal instead of filtering its output.

```
warren.walk(graph, start anyarray, steps warren.step[], mode := 'ends',
            distinct_ends := true, max_results := NULL, reverse := false, version := NULL)
  RETURNS TABLE (end_id, path anyarray, rels text[], count bigint)

warren.s(rels text[], direction := 'out', filter := NULL, min_hops := 1, max_hops := NULL)
  RETURNS warren.step
```

A step (`warren.step`, a composite type) has:

| Field | Meaning |
|---|---|
| `rels` | relations to follow; NULL means all |
| `direction` | `out` (default), `in`, `both` |
| `filter` | ids the step's hops may enter, and the step may end on; NULL means any. Starts are not filtered |
| `min_hops`, `max_hops` | the hop range; `max_hops` defaults to `min_hops`, so a step is one hop by default. `min_hops` may be 0 (the step may end where it started); `max_hops` is at most 1,024 |

Hops are walks, as in a recursive CTE: nodes and edges may repeat, and a step
ends on every node some walk of a length in its range reaches.

```sql
-- from ash, one highway hop, then one lane hop
SELECT * FROM warren.walk('roads', ARRAY[1], ARRAY[warren.s(ARRAY['highway']), warren.s(ARRAY['lane'])]);
-- end_id 3
SELECT * FROM warren.walk('roads', ARRAY[1], ARRAY[warren.s(ARRAY['highway']), warren.s(ARRAY['lane'])],
                          mode := 'paths');
-- 3 | {1,2,3} | {highway,lane}
```

**Modes.**

| `mode` | Rows |
|---|---|
| `ends` (default) | one per end node (`end_id`); with `distinct_ends := false`, one per walk |
| `paths` | `end_id`, `path` (the ids, start first) and `rels` (each hop's relation); one path per end node, or with `distinct_ends := false` every walk |
| `count` | one row, `count`: the number of distinct ends, or of walks with `distinct_ends := false`. Exact, never materialised |

```sql
SELECT end_id FROM warren.walk('roads', ARRAY[1], ARRAY[warren.s(NULL, 'out', NULL::int[], 2, 2)]);
-- 1, 3, 5: two hops from ash
SELECT count FROM warren.walk('roads', ARRAY[1], ARRAY[warren.s(NULL, 'out', NULL::int[], 2, 2)],
                              mode := 'count', distinct_ends := false);
-- 5 walks: 1-2-1, 1-2-3, 1-4-1, 1-4-3, 1-4-5
```

A walk that returns a row per walk (`paths`, or `ends` without
`distinct_ends`) can be exponential in its hops, so it is always bounded:
`max_results` defaults to 100,000 rows, and a larger `max_results` than
`warren.max_walk_paths` is refused ([limits.md](limits.md#settings)).
Stopping at the bound is truncation, with a `NOTICE`. `walk` collects its
rows before returning the first.

**`reverse := true`** runs the chain from its last step back to its first,
directions flipped. `start` is then the set of allowed **ends**, and
`end_id` reports the nodes the walk could have **started** from. Use it when
the far end of a pattern is rarer than the near end:

```sql
SELECT * FROM warren.walk('roads', ARRAY[3], ARRAY[warren.s(ARRAY['highway']), warren.s(ARRAY['lane'])],
                          reverse := true);
-- end_id 1: the walks highway-then-lane that end at cedar start at ash
```

**Steps as jsonb.** For application code, `warren.steps(jsonb)` (and the
cast `jsonb::warren.step[]`) builds the step array from an array of objects
with the optional keys `rels`, `direction`, `filter`, `min_hops`, `max_hops`
and the same defaults. An unknown key or a malformed value is an error
(`invalid_parameter_value`), so a typo never widens a walk.

```sql
SELECT * FROM warren.walk('roads', ARRAY[1],
    '[{"rels": ["highway"]}, {"rels": ["lane"], "filter": [3, 5]}]'::jsonb::warren.step[]);
-- end_id 3
```

A walk is a chain: there is no variable binding or join inside a pattern.
Express cycles or "A and B share a C" by intersecting two walks' `end_id`s in
SQL.

## `last_stats()`

`warren.last_stats()` describes this session's last call to `bfs`, `walk`,
`reachable`, `reachable_set`, `shortest_path`, `dijkstra` or `components`, as
`jsonb` (NULL before the first):

| Key | Meaning |
|---|---|
| `function`, `graph`, `version` | what ran, on which version |
| `nodes_visited` | nodes admitted (starts included); a filter lowers it |
| `edges_traversed` | CSR row entries scanned |
| `truncated` | whether `max_results` or a ceiling stopped the call early |

```sql
SELECT * FROM warren.bfs('roads', ARRAY[1], 3, max_results := 2);      -- NOTICE: truncated
SELECT warren.last_stats();
-- {"graph": "roads", "version": …, "function": "bfs", "truncated": true, "nodes_visited": 2, "edges_traversed": 2}
```

After a streaming call stopped by `LIMIT`, it counts the work actually done.
