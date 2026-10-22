# Maintained analytics (design note)

*Status: proposal, 2026-10-22. Nothing here is decided or implemented. It
extends DESIGN.md §10, "Incremental maintenance". The open questions at the
end would become §14 items when decided.*

## The problem

A knowledge graph that streams new facts in all day has one structural
version after another. Today every whole-graph result (`pagerank`,
`components_vector`, `sssp_vector`, `louvain`, `leiden`) is computed from
scratch on the version it is asked for. Each `refresh` throws away what the
previous version's results already knew. Most of the graph did not change,
so most of that work is repeated.

DESIGN.md §10 plans the structural half of the answer:
- deltas captured by triggers or logical replication into `warren.deltas`;
- reads over the CSR plus an overlay;
- compaction into a new version.

That keeps the *graph* current, so a whole-graph computation is always ready
to run. This note covers the other half: keeping *results* current. A
whole-graph computation registered once is then already run on every new
version, at a cost that follows the size of the change, not the size of the
graph.

## What is already in place

- **Stable dense ids across versions** (DESIGN.md §5.2). Node `42` in
  version 7 is node `42` in version 8, so a vector on version 7 is already
  aligned with version 8, apart from appended nodes and new tombstones.
- **Structural diffs** (`warren_core::version::diff`, DESIGN.md §7). The
  node and edge deltas between two versions in one id space cost
  O(N + M), computed by merging the two CSRs row by row.
- **Vectors bound to one version** (DESIGN.md §6.2). A result already
  carries the version it is valid for.
- **Parent links** (`versions.parent_version`, DESIGN.md §6.6). These are
  used for contraction today; they are the same kind of lineage this note
  needs between a result and the result it was updated from.

What is missing is **provenance**. Nothing records that a vector *is*
`pagerank(damping 0.85, tol 1e-6, rels {cites})` on version 7. A row in
`warren.vectors` has a name, a dtype and a version, not the computation that
made it. Without provenance nothing can update the result.

## The idea

**Register a result as maintained.** Each maintained result records:
- its algorithm and arguments;
- the version it is valid for;
- any companion state its update rule needs.

**Update instead of recompute.** When a new version of the graph appears, the
engine takes three inputs: the result on the parent version, the
`diff` from parent to new version, and the companion state. From them it
produces the result on the new version.

**The update rule follows the computation's algebra.** It is not hand-written
per call site.

| Class | Examples in pg_warren | Update | Guarantee |
|---|---|---|---|
| Selective `⊕` (`min`, `max`, `lor`): monotone fixpoints | `sssp_vector` (`min_plus`), `components_vector` and `label_propagation` (min-label), BFS levels | Insertions: restart the fixpoint with a frontier of the changed edges' targets. Deletions: trim the values the deleted edges supported, reset them, re-run from their boundary | The same fixpoint as a recompute |
| Linear fixpoints `x = αAᵀx + b` | `pagerank`, personalised PageRank, `katz_centrality` | Residual push of the correction from the rows that changed | Within the recompute's tolerance, with a stated bound |
| Heuristics | `louvain`, `leiden`, `label_propagation_majority` | Warm start from the parent version's communities; local moving on the affected nodes only; re-aggregate | Quality, not equality: a modularity gap against a recompute, and a drift policy |

These three classes cover the library's common computations. Eigenvectors
(`eigen`, `eigenvector_centrality`) and the sampled centralities are out of
scope here. Warm-starting Lanczos from the parent version's Ritz vectors is a
separate question.

### Selective semirings

For `⊕` in {`min`, `max`, `lor`}, the library's iterations converge to the
least (or greatest) fixpoint. That fixpoint is a minimum over paths and does
not depend on the order of evaluation. So any schedule that reaches it gives
the same vector. For `min_plus` on floats each path's length is a left fold
along the path, the same in both schedules. The property tests must confirm
this before it is claimed.

- **Edges added.** Values can only improve. Seed the frontier with each added
  edge's target, valued `x[source] ⊗ w`, and run the usual masked product
  until nothing changes. The work is proportional to the region the
  improvement reaches.
- **Edges removed** (or weights worsened, or nodes tombstoned). A value
  whose best path used a removed edge is no longer supported. Two options:
  - *Dependency trimming*, as in KickStarter. Keep one parent per node, the
    neighbour its value came from, as a companion `i64` vector (8 bytes per
    node). Reset the subtree hanging from each removed parent edge to the
    identity. Re-seed it from its live in-neighbours, then propagate.
  - *Scoped recompute* where parent pointers are meaningless.
    `components_vector`'s min-label can split a component, so recompute just
    the components that lost an edge. When the change touches the giant
    component this degrades to a full recompute, and the note says so rather
    than promise otherwise.

