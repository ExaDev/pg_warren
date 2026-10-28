# Trace exposure to the nearest taint

Mark 100 people as tainted, give every other person their minimum distance
from any taint, and retain a pointer that explains the route. This pattern
can represent downstream exposure to a flagged account or disrupted hub.
Run [the setup](setup.md) once, then run the following sections in order.

## Select the tainted people

<!-- sql:taint-seeds.sql -->

The hash ordering chooses 100 reproducible sample seeds. Replace it with
your actual business rule. These are ordinary `people.id` values; no
internal graph numbering is involved.

## Save the nearest distance

<!-- sql:taint-distance.sql -->

One multi-source BFS visits each reachable person at their shortest
distance from **any** seed. Tainted people receive zero; an untainted
person's distance is its nearest-taint distance along outward edges.
Unreachable people retain `Infinity` in the saved vector. With no seeds,
everyone would retain infinity.

For this dataset, the distance distribution is:

| Distance | People |
| --- | --- |
| 0 | 100 |
| 1 | 490 |
| 2 | 2,031 |
| 3 | 4,807 |
| 4 | 2,425 |
| 5 | 143 |
| 6 | 4 |

The graph holds two persisted vectors: `is_tainted` for seed membership
and `taint_distance` for distances. `vector_values` returns `node_id` and
`value`, which join directly to application tables.

## Save pointers and reconstruct a path

BFS already returns the predecessor as `via`. Save that original id as
`toward_taint`, alongside the distance. The table contains reached people
only; a left join from `people` can also show the unreachable ones.

<!-- sql:taint-routes.sql -->

Each pointer decreases the distance by one. Tainted seeds have distance
zero and a NULL pointer. For person 1316, the path ends at tainted person
2081, six hops away. A route contains one chosen shortest path; equally
near seeds or alternative shortest routes are not all enumerated.

## Be explicit about direction and freshness

Propagation follows edges **outward from a taint**. The pointers walk
back toward that taint, **against those directed edges**. If you instead
want paths that follow edges from a person to a taint, use
`direction := 'in'` in **both** BFS calls. For an undirected interpretation,
use `'both'` in both calls.

The vectors and route table are saved results, not live annotations.
Rerun all analysis sections when the tainted selection changes. If nodes
or edges change, refresh the graph first and then rerun the analysis.
Both BFS calls must use the same graph version and seed set; in a
concurrently changing application, use a `REPEATABLE READ` transaction
to keep that snapshot consistent across all sections.

In logistics, distance from disruption describes potential exposure.
Actual impact also depends on orders, stock, capacities, and alternatives,
which you can join to these graph results in SQL.
