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
| `louvain(graph, out)`, `leiden(graph, out)` | a label vector of communities that maximise modularity ([below](#finding-communities-louvain-and-leiden)) |

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
quotient, and only while one of its versions is still linked to that
graph: once the parent graph or every linked version is dropped, the
quotient takes no new versions (22023), so contract into a new graph.
Two crossed contractions in concurrent transactions (`q1` into `q2` and
`q2` into `q1`), or a `contract` and a `refresh` of the same parent in two
concurrent transactions, can deadlock (40P01): retry the one Postgres
cancels. A quotient can itself be contracted, which gives a hierarchy of
communities.

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

## Finding communities: Louvain and Leiden

`louvain` and `leiden` find communities that maximise **modularity**: how
much more edge weight a partition keeps inside its communities than a
random graph with the same degrees would. Each writes a label vector, so
`contract` and `project` take its result directly.

| Call | What it does |
|---|---|
| `louvain(graph, out, resolution := 1, seed := 0, max_levels := 100, tolerance := 1e-7, weighted, levels_prefix, version)` | Louvain (Blondel et al. 2008); returns the partition's modularity |
| `leiden(graph, out, resolution := 1, seed := 0, max_levels := 100, tolerance := 1e-7, randomness := 0.01, weighted, levels_prefix, version)` | Leiden (Traag et al. 2019): every community it finds is connected |
| `modularity(graph, labels, resolution := 1, weighted, version)` | the modularity of any label vector's partition |

```sql
SELECT warren.louvain('roads', 'community');            -- 0.0397: the modularity
SELECT node_id, value FROM warren.vector_values('roads', 'community', NULL::int);
-- ash, dale and elm: 0; birch and cedar: 1
SELECT warren.modularity('roads', 'region');            -- the hand-made regions: -0.142
```

Both treat the graph as undirected (an edge and its reverse add up) and
use edge weights if `weighted` (NULL: if the graph has them). A community
is labelled by the smallest dense id among its members, so labels are
stable for a given graph version and seed. `resolution` above 1 favours
smaller communities, below 1 larger ones.

Both work in levels. A level moves nodes to the neighbouring community
that most raises modularity, revisiting the neighbours of each node that
moved until none moves (Traag et al.'s fast local moving), then contracts the level's graph by the communities found, so the next
level moves whole communities. Louvain stops when a level raises
modularity by no more than `tolerance`, as NetworkX's `threshold` does.
Leiden refines each community into well-connected parts before it
contracts, so no community it returns is split in two, or held together
only through another community. It continues while the refinement still
merges nodes, as Traag et al.'s does; `tolerance` ends each level's moving
instead. `randomness` is θ of the refinement's random merges (0: always
the best merge). `levels_prefix := 'lv'` also keeps each level's
partition, as vectors `lv_1`, `lv_2`, …, coarsest last: a hierarchy of
communities.

The result is the same for any number of threads (`warren.threads`), and
for a given `seed`. Different seeds give different, comparably good
partitions. Modularity counts an undirected self-loop once in its node's
degree, as `contract` does, so a partition scores the same on a graph and,
as singletons, on its quotient. NetworkX counts it twice, so the two agree
on graphs without self-loops.

The steps are SQL functions too, for programs of your own:
`local_moving(graph, out, initial, keys, resolution, seed, max_sweeps,
tolerance, weighted, persist, version)` (one level's moving, from the
partition `initial` or from each node alone; returns the number of
communities) and `leiden_refine(graph, communities, out, keys, resolution,
seed, randomness, weighted, persist, version)`. `louvain` and `leiden` are
PL/pgSQL over them, `contract`, `project` and `lift`
(`crates/pg_warren/src/community_sql/library/`). Graph names beginning
with `_warren_` are reserved for their levels (42939): `contract(graph,
labels, NULL)` makes a new graph of the caller's named
`_warren_level_<version id>`, after the id it returns.

At 1M nodes and 5M undirected edges on 4 vCPUs, `louvain` takes 6.6 s and
`leiden` 13.4 s; at 10M nodes, 95 s and 213 s ([limits.md](limits.md)).
Each level writes its graph as a version (segments and WAL), and the rows
of a level's graph dropped during the run are reclaimed only by `VACUUM`
after its transaction ends, so a run needs free disk for every level: on
that 1M-node graph, whose own segments take 75 MB, `louvain` wrote 166 MB
of segments and about as much WAL, `leiden` (14 levels) 511 MB and 670 MB.

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
