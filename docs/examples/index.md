# Example library

Build a graph of 10,000 people and 49,985 follows, then use SQL to explore
communities, propagate state, trace exposure, and try changes transactionally.
Every recipe includes runnable SQL and explains how to interpret its results.

Start with [the dataset setup](setup.md). You can then run any recipe in
any order. Each recipe also has a complete SQL download that includes setup
and runs in a fresh database, through either psql or pgAdmin's Query Tool.
The examples use functions available in pg_warren 0.7.0.

| Example | What you will build |
| --- | --- |
| [Traverse from your application data](bfs.md) | Look up a person, explore their neighbourhood, and join results back to your tables. |
| [Find communities with Louvain and Leiden](communities.md) | Label every person with a community and inspect the largest groups. |
| [Count walks with a semiring](walk-counts.md) | Count all six-hop walks from a person using `plus_times`. |
| [Compute distances with min plus](min-plus.md) | Implement shortest distances with `min_plus` and check them against BFS. |
| [Propagate a boolean through the graph](boolean-spread.md) | Spread an alert and save a reached flag for every person. |
| [Trace exposure to the nearest taint](taint.md) | Start from 100 tainted people, save nearest distances, and reconstruct an explanatory path. |
| [Try a change and roll it back](versions.md) | Build a staged version, query the proposed change, and undo it. |

## Use your own data

The graph uses your original table ids. A `node_id` returned by BFS or a
vector is a `people.id`, not an internal graph index. Replace the node and
edge queries in the setup with your application's queries, then replace
the sample name or taint selection with a business identifier or rule.

The graph is directed: `src` follows `dst`. Traversal defaults to following
those edges outward. Louvain and Leiden instead use the undirected view.
The dataset is synthetic, with no planted communities; discovered groups
demonstrate the algorithms rather than established real-world structure.

## Results and freshness

Queries read a built graph version. After changing people or follows, call
`warren.refresh('example_social')` and rerun analyses for that new version.
Vectors and saved route tables are computed results, not automatically
maintained annotations. Changing a seed selection also requires rerunning
the corresponding analysis.

The examples show correctness and composition with SQL. They are not
performance benchmarks; timings depend on hardware and workload. For
measured comparisons, see [performance](../../README.md#how-it-compares).
