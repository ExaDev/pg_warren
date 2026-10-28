# Count walks with a semiring

Count how many directed walks of exactly six edges lead from one person
to each other person. Run [the setup](setup.md) once, then execute this script.

## Multiply and add along the network

A semiring chooses how to combine a value with an edge, and how to combine
contributions from neighbours. `plus_times` multiplies an edge weight by a
neighbour's value, then adds the contributions. Here every edge has weight 1.

<!-- sql:walk-counts.sql -->

The initial vector is 1 at person 1 and 0 elsewhere. Each multiplication
extends every walk by one edge. After six products, `walk6` stores the
number of six-edge walks ending at each person. `person-8097` receives 11
such walks on this dataset. `total_walks` counts their multiplicities,
while `distinct_endpoints` counts the people with a nonzero result.

These are **walks**, so nodes and edges may repeat. The result is neither
a count of simple paths nor a shortest distance. The script only uses six
steps; walk counts grow rapidly, and the default floating-point vectors
cannot represent arbitrarily large integers exactly.

## Follow the correct direction

The adjacency matrix has `A[src, dst] = 1`. With `transpose := true`, each
destination receives contributions from its incoming neighbours: values
propagate outward from the seed. Without transpose, each node reads its
outgoing neighbours' values instead. See [semiring products](../compute.md#semiring-products-spmv-and-spmm).
