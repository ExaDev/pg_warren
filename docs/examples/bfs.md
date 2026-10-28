# Traverse from your application data

Find how much of the network is reachable from a person, then return their
neighbours as ordinary application rows. Run [the setup](setup.md) once,
then execute this script in the same database.

## Traverse and join

<!-- sql:bfs.sql -->

The start array comes from a name lookup. In an application, use a bound
parameter for the name, email, or business identifier. The returned ids
already belong to your table; there is no internal-id translation step.

The first query visits each person once at their shortest distance from
the start. For `person-1`, its depth distribution is:

| Depth | People |
| --- | --- |
| 0 | 1 |
| 1 | 5 |
| 2 | 25 |
| 3 | 124 |
| 4 | 599 |
| 5 | 2,425 |
| 6 | 4,912 |
| 7 | 1,825 |
| 8 | 80 |
| 9 | 4 |

All 10,000 people are reached within nine hops. The second query returns
names and each node's predecessor, limited to three hops. The last query
returns a shortest path as an array of original ids.

## Change the starting set

The lookup can return several ids. BFS then measures the minimum distance
from **any** start, with all starts at depth zero. A lookup returning no
rows produces an empty start array and no traversal results.

Edges are followed outward. Use `direction := 'in'` to traverse against
them or `'both'` to ignore direction. A depth limit bounds hops, not the
number of rows; `max_results` can additionally cap the number visited.
See [traversal](../traversal.md) for filters and truncation semantics.