### Linear fixpoints (PageRank)

`pagerank` solves `x = d·Pᵀx + d·(dangling mass)·v + (1 − d)·v`, where `v`
is the personalisation (uniform `1/N` over live nodes, or `seed / sum(seed)`)
and both the teleport and the dangling nodes' rank go to it. On a new
version `P` becomes `P′`. Evaluate the residual `r` of the parent's `x` in
the new system:
- `r` is non-zero only at the out-neighbours (old and new) of nodes whose
  rows changed;
- it also has two terms that touch every node, described next.

Push `r` from where it is large (Gauss–Southwell; Andersen–Chung–Lang
push). The error after stopping is bounded by
`‖x − x*‖₁ ≤ ‖r‖₁ / (1 − d)`. Stopping at the same `‖r‖₁` the full
iteration reaches therefore gives a result within the recompute's tolerance,
and the bound can be reported.

Two terms touch every node and need care:
- **The uniform personalisation `1/N`.** Appending or tombstoning nodes
  changes `N`, and so `v` everywhere. The solution is linear in `v`, so this
  is a rescale of the parent's `x` by `N_parent / N`, plus residual at the
  new nodes. It is not a reason to recompute. A seeded `v` changes only
  where the seed does.
- **The dangling mass.** A node gaining its first or losing its last
  out-edge changes the mass spread over `v`. Treat it as a rank-one
  correction along `v`: one scalar per update.

Results equal a recompute *to tolerance*, not bit for bit. A maintained
PageRank and a fresh `pagerank` on the same version can differ in the last
digits, and the note must be explicit about that. Bit-for-bit equality holds
only for the selective class.

### Heuristics (Louvain, Leiden)

