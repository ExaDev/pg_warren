# Propagate a boolean through the graph

Start an alert at person 1 and spread it to everyone reachable along
outgoing follows. Save a reached flag on the graph. Run
[the setup](setup.md) once, then execute the whole script in one session.

## Spread and retain the alert

The `lor_land` semiring ANDs across an edge and ORs contributions from
incoming neighbours. Combining that result with the previous `seen`
vector preserves everyone reached in earlier rounds.

<!-- sql:boolean-spread.sql -->

In Messages, the reached population grows from 6 after one hop to 8,091
after six, then all 10,000 after nine. The final result agrees with BFS.

The result is the persisted vector **`seen`**, attached to the current
graph version. `vector_values` exposes its boolean values as numbers:
1 for true and 0 for false. The query converts them to SQL booleans with
`value <> 0` and joins their original ids back to your people.

## Apply it to a business rule

Replace the seed query with your application's flagged rows, returning
`(id, true)` for each. The default fill is false. Multiple starts propagate
in the same computation. This pattern models reachability or potential
exposure; a true flag alone does not establish real-world impact.

The ten-round limit and early-exit count of 10,000 belong to this dataset.
Adjust them for your graph. For shortest distances and an explanatory
route from multiple seeds, continue with [taint analysis](taint.md).
