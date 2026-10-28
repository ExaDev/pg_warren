# Find communities with Louvain and Leiden

Assign every person a community, compare group sizes, and inspect their
members. Run [the setup](setup.md) once, then execute this script.

## Find and inspect communities

<!-- sql:communities.sql -->

Each algorithm persists a label vector: `louvain` or `leiden`. A label is
a community identifier, not an importance score or a `people.id`. Read it
with `vector_values` and join `node_id` back to `people`.

The functions return modularity, a measure of how much more edge weight
stays inside groups than expected under a model with the same degrees.
The summary counts communities and reports their smallest and largest
sizes. The last query shows sample members of the largest Louvain groups.

On pg_warren 0.7.0 with this dataset and seed 42, Louvain found 20 groups
with modularity about 0.262; Leiden found 22 with modularity about 0.295.
These are example results, not a guarantee for future algorithm versions
or a general claim that Leiden always scores higher.

## Interpret the grouping

Both algorithms use the **undirected view**, even though the source graph
is directed. Different seeds can produce different partitions. Higher
`resolution` favours smaller communities; lower values favour larger ones.

This synthetic network has no planted group structure. On real data,
inspect communities against external attributes, such as geography or
operating region, before assigning business meaning to them. See
[communities](../communities.md) for the algorithms and their settings.
