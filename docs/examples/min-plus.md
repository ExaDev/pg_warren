# Compute distances with min plus

Build shortest distances using sparse matrix products, then check every
answer against native BFS. Run [the setup](setup.md) once and execute the
whole script in one SQL session.

## Relax distances until they stop changing

`min_plus` adds an edge's weight to its neighbour's distance, then takes
the smallest contribution. In this unweighted graph each edge costs 1.
The seed starts at zero and every other node at positive infinity.

<!-- sql:min-plus.sql -->

`accumulate := 'min'` keeps an existing distance unless a shorter one is
found. Each round extends the search by another edge; the loop stops when
no distance improves. Messages show convergence after ten rounds on this
dataset: nine rounds discover nodes, and the tenth confirms stability.

The persisted `distance` vector contains a value for every live node.
The farthest people are nine hops away. The final cross-check reports
**zero mismatches** against BFS. Unreachable nodes would retain infinity;
the example's ring makes everyone reachable.

## Choose the right primitive

For this unweighted task, native BFS is the simpler operation. This
example exposes the algebra underneath: on a graph whose nonnegative
weights represent travel times, `min_plus` instead relaxes travel-time
distances. Use Dijkstra for a single weighted path or the
`sssp_vector` library function for weighted distances; see
[compute](../compute.md#the-algorithm-library).

`next_distance` and `changed` are transient scratch vectors. `distance` is
persisted and can be read by later sessions. The 100-round limit protects
this demonstration from an unbounded loop; a different graph may need
more rounds. See [vectors](../compute.md#vectors) for lifetime and version rules.