Warm-start local moving from the parent version's communities. Only the
*affected* nodes move: endpoints of changed edges, widened by delta-screening
or a dynamic frontier (Zarayeneh & Kalyanaraman; Sahu's DF Louvain). Then
refine (Leiden) and re-aggregate as usual. Two consequences:
- **No equality guarantee.** A recompute from singletons can land in a
  different local optimum, so the guarantee is a measured quality bound.
  Tests assert that maintained modularity is within ε of a recompute's on
  generated version histories.
- **Drift policy.** Quality can drift over many small updates. The policy
  would be something like "recompute from scratch when modularity has fallen
  more than ε below the last full run, or after k updates". It belongs with
  the registration so it is explicit.

## The SQL surface (sketch)

```sql
-- register: computes once, then keeps the result current
warren.maintain(graph text, name text, algorithm text, args jsonb DEFAULT '{}',
                policy jsonb DEFAULT '{}') RETURNS bigint;
warren.unmaintain(graph text, name text) RETURNS void;
-- the result on a version: computed, updated from its parent, or recomputed
warren.vector_values(graph, name, version)   -- unchanged; maintenance is invisible here
warren.maintained(graph text) RETURNS TABLE (name text, algorithm text, args jsonb,
                                             valid_version bigint, last_full_version bigint,
                                             bound float8, ...);
```

A catalog table `warren.maintained` records the provenance:
- the algorithm and its arguments;
- the relations and weighting;
- the vector it writes;
- the version it is valid for;
- the last version on which it was recomputed in full, rather than updated;
- its companion state: parent pointers, the residual, the previous
  communities.

That state is stored as ordinary vectors bound to the same version, so
retention, compaction moves and the cache treat it as they treat any other
vector.

**Compaction** (DESIGN.md §5.2) renumbers the id space. It moves bound
vectors by the renumbering, and maintained results and their companions move
with them. The next update then diffs within the new id space, so a
compaction costs nothing extra.

**Staged versions** (DESIGN.md §8) gain something too. The change-detection
workflow can update the maintained results onto the staged version, and
compare PageRank or the communities before and after the batch, before
deciding to `COMMIT` or `ROLLBACK`. That uses the same machinery at a small
fraction of the cost of recomputing on the staged version.

## When updates run

- **Synchronously in `promote`** (and in `refresh(promote := true)`). The
  current version and its results then always agree, but the promoting
  transaction pays for every maintained result. Simple and transactional;
  probably the default for a first version.
- **Asynchronously, by the background compute worker** (DESIGN.md §6.9,
  1.x). Promotion stays fast; results lag by a bounded amount, and
  `warren.maintained` reports which version each one is valid for. A read of
  a result not yet valid on the requested version either waits, updates it on
  demand, or reads the latest valid one, as policy says.
- **With the §10 overlay.** Results stay bound to versions. They are updated
  per compaction of the overlay into a version, not per delta. Updating per
  overlay batch would need results valid on the overlay's view, which is not
  a version. That is a later question, and probably not worth it.

## How it is tested

The test oracle is a full recompute on the same version. That gives the TDD
shape for every rule:
1. Generate a version history: a base graph plus random insert, delete and
   mixed batches, including tombstoned nodes and appended nodes. The
   fixtures in `bench/fixtures/evolving.py` already produce volume, bridge
   and novel-node ingest.
2. Maintain the result along the history.
3. At every version, compare it with a recompute:
   - selective class: identical;
   - linear class: within the bound;
   - heuristics: modularity within ε.

These are `proptest`s in `warren-core` against in-memory versions, and
`#[pg_test]`s for the catalog, the promotion hook, retention and compaction.

## The first experiment

PageRank only, in `warren-core`, behind a plain function. No catalog and no
SQL yet.

1. Write the property test first: on generated histories, the updated vector
   is within tolerance of a recompute.
2. Implement the residual push with the two corrections that touch every
   node.
3. Measure time and peak memory against batch size, from 1 edge to 10% of
   edges, on the evolving fixtures and a random graph of 1M nodes. Find the
   crossover where recompute wins, and look at hub deletion (the bad case
   for push).

That one plot says whether the rest is worth building. Components and SSSP
(selective) follow; Louvain last.

## Prior art and where this sits

Literature checked 2026-10-22. The update rules are known; their
integration is not.
- **Automatic rule selection from algebra.** Ingress (Gong et al., PVLDB 14,
  2021) picks a memoisation policy automatically from the aggregation's
  algebra, for vertex-centric programs in a standalone engine.
- **The selective and linear rules.**
  - KickStarter (Vora et al., ASPLOS 2017): dependency trimming under
    deletions.
  - GraphBolt (Mariappan & Vora, EuroSys 2019) and DZiG (EuroSys 2021):
    exact refinement of iterative algorithms such as PageRank.
  - RisGraph (Feng et al., SIGMOD 2021): per-update incremental BFS, SSSP
    and WCC. It also reports differential dataflow about 78× slower on BFS
    over Twitter-2010.
- **Versioned snapshots.** TEGRA (Iyer et al., NSDI 2021) and CommonGraph
  (ASPLOS 2023) compute across snapshots, outside a database.
- **Dynamic-frontier PageRank and Louvain.** Sahu (arXiv 2401.15870,
  2404.19634) gives the warm-start rules for the heuristic class.
- **General incremental view maintenance.**
  - DBSP and Feldera (Budiu et al., PVLDB 16, 2023) and Materialize
    (`WITH MUTUALLY RECURSIVE`) maintain any recursive query.
  - Ammar et al. (PVLDB 15, 2022) measure differential dataflow's memory on
    recursive graph queries.
  - pg_trickle is general IVM inside Postgres. It claims maintained PageRank
    and components written as SQL, with no exactness guarantee.

What looks open:
- graph analytics as maintained results *inside the database*, bound to
  transactional versions, with a per-version guarantee against a recompute;
- one registration across a library that mixes selective, linear and
  heuristic computations, with the rule chosen from the semiring;
- a head-to-head against general IVM (Feldera, Materialize, pg_trickle) on
  time, memory and correctness.

The bet is that knowing the semiring, and owning the CSR, removes the
per-iteration difference traces that general IVM keeps. If so, the saving is
large in memory at equal or better latency.

## Open questions (future §14 items)

1. **Provenance.** Its own catalog table (`warren.maintained`), or columns
   on `warren.vectors` (`algorithm`, `args`, `derived_from`)? A table keeps
   `warren.vectors` unchanged and holds policy; columns would let any
   library result record how it was made.
2. **When updates run.** Synchronous in `promote` first, the compute worker
   later? Or wait for the worker?
3. **Reads of a stale result.** Wait, update on demand, or serve the latest
   valid version?
4. **Guarantee per class.** Bit-for-bit for selective, a residual bound for
   linear, a modularity gap for heuristics: is that the contract?
5. **Companion state budget.** Parent pointers cost 8 bytes per node per
   maintained traversal, and the residual a vector. Charge them to the
   vector storage like any vector, or cap them separately?
6. **Library scope.** Which functions take part in the first release, and
   is `maintain` limited to the library or open to user PL/pgSQL fixpoints
   that declare their semiring?
7. **Drift policy defaults** for the heuristic class.
