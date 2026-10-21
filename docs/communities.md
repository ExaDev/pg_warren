# Communities

Some state belongs to a group of nodes rather than to each node: a fraud
ring's total suspicion, a topic cluster's centroid embedding, a
department's headcount. Keeping a copy on every member wastes memory and
goes stale; pg_warren keeps it once per community instead, on a **quotient
graph** with one node per community, and moves vectors between the two
levels (DESIGN.md §6.6).

| Call | What it does |
|---|---|
| `contract(graph, label_vector, out_graph)` | a new version of graph `out_graph`: one node per distinct label, the edges between communities aggregated |
| `project(graph, x, out_graph, y, agg := 'sum')` | `y` on the quotient: each community's `sum`, `mean`, `max`, `min` or `count` of `x` over its members (`Pᵀ·x`) |
| `lift(out_graph, y, graph, x)` | `x` on the graph: each node gets its community's row of `y` (`P·y`) |

The examples continue those of [compute.md](compute.md), on the `roads`
graph; they run as written in `test/sql/docs_examples.sql`.

## Contracting a graph

A **label vector** names each node's community. Any numeric vector of
integers works (`i64`, or `f64`/`f32` holding whole numbers), such as the
output of `components_vector` or `label_propagation`, or one you load with
`vector_from_query`:

```sql
SELECT warren.vector_from_query('roads', 'region',
  'SELECT id, CASE WHEN id <= 2 THEN 1 ELSE 2 END FROM city');
SELECT warren.contract('roads', 'region', 'regions');   -- returns the new version's id
```

`regions` is a graph like any other: its node ids (`bigint`) are the
labels, so results join back to them; it has versions, retention, vectors,
and every traversal and compute function works on it. Each label is one
node; the weight of an edge between two communities is the sum of the
weights of the edges joining their members (or their count, with
`weighted := false`), and the edges inside a community make its self-loop
(left out with `drop_self_loops := true`). An undirected graph gives an
undirected quotient. The quotient is the matrix product `PᵀAP` of the
stored adjacency, so an undirected edge inside a community counts twice
in its self-loop (both its stored directions) and a self-loop once, as
each counts in its nodes' degrees: a community's weighted degree is the
sum of its members', and the total edge weight is kept. Relations are
kept, each contracted on its own.

```sql
SELECT warren.vector_const('regions', 'one', 1);
SELECT warren.spmv('regions', 'one', 'km_out');       -- each region's weighted degree
SELECT node_id AS region, value AS km FROM warren.vector_values('regions', 'km_out', NULL::bigint);
-- region 1: 54 (ash–birch, 10 km, inside: counted both ways, + 34 km to region 2);
-- region 2: 52 (9 km inside, twice, + 34): each region's members' summed degree
```

Contracting again into the same name adds a version (`promote := false`
leaves it staged). A label seen before keeps its dense id, so vectors on two
versions of the quotient compare by label. `warren.refresh` and
`warren.compact` refuse a contraction: run the labelling again and
`contract` again. So a quotient's id space grows with every label it has
ever seen (labels that disappear are tombstoned) and cannot be compacted:
when labels change wholesale, as a fresh labelling's usually do, contract
into a new graph instead. Only versions of one parent graph can go into a
quotient. A quotient can itself be contracted, which gives a
hierarchy of communities.

Labels must be integers: a NaN, an infinity or a fraction is refused
(22023), as is a `bool` vector (42804). Deleted (tombstoned) nodes belong
to no community.

## Moving vectors between the levels

A quotient version remembers the version it was contracted from and the
membership of each node. `project` reads `x` on exactly that version of the
parent and writes `y` on the quotient; `lift` does the reverse.

```sql
SELECT warren.vector_from_query('roads', 'pop', 'SELECT id, pop FROM city');
SELECT warren.project('roads', 'pop', 'regions', 'population');            -- sum
SELECT warren.project('roads', 'pop', 'regions', 'largest', agg := 'max');
SELECT warren.lift('regions', 'population', 'roads', 'region_population');
SELECT node_id, value FROM warren.vector_values('roads', 'region_population', NULL::int);
-- ash and birch: 150; cedar, dale and elm: 110
```

`project` reads `x` of any element type and width; the result is `double
precision` (`bigint` for `count`, which counts each community's members).
On a matrix it works column by column, so `agg := 'mean'` of an embedding
matrix gives each community's centroid. `lift` keeps `y`'s type and width.
Both take `persist := false` for a transient result, and `version :=` to
use an older version of the quotient.

## Lineage, retention and access

- `warren.info(out_graph)` reports `"source": "contraction"` and, for each
  version, `parent_version`, `parent_graph` and `label_vector` (the label
  vector's id; NULL once that vector is gone, or if it was transient).
- A version of the parent that a quotient version was contracted from is
  kept beyond `retain_versions`, with a `NOTICE`, so `project` and `lift`
  keep working. Dropping it anyway (`drop_version`, or `drop_graph` of the
  parent) keeps the quotient but unlinks it: `project` and `lift` then fail
  with 55000.
- To contract, a role must be able to read the parent graph. The quotient
  it creates is its own (it may `grant` it). Contracting into an existing
  quotient needs membership in that quotient's owner role. `project` and
  `lift` need read access to both graphs, and the usual rules for replacing
  a vector (DESIGN.md §7).
